//! Agent hooks (design 6.1–6.3). `PreToolUse` rewrites a shell command
//! into `ats run --session <id> --sh '<command>'` unless it should run as
//! it is. Rewriting is local: it reads profiles and the configuration but
//! never contacts the daemon. The answer carries no permission decision,
//! so the agent's own permission checks still apply to every command.
//!
//! `Stop` and `UserPromptSubmit` remind the agent of its session's
//! submissions. When the daemon cannot be reached they let everything
//! through and change nothing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::client;
use crate::config::Config;
use crate::executor::JOB_ENV;
use crate::paths::Paths;
use crate::profile::Profiles;
use crate::recognize::Recognizer;
use crate::store::Reminder;

/// How long a reminder waits for the request-file channel when the socket
/// cannot be reached, so a stopped daemon delays the agent only briefly.
const REMINDER_TIMEOUT: Duration = Duration::from_secs(1);
/// Commands in reminders are cut to this many characters.
const COMMAND_CHARS: usize = 120;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    PreToolUse,
    Stop,
    UserPromptSubmit,
}

/// What the hook tells the agent.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// Run the call as it is.
    Nothing,
    /// Print this JSON.
    Json(Value),
    /// Block the call with this reason (exit code 2), as a chained hook
    /// asked.
    Block(String),
}

/// Everything a rewrite needs besides the hook input.
pub struct Setup {
    pub agent: Agent,
    pub profiles: Profiles,
    pub wrappers: Vec<String>,
    /// Other rewriting hooks, run first in order.
    pub chain: Vec<String>,
    /// This ats binary, which the rewritten command runs.
    pub ats: PathBuf,
    /// The hook runs inside an ats job, whose commands run directly.
    pub in_job: bool,
}

impl Setup {
    pub fn load(paths: &Paths, agent: Agent) -> Result<Setup> {
        let config = Config::load(paths)?;
        let chain = crate::install::Record::load(paths)?.chain(agent);
        Ok(Setup {
            agent,
            profiles: Profiles::load(Some(&paths.profiles))?,
            wrappers: config.recognize.wrappers,
            chain,
            ats: std::env::current_exe().context("locating the ats binary")?,
            in_job: std::env::var_os(JOB_ENV).is_some(),
        })
    }
}

/// Reads the hook input from stdin and answers it.
pub async fn run(paths: &Paths, agent: Agent, event: Event) -> Result<Answer> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let input: Value = serde_json::from_str(&text).context("reading the hook input")?;
    Ok(match event {
        Event::PreToolUse => pre_tool_use(&input, &Setup::load(paths, agent)?),
        Event::Stop => stop(paths, &input).await,
        Event::UserPromptSubmit => {
            user_prompt_submit(paths, dirs::home_dir().as_deref(), agent, &input).await
        }
    })
}

/// The daemon's reminders for this session, or `None` when there is no
/// session or the daemon cannot answer.
async fn reminders(
    paths: &Paths,
    method: &str,
    input: &Value,
    what: &str,
) -> Option<Vec<Reminder>> {
    let session = input["session_id"].as_str()?;
    let path = format!("/v1/sessions/{session}/{what}");
    match client::call_with_timeout(paths, method, &path, None, REMINDER_TIMEOUT).await {
        Ok((answer, _)) if answer.status == 200 => serde_json::from_value(answer.body).ok(),
        Ok((answer, _)) => {
            eprintln!("ats: {path}: daemon answered {}", answer.status);
            None
        }
        Err(e) => {
            eprintln!("ats: {path}: {e:#}");
            None
        }
    }
}

fn line(r: &Reminder) -> String {
    let mut command: String = r.command.chars().take(COMMAND_CHARS).collect();
    if command.len() < r.command.len() {
        command.push('…');
    }
    let code = r
        .exit_code
        .map(|c| format!(", exit code {c}"))
        .unwrap_or_default();
    format!(
        "- {} (job {}) {}{code}: {command}",
        r.submission, r.job, r.state
    )
}

/// Stops the agent once while its session still has submissions running
/// (design 6.2); a second stop goes through.
async fn stop(paths: &Paths, input: &Value) -> Answer {
    if input["stop_hook_active"] == true {
        return Answer::Nothing;
    }
    let Some(running) = reminders(paths, "GET", input, "running")
        .await
        .filter(|r| !r.is_empty())
    else {
        return Answer::Nothing;
    };
    let lines: Vec<String> = running.iter().map(line).collect();
    Answer::Json(json!({
        "decision": "block",
        "reason": format!(
            "ats: submissions from this session are still running. Wait for each with `ats wait <submission>`, or leave it running with `ats detach <submission>`; you are told when it ends:\n{}",
            lines.join("\n")
        ),
    }))
}

/// Tells the agent which of its submissions ended since it last heard, each
/// once (design 6.2), and what keeps its commands from going through ats.
async fn user_prompt_submit(
    paths: &Paths,
    home: Option<&Path>,
    agent: Agent,
    input: &Value,
) -> Answer {
    let mut notes = Vec::new();
    if agent == Agent::Codex && input["permission_mode"] != "bypassPermissions" {
        notes.push("ats: this Codex session asks for approval, so its shell commands run directly instead of through ats.".to_string());
    }
    if let Some(home) = home {
        for command in crate::install::other_rewriters(home, agent) {
            notes.push(format!(
                "ats: the hook `{command}` also rewrites shell commands, next to ats's; ask the user to run `ats install` so ats runs it first."
            ));
        }
    }
    if let Some(ended) = reminders(paths, "POST", input, "ended")
        .await
        .filter(|r| !r.is_empty())
    {
        let lines: Vec<String> = ended.iter().map(line).collect();
        notes.push(format!(
            "ats: submissions from this session ended since your last turn; `ats wait <submission>` shows the result:\n{}",
            lines.join("\n")
        ));
    }
    if notes.is_empty() {
        return Answer::Nothing;
    }
    Answer::Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": notes.join("\n"),
        }
    }))
}

pub fn pre_tool_use(input: &Value, setup: &Setup) -> Answer {
    if input["tool_name"] != "Bash" {
        return Answer::Nothing;
    }
    let submitted = input["tool_input"].clone();
    let mut input = input.clone();
    let mut decision = Map::new();
    for command in &setup.chain {
        let out = match run_chained(command, &input) {
            Ok(Chained::Out(out)) => out,
            Ok(Chained::Silent) => continue,
            Ok(Chained::Block(reason)) => return Answer::Block(reason),
            Err(e) => {
                eprintln!("ats: chained hook `{command}`: {e:#}");
                continue;
            }
        };
        let specific = &out["hookSpecificOutput"];
        if specific["permissionDecision"] == "deny" {
            return Answer::Json(out);
        }
        // Each hook's input replaces the whole object; keeping the fields
        // it left out keeps options such as the timeout.
        if let (Some(updated), Some(current)) = (
            specific["updatedInput"].as_object(),
            input["tool_input"].as_object_mut(),
        ) {
            current.extend(updated.clone());
        }
        for key in ["permissionDecision", "permissionDecisionReason"] {
            if let Some(v) = specific.get(key) {
                decision.insert(key.into(), v.clone());
            }
        }
    }

    let command = input["tool_input"]["command"].as_str().unwrap_or_default();
    let cwd = Path::new(input["cwd"].as_str().unwrap_or("/"));
    let rewritten = rewrite(command, cwd, input["session_id"].as_str(), setup);
    let cd_inside = rewritten.as_ref().is_some_and(|r| r.cd_inside);
    if let Some(r) = rewritten {
        input["tool_input"]["command"] = json!(r.command);
    }
    if input["tool_input"] == submitted && decision.is_empty() {
        return Answer::Nothing;
    }
    let mut specific = Map::new();
    specific.insert("hookEventName".into(), json!("PreToolUse"));
    specific.insert("updatedInput".into(), input["tool_input"].clone());
    specific.extend(decision);
    // Claude Code keeps the directory a command changes to; inside ats it
    // does not change.
    if cd_inside && setup.agent == Agent::Claude {
        specific.insert(
            "additionalContext".into(),
            json!("ats ran this command in its own shell, so its `cd` does not change the directory of later commands."),
        );
    }
    // Codex applies a changed input only with `allow`, which also skips
    // its approval. ats gives it only where Codex asks for no approval;
    // elsewhere the command runs as the agent sent it.
    if setup.agent == Agent::Codex {
        if input["permission_mode"] != "bypassPermissions" {
            return Answer::Nothing;
        }
        specific.insert("permissionDecision".into(), json!("allow"));
    }
    Answer::Json(json!({ "hookSpecificOutput": specific }))
}

enum Chained {
    Out(Value),
    Silent,
    Block(String),
}

/// Runs one chained hook with the current input. Exit code 2 blocks the
/// call; other failures leave the input as it was.
fn run_chained(command: &str, input: &Value) -> Result<Chained> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // A hook may exit without reading its input.
    let _ = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.to_string().as_bytes());
    let out = child.wait_with_output()?;
    match out.status.code() {
        Some(0) => {}
        Some(2) => {
            return Ok(Chained::Block(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        _ => bail!(
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Ok(Chained::Silent);
    }
    Ok(Chained::Out(
        serde_json::from_str(text.trim()).context("reading its output")?,
    ))
}

/// A command rewritten to run through ats.
#[derive(Debug, PartialEq)]
struct Rewritten {
    command: String,
    /// A `cd` inside the part ats runs, which then does not change the
    /// directory of later commands.
    cd_inside: bool,
}

/// The command to run instead, or `None` to run it as it is.
fn rewrite(command: &str, cwd: &Path, session: Option<&str>, setup: &Setup) -> Option<Rewritten> {
    if setup.in_job || command.trim().is_empty() {
        return None;
    }
    // A leading `cd` stays in the agent's shell, which keeps the directory
    // for its later commands as it would without ats.
    if let Some((cd, dir, rest)) = leading_cd(command) {
        let dir = match dir.strip_prefix('~') {
            Some(home) => dirs::home_dir()
                .unwrap_or_default()
                .join(home.trim_start_matches('/')),
            None => cwd.join(dir),
        };
        let inner = rewrite(rest, &dir, session, setup)?;
        return Some(Rewritten {
            command: format!("{cd} {}", inner.command),
            ..inner
        });
    }
    if let Some(ats) = ats_command(command, session, setup) {
        return ats.map(|command| Rewritten {
            command,
            cd_inside: false,
        });
    }
    let analysis = Recognizer::new(&setup.profiles, &setup.wrappers).shell(command, cwd);
    let ats_inside = analysis
        .segments
        .iter()
        .any(|s| s.tool.as_deref() == Some("ats"));
    if ats_inside
        || analysis.light
        || analysis.interactive
        || analysis.segments.iter().any(|s| s.service)
    {
        return None;
    }
    let session = session
        .map(|s| format!(" --session {}", quote(s)))
        .unwrap_or_default();
    Some(Rewritten {
        command: format!(
            "{} run{session} --sh {}",
            quote(&setup.ats.to_string_lossy()),
            quote(command)
        ),
        cd_inside: analysis
            .segments
            .iter()
            .any(|s| matches!(s.tool.as_deref(), Some("cd" | "pushd" | "popd"))),
    })
}

/// `cd <dir> && rest` or `cd <dir>; rest` with a plain directory word:
/// the `cd` with its separator, the directory, and the rest.
fn leading_cd(command: &str) -> Option<(&str, &str, &str)> {
    let start = command.len() - command.trim_start().len();
    let after = command[start..].strip_prefix("cd ")?.trim_start();
    let dir_end = after
        .find(|c: char| c.is_whitespace() || c == ';' || c == '&')
        .unwrap_or(after.len());
    let dir = &after[..dir_end];
    if dir.is_empty()
        || !dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./~-".contains(c))
    {
        return None;
    }
    let tail = after[dir_end..].trim_start();
    let rest = tail
        .strip_prefix("&&")
        .or_else(|| tail.strip_prefix(';'))?
        .trim_start();
    if rest.is_empty() {
        return None;
    }
    let cd_end = command.len() - rest.len();
    Some((command[..cd_end].trim_end(), dir, rest))
}

/// For a line that starts with ats: `ats run` without a session gets one
/// (`Some(Some(..))`); other ats lines run as they are (`Some(None)`).
/// `None` when the line does not start with ats.
fn ats_command(command: &str, session: Option<&str>, setup: &Setup) -> Option<Option<String>> {
    let lead = command.len() - command.trim_start().len();
    let mut words = command[lead..].splitn(3, ' ');
    let program = words.next()?;
    let is_ats = Path::new(program).file_name().is_some_and(|n| n == "ats")
        || Path::new(program) == setup.ats;
    if !is_ats {
        return None;
    }
    let has_session = command
        .split_whitespace()
        .any(|w| w == "--session" || w.starts_with("--session="));
    let (Some("run"), Some(session), false) = (words.next(), session, has_session) else {
        return Some(None);
    };
    let at = lead + program.len() + " run".len();
    Some(Some(format!(
        "{} --session {}{}",
        &command[..at],
        quote(session),
        &command[at..]
    )))
}

/// POSIX shell quoting: plain words as they are, anything else in single
/// quotes.
pub fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(chain: &[&str]) -> Setup {
        Setup {
            agent: Agent::Claude,
            profiles: Profiles::builtin(),
            wrappers: vec!["wrap".into()],
            chain: chain.iter().map(|c| c.to_string()).collect(),
            ats: "/opt/ats".into(),
            in_job: false,
        }
    }

    /// A PreToolUse input shaped like the ones Claude Code and Codex send.
    fn input(command: &str, cwd: &Path) -> Value {
        json!({
            "session_id": "s1",
            "cwd": cwd,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "d", "timeout": 600000 },
            "tool_use_id": "toolu_1",
        })
    }

    fn command(answer: &Answer) -> Option<&str> {
        match answer {
            Answer::Json(out) => out["hookSpecificOutput"]["updatedInput"]["command"].as_str(),
            _ => None,
        }
    }

    #[test]
    fn rewrites_heavy_commands_and_passes_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let s = setup(&[]);
        for (src, want) in [
            (
                "cargo build",
                Some("/opt/ats run --session s1 --sh 'cargo build'"),
            ),
            (
                "./build.sh && make test",
                Some("/opt/ats run --session s1 --sh './build.sh && make test'"),
            ),
            (
                "echo it's > out.txt",
                Some(r"/opt/ats run --session s1 --sh 'echo it'\''s > out.txt'"),
            ),
            ("ls -la", None),
            ("git rev-parse HEAD && cat Cargo.toml", None),
            (
                "git status",
                Some("/opt/ats run --session s1 --sh 'git status'"),
            ),
            ("cd sub", None),
            (
                "cd sub && cargo build",
                Some("cd sub && /opt/ats run --session s1 --sh 'cargo build'"),
            ),
            ("cd sub; ls", None),
            (
                "make && cd out",
                Some("/opt/ats run --session s1 --sh 'make && cd out'"),
            ),
            ("vim notes.md", None),
            ("python3", None),
            ("", None),
            ("ats status", None),
            ("ats run --session other -- make", None),
            ("ats run -- make", Some("ats run --session s1 -- make")),
            (
                "/opt/ats run --sh 'make'",
                Some("/opt/ats run --session s1 --sh 'make'"),
            ),
            ("make && ats wait s_1", None),
        ] {
            let answer = pre_tool_use(&input(src, dir.path()), &s);
            assert_eq!(command(&answer), want, "{src:?}");
        }
    }

    #[test]
    fn keeps_the_other_fields_and_asks_for_no_permission() {
        let dir = tempfile::tempdir().unwrap();
        let Answer::Json(out) = pre_tool_use(&input("cargo test", dir.path()), &setup(&[])) else {
            panic!("not rewritten");
        };
        let specific = &out["hookSpecificOutput"];
        assert_eq!(specific["updatedInput"]["timeout"], 600000);
        assert_eq!(specific["updatedInput"]["description"], "d");
        assert!(specific.get("permissionDecision").is_none());
    }

    #[test]
    fn runs_directly_inside_a_job_and_ignores_other_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = setup(&[]);
        s.in_job = true;
        assert_eq!(
            pre_tool_use(&input("cargo build", dir.path()), &s),
            Answer::Nothing
        );
        let mut read = input("cargo build", dir.path());
        read["tool_name"] = json!("Read");
        assert_eq!(pre_tool_use(&read, &setup(&[])), Answer::Nothing);
    }

    #[test]
    fn chained_hooks_run_first() {
        let dir = tempfile::tempdir().unwrap();
        let rewrite_to = |to: &str| {
            format!(
                r#"cat >/dev/null; echo '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecisionReason":"r","updatedInput":{{"command":"{to}"}}}}}}'"#
            )
        };
        // Its rewrite is wrapped, keeping the fields it left out.
        let s = setup(&[&rewrite_to("wrap cargo build")]);
        let answer = pre_tool_use(&input("cargo build", dir.path()), &s);
        assert_eq!(
            command(&answer),
            Some("/opt/ats run --session s1 --sh 'wrap cargo build'")
        );
        let Answer::Json(out) = answer else {
            unreachable!()
        };
        assert_eq!(out["hookSpecificOutput"]["updatedInput"]["timeout"], 600000);
        assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], "r");
        // A light command stays light behind a configured wrapper; the
        // chained rewrite is still returned.
        let s = setup(&[&rewrite_to("wrap git rev-parse HEAD")]);
        assert_eq!(
            command(&pre_tool_use(&input("git rev-parse HEAD", dir.path()), &s)),
            Some("wrap git rev-parse HEAD")
        );
        // Silent and failing hooks change nothing.
        let s = setup(&["cat >/dev/null", "exit 1"]);
        assert_eq!(pre_tool_use(&input("ls", dir.path()), &s), Answer::Nothing);
        // A denial or a block is passed on as it is.
        let deny = r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}'"#;
        let Answer::Json(out) = pre_tool_use(&input("cargo build", dir.path()), &setup(&[deny]))
        else {
            panic!("not denied");
        };
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(out["hookSpecificOutput"].get("updatedInput").is_none());
        let block = "echo no >&2; exit 2";
        assert_eq!(
            pre_tool_use(&input("cargo build", dir.path()), &setup(&[block])),
            Answer::Block("no\n".into())
        );
    }

    #[test]
    fn codex_gets_allow_only_where_it_asks_for_no_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = setup(&[]);
        s.agent = Agent::Codex;
        let mut call = input("cargo build", dir.path());
        call["permission_mode"] = json!("bypassPermissions");
        let Answer::Json(out) = pre_tool_use(&call, &s) else {
            panic!("not rewritten");
        };
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
        call["permission_mode"] = json!("default");
        assert_eq!(
            pre_tool_use(&call, &s),
            Answer::Nothing,
            "the agent's approval stays"
        );
    }

    #[test]
    fn a_leading_cd_stays_with_the_agent() {
        assert_eq!(
            leading_cd("cd sub && make"),
            Some(("cd sub &&", "sub", "make"))
        );
        assert_eq!(
            leading_cd("  cd ~/w/x;make -j"),
            Some(("  cd ~/w/x;", "~/w/x", "make -j"))
        );
        for other in [
            "cd \"a b\" && make",
            "cd $DIR && make",
            "cd sub",
            "cd sub &&",
            "make && cd sub",
        ] {
            assert_eq!(leading_cd(other), None, "{other}");
        }
        // Claude Code is told when a `cd` ran inside ats.
        let dir = tempfile::tempdir().unwrap();
        let note = |src: &str| match pre_tool_use(&input(src, dir.path()), &setup(&[])) {
            Answer::Json(out) => out["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .map(str::to_string),
            _ => None,
        };
        assert_eq!(note("cd sub && cargo build"), None);
        assert!(note("make && cd out").is_some_and(|n| n.contains("`cd`")));
    }

    #[tokio::test]
    async fn turn_notes_for_approval_and_other_rewriters() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with(dir.path().join("ats"), dir.path().join("requests"));
        let home = dir.path().join("user");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/hooks.json"),
            r#"{"hooks":{"PreToolUse":[
                {"matcher":"Bash","hooks":[{"type":"command","command":"/opt/ats hook codex pre-tool-use"}]},
                {"matcher":"Bash","hooks":[{"type":"command","command":"wrap hook"}]}]}}"#,
        )
        .unwrap();
        let context = |answer: Answer| match answer {
            Answer::Json(out) => out["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        let asking = json!({"session_id": "s", "permission_mode": "default"});
        let text = context(user_prompt_submit(&paths, Some(&home), Agent::Codex, &asking).await);
        assert!(
            text.contains("asks for approval") && text.contains("`wrap hook`"),
            "{text}"
        );
        let open = json!({"session_id": "s", "permission_mode": "bypassPermissions"});
        let text = context(user_prompt_submit(&paths, Some(&home), Agent::Codex, &open).await);
        assert!(
            !text.contains("approval") && text.contains("`wrap hook`"),
            "{text}"
        );
        let empty = dir.path().join("nobody");
        assert_eq!(
            user_prompt_submit(&paths, Some(&empty), Agent::Claude, &open).await,
            Answer::Nothing
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("s-1_a.b"), "s-1_a.b");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
