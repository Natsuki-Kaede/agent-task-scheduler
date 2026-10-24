//! `ats install` and `ats uninstall` (design 5.3, 6.1, 12): the agents' hook
//! configuration, an instruction block in their global instructions, and
//! the LaunchAgent that keeps the daemon running.
//! Every change is shown as a diff and written only after confirmation,
//! with a backup of each file first. The hooks moved into ats's chain are
//! kept as they were in `installed.json`, so uninstall puts them back.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::{Agent, quote};
use crate::paths::Paths;

/// The LaunchAgent's label and file name.
const LABEL: &str = "io.github.natsuki-kaede.ats";
const BEGIN: &str = "<!-- ats:begin -->";
const END: &str = "<!-- ats:end -->";
const INSTRUCTIONS: &str = "\
## Agent Task Scheduler (ats)

Shell commands with real load run through `ats`, which starts them when the machine has memory for them, so parallel sessions do not slow each other down. A hook rewrites them into `ats run`; you do not need to call it yourself.

- When the command finishes within 90 seconds, its output and exit code come back unchanged.
- Exit code 75 with an `[ats] submission ...` line: the command is still running in the background. Get the result with `ats wait <submission>` (up to 90 seconds per call), follow its output with `ats logs <submission> -f`, or leave it running with `ats detach <submission>`; you are told at your next turn when it ends.
- Exit code 76: a predecessor given with `--after` failed. 77: ats rejected the submission. 78: ats itself failed.
- `ats why <submission>` explains a wait. Use `ats run --now` only when the user asks to run something immediately.";

/// Hook events ats answers: the event, its matcher, and the `ats hook`
/// event name.
const EVENTS: [(&str, Option<&str>, &str); 3] = [
    ("PreToolUse", Some("Bash"), "pre-tool-use"),
    ("Stop", None, "stop"),
    ("UserPromptSubmit", None, "user-prompt-submit"),
];

/// A hook moved into ats's chain, as it was and where it was.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Moved {
    pub group: usize,
    pub index: usize,
    pub hook: Value,
}

/// What `ats install` moved, per agent.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct Record {
    #[serde(default)]
    pub agents: BTreeMap<String, Vec<Moved>>,
}

impl Record {
    pub fn load(paths: &Paths) -> Result<Record> {
        match std::fs::read_to_string(&paths.installed) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", paths.installed.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.installed.display())),
        }
    }

    /// The commands of the hooks ats runs before its own rewrite.
    pub fn chain(&self, agent: Agent) -> Vec<String> {
        self.agents
            .get(name(agent))
            .into_iter()
            .flatten()
            .filter_map(|m| m.hook["command"].as_str().map(str::to_string))
            .collect()
    }
}

fn name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}

/// An agent's hook configuration and global instructions.
fn files(home: &Path, agent: Agent) -> (PathBuf, PathBuf) {
    match agent {
        Agent::Claude => (
            home.join(".claude/settings.json"),
            home.join(".claude/CLAUDE.md"),
        ),
        Agent::Codex => (
            home.join(".codex/hooks.json"),
            home.join(".codex/AGENTS.md"),
        ),
    }
}

/// The agents named, or else those set up on this machine.
pub fn agents(home: &Path, named: &[Agent]) -> Vec<Agent> {
    if !named.is_empty() {
        return named.to_vec();
    }
    [Agent::Claude, Agent::Codex]
        .into_iter()
        .filter(|a| files(home, *a).0.parent().is_some_and(Path::is_dir))
        .collect()
}

fn is_ats(hook: &Value, agent: Agent) -> bool {
    let command = hook["command"].as_str().unwrap_or_default();
    command.contains("ats")
        && EVENTS
            .iter()
            .any(|(_, _, e)| command.ends_with(&format!(" hook {} {e}", name(agent))))
}

fn groups<'a>(doc: &'a mut Value, event: &str) -> Option<&'a mut Vec<Value>> {
    doc.get_mut("hooks")?.get_mut(event)?.as_array_mut()
}

/// Drops empty groups, events and the `hooks` object itself.
fn tidy(doc: &mut Value) {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty() {
        doc.as_object_mut().expect("an object").remove("hooks");
    }
}

fn remove_ats(doc: &mut Value, agent: Agent) {
    for (event, _, _) in EVENTS {
        for group in groups(doc, event).into_iter().flatten() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.retain(|h| !is_ats(h, agent));
            }
        }
    }
    tidy(doc);
}

fn add_ats(doc: &mut Value, agent: Agent, ats: &Path) {
    let hooks = doc
        .as_object_mut()
        .expect("an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    for (event, matcher, hook_event) in EVENTS {
        let command = format!(
            "{} hook {} {hook_event}",
            quote(&ats.to_string_lossy()),
            name(agent)
        );
        let mut group = json!({ "hooks": [{ "type": "command", "command": command }] });
        if let Some(m) = matcher {
            group = json!({ "matcher": m, "hooks": group["hooks"] });
        }
        let list = hooks
            .as_object_mut()
            .expect("an object")
            .entry(event)
            .or_insert_with(|| json!([]));
        list.as_array_mut().expect("an array").push(group);
    }
}

/// Other hooks that only see shell commands before they run: the ones
/// that may rewrite them.
fn candidates(doc: &Value) -> Vec<Moved> {
    let mut out = Vec::new();
    for (g, group) in doc["hooks"]["PreToolUse"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if group["matcher"] != "Bash" {
            continue;
        }
        for (i, hook) in group["hooks"].as_array().into_iter().flatten().enumerate() {
            out.push(Moved {
                group: g,
                index: i,
                hook: hook.clone(),
            });
        }
    }
    out
}

fn move_out(doc: &mut Value, moved: &[Moved]) {
    let mut order: Vec<&Moved> = moved.iter().collect();
    order.sort_by_key(|m| std::cmp::Reverse((m.group, m.index)));
    for m in order {
        if let Some(hooks) = groups(doc, "PreToolUse")
            .and_then(|g| g.get_mut(m.group))
            .and_then(|g| g["hooks"].as_array_mut())
        {
            hooks.remove(m.index);
        }
    }
    tidy(doc);
}

/// Puts moved hooks back where they were: into the `Bash` group at their
/// position when it is still there, else into a new group at its place.
fn restore(doc: &mut Value, moved: &[Moved]) {
    let mut order: Vec<&Moved> = moved.iter().collect();
    order.sort_by_key(|m| (m.group, m.index));
    for m in order {
        let hooks = doc
            .as_object_mut()
            .expect("an object")
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let groups = hooks
            .as_object_mut()
            .expect("an object")
            .entry("PreToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("an array");
        match groups.get_mut(m.group).filter(|g| g["matcher"] == "Bash") {
            Some(group) => {
                let list = group["hooks"].as_array_mut().expect("an array");
                list.insert(m.index.min(list.len()), m.hook.clone());
            }
            None => groups.insert(
                m.group.min(groups.len()),
                json!({ "matcher": "Bash", "hooks": [m.hook] }),
            ),
        }
    }
}

/// `text` without ats's instruction block.
fn without_block(text: &str) -> String {
    match (text.find(BEGIN), text.find(END)) {
        (Some(b), Some(e)) if b < e => {
            let head = text[..b].trim_end_matches('\n');
            let tail = text[e + END.len()..].trim_start_matches('\n');
            match (head.is_empty(), tail.is_empty()) {
                (true, _) => tail.to_string(),
                (false, true) => format!("{head}\n"),
                (false, false) => format!("{head}\n\n{tail}"),
            }
        }
        _ => text.to_string(),
    }
}

fn with_block(text: &str) -> String {
    let base = without_block(text);
    let block = format!("{BEGIN}\n{INSTRUCTIONS}\n{END}\n");
    if base.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{block}", base.trim_end_matches('\n'))
    }
}

fn launch_agent(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Starts `ats daemon` at login and again if it fails. A data directory
/// chosen with `ATS_HOME` is passed on.
fn plist(paths: &Paths, ats: &Path) -> String {
    let env = match std::env::var_os(crate::paths::HOME_ENV) {
        Some(_) => format!(
            "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>{}</key>\n    <string>{}</string>\n  </dict>\n",
            crate::paths::HOME_ENV,
            xml(&paths.home.to_string_lossy())
        ),
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>daemon</string>
  </array>
{env}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        xml(&ats.to_string_lossy()),
        xml(&paths.home.join("daemon.log").to_string_lossy())
    )
}

/// Loads or unloads the LaunchAgent in this user's session.
fn launchctl(home: &Path, load: bool, out: &mut impl Write) -> Result<()> {
    // SAFETY: getuid cannot fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    // Unloading first lets a reinstall pick up a changed file; it fails
    // harmlessly when nothing is loaded.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .stderr(std::process::Stdio::null())
        .status();
    if load {
        let status = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(launch_agent(home))
            .status()
            .context("running launchctl")?;
        if !status.success() {
            bail!("launchctl bootstrap failed: {status}");
        }
        writeln!(out, "The daemon now runs at login: {LABEL}")?;
    }
    Ok(())
}

/// Hooks besides ats's that rewrite shell commands for `agent`: ones added
/// after `ats install` moved the others into its chain.
pub fn other_rewriters(home: &Path, agent: Agent) -> Vec<String> {
    let path = files(home, agent).0;
    let Some(doc) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let has_ats = candidates(&doc).iter().any(|c| is_ats(&c.hook, agent));
    if !has_ats {
        return Vec::new();
    }
    candidates(&doc)
        .into_iter()
        .filter(|c| !is_ats(&c.hook, agent))
        .filter_map(|c| c.hook["command"].as_str().map(str::to_string))
        .collect()
}

/// Claude Code checks permission rules against the rewritten command, so
/// rules that allow particular shell commands stop matching them. Modes
/// that ask for no approval are not affected.
fn shell_allow_rules(doc: &Value) -> Vec<String> {
    let mode = doc["permissions"]["defaultMode"]
        .as_str()
        .unwrap_or("default");
    if matches!(mode, "auto" | "bypassPermissions") {
        return Vec::new();
    }
    doc["permissions"]["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|r| r.starts_with("Bash(") && !r.starts_with("Bash(ats"))
        .map(str::to_string)
        .collect()
}

/// One file to write; `new` is `None` to remove the file.
struct Change {
    path: PathBuf,
    old: Option<String>,
    new: Option<String>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn parse(path: &Path, text: Option<&str>) -> Result<Value> {
    let doc: Value = match text {
        Some(t) if !t.trim().is_empty() => serde_json::from_str(t)
            .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?,
        _ => json!({}),
    };
    if !doc.is_object() {
        bail!("{} does not hold a JSON object", path.display());
    }
    Ok(doc)
}

fn json_text(doc: &Value) -> String {
    serde_json::to_string_pretty(doc).expect("a JSON value") + "\n"
}

fn ask(input: &mut impl BufRead, out: &mut impl Write, question: &str) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// `launchd` loads the LaunchAgent once it is written.
pub fn install(
    paths: &Paths,
    home: &Path,
    agents: &[Agent],
    ats: &Path,
    launchd: bool,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let mut record = Record::load(paths)?;
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    for &agent in agents {
        let (hooks_file, instructions) = files(home, agent);
        let old = read(&hooks_file)?;
        let mut doc = parse(&hooks_file, old.as_deref())?;
        remove_ats(&mut doc, agent);
        let mut chosen = Vec::new();
        for c in candidates(&doc) {
            let command = c.hook["command"].as_str().unwrap_or("?");
            let question = format!(
                "{} also runs `{command}` on every shell command. If it rewrites commands, ats runs it first and wraps its result. Move it into ats's hook?",
                hooks_file.display()
            );
            if ask(input, out, &question)? {
                chosen.push(c);
            }
        }
        move_out(&mut doc, &chosen);
        add_ats(&mut doc, agent, ats);
        if agent == Agent::Claude {
            notes.extend(shell_allow_rules(&doc).into_iter().map(|r| format!(
                "Claude Code checks permission rules against the rewritten command `ats run ... --sh '...'`, so the rule {r} no longer matches what it allowed; those commands will ask for permission."
            )));
        }
        changes.push(Change {
            path: hooks_file,
            new: Some(json_text(&doc)),
            old,
        });
        let old = read(&instructions)?;
        changes.push(Change {
            path: instructions,
            new: Some(with_block(old.as_deref().unwrap_or_default())),
            old,
        });
        record
            .agents
            .entry(name(agent).into())
            .or_default()
            .extend(chosen);
    }
    let service = launch_agent(home);
    changes.push(Change {
        old: read(&service)?,
        new: Some(plist(paths, ats)),
        path: service,
    });
    if agents.contains(&Agent::Codex) {
        notes.push("Codex queues commands through ats only in sessions that need no approval (approval policy \"never\", such as full access); in other sessions they run directly. Codex asks you to trust the changed hooks the next time it starts.".into());
    }
    // Shown with the diffs, before the question.
    for note in &notes {
        writeln!(out, "Note: {note}")?;
    }
    if apply(paths, changes, &record, input, out)? && launchd {
        launchctl(home, true, out)?;
    }
    Ok(())
}

/// With `all`, the LaunchAgent goes too; `launchd` unloads it.
pub fn uninstall(
    paths: &Paths,
    home: &Path,
    agents: &[Agent],
    all: bool,
    launchd: bool,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let mut record = Record::load(paths)?;
    let mut changes = Vec::new();
    for &agent in agents {
        let (hooks_file, instructions) = files(home, agent);
        if let Some(old) = read(&hooks_file)? {
            let mut doc = parse(&hooks_file, Some(&old))?;
            remove_ats(&mut doc, agent);
            restore(
                &mut doc,
                &record.agents.remove(name(agent)).unwrap_or_default(),
            );
            let new = (doc != json!({})).then(|| json_text(&doc));
            changes.push(Change {
                path: hooks_file,
                new,
                old: Some(old),
            });
        }
        if let Some(old) = read(&instructions)? {
            let new = Some(without_block(&old)).filter(|t| !t.trim().is_empty());
            changes.push(Change {
                path: instructions,
                new,
                old: Some(old),
            });
        }
    }
    let service = launch_agent(home);
    if all && let Some(old) = read(&service)? {
        changes.push(Change {
            path: service,
            old: Some(old),
            new: None,
        });
    }
    if apply(paths, changes, &record, input, out)? && all && launchd {
        launchctl(home, false, out)?;
    }
    Ok(())
}

/// Shows the changes, asks once, then backs up and writes each file and
/// the record. Returns whether anything was written.
fn apply(
    paths: &Paths,
    changes: Vec<Change>,
    record: &Record,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<bool> {
    let changes: Vec<Change> = changes.into_iter().filter(|c| c.old != c.new).collect();
    if changes.is_empty() {
        writeln!(out, "Nothing to change.")?;
        return Ok(false);
    }
    for c in &changes {
        out.write_all(
            diff(
                &c.path,
                c.old.as_deref().unwrap_or_default(),
                c.new.as_deref().unwrap_or_default(),
            )?
            .as_bytes(),
        )?;
        if c.new.is_none() {
            writeln!(out, "{} will be removed.", c.path.display())?;
        }
    }
    if !ask(input, out, "Write these changes?")? {
        writeln!(out, "Nothing written.")?;
        return Ok(false);
    }
    let backup = paths
        .home
        .join("backups")
        .join(humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string());
    for (i, c) in changes.iter().enumerate() {
        if let Some(old) = &c.old {
            std::fs::create_dir_all(&backup)?;
            let name = c.path.file_name().expect("a file name").to_string_lossy();
            std::fs::write(backup.join(format!("{i}-{name}")), old)?;
        }
        match &c.new {
            Some(text) => write_atomic(&c.path, text)?,
            None => std::fs::remove_file(&c.path)?,
        }
    }
    std::fs::create_dir_all(&paths.home)?;
    write_atomic(
        &paths.installed,
        &(serde_json::to_string_pretty(record)? + "\n"),
    )?;
    if backup.exists() {
        writeln!(
            out,
            "Written. The previous files are in {}",
            backup.display()
        )?;
    } else {
        writeln!(out, "Written.")?;
    }
    Ok(true)
}

/// Replaces `path` through a temporary file, keeping its permissions.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().expect("a parent directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.ats-tmp",
        path.file_name().expect("a file name").to_string_lossy()
    ));
    std::fs::write(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// A unified diff from the system's `diff`.
fn diff(path: &Path, old: &str, new: &str) -> Result<String> {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ats-install-{}-{call}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let (a, b) = (dir.join("old"), dir.join("new"));
    std::fs::write(&a, old)?;
    std::fs::write(&b, new)?;
    let label = path.display().to_string();
    let output = std::process::Command::new("diff")
        .args(["-u", "--label", &label, "--label", &label])
        .arg(&a)
        .arg(&b)
        .output()
        .context("running diff")?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r#"{
  "$schema": "https://example.com/settings.json",
  "hooks": {
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "observe post" }] }
    ],
    "PreToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "observe pre" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "wrap hook claude", "timeout": 5 }] }
    ]
  },
  "theme": "dark"
}
"#;

    fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("user");
        let paths = Paths::with(dir.path().join("ats"), dir.path().join("requests"));
        (dir, paths, home)
    }

    fn run(f: impl FnOnce(&mut &[u8], &mut Vec<u8>) -> Result<()>, answers: &str) -> String {
        let mut out = Vec::new();
        f(&mut answers.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn installs_and_uninstalls_without_a_trace() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = home.join(".claude/settings.json");
        let instructions = home.join(".claude/CLAUDE.md");
        // Agents keep this file indented by two spaces.
        let original = json_text(&serde_json::from_str(SETTINGS).unwrap());
        std::fs::write(&settings, &original).unwrap();
        std::fs::write(&instructions, "# Mine\n\nKeep this.\n").unwrap();
        let ats = Path::new("/opt/ats");
        let agents = agents(&home, &[]);
        assert_eq!(agents, [Agent::Claude], "only the agent set up here");

        // Move the rewriting hook, then confirm.
        let shown = run(
            |i, o| install(&paths, &home, &agents, ats, false, i, o),
            "y\ny\n",
        );
        assert!(shown.contains("+    \"Stop\": ["), "{shown}");
        let service = launch_agent(&home);
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&service)
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
        assert!(
            std::fs::read_to_string(&service)
                .unwrap()
                .contains("<string>/opt/ats</string>")
        );
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "hooks", "theme"], "key order kept");
        let pre = doc["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(
            pre[0]["hooks"][0]["command"], "observe pre",
            "hooks for every tool stay"
        );
        assert_eq!(
            pre[1]["hooks"][0]["command"],
            "/opt/ats hook claude pre-tool-use"
        );
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/opt/ats hook claude stop"
        );
        assert_eq!(
            Record::load(&paths).unwrap().chain(Agent::Claude),
            ["wrap hook claude"]
        );
        let text = std::fs::read_to_string(&instructions).unwrap();
        assert!(
            text.starts_with("# Mine\n\nKeep this.\n\n<!-- ats:begin -->")
                && text.ends_with("<!-- ats:end -->\n")
        );
        assert!(
            paths
                .home
                .join("backups")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );

        // Installing again changes nothing.
        assert!(
            run(|i, o| install(&paths, &home, &agents, ats, false, i, o), "")
                .contains("Nothing to change.")
        );

        run(
            |i, o| uninstall(&paths, &home, &agents, true, false, i, o),
            "y\n",
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            original,
            "byte for byte"
        );
        assert!(
            !launch_agent(&home).exists(),
            "the LaunchAgent goes with a full uninstall"
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Mine\n\nKeep this.\n"
        );
        assert!(
            Record::load(&paths)
                .unwrap()
                .chain(Agent::Claude)
                .is_empty()
        );
    }

    #[test]
    fn names_shell_allow_rules_that_stop_matching() {
        let doc = |mode: &str| json!({"permissions": {"defaultMode": mode, "allow": ["Bash(cargo build:*)", "Bash(ats status)", "Read"]}});
        assert_eq!(shell_allow_rules(&doc("default")), ["Bash(cargo build:*)"]);
        assert!(shell_allow_rules(&doc("auto")).is_empty());
        assert!(shell_allow_rules(&json!({})).is_empty());
    }

    #[test]
    fn declining_writes_nothing() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), SETTINGS).unwrap();
        let shown = run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &[Agent::Claude],
                    Path::new("/opt/ats"),
                    false,
                    i,
                    o,
                )
            },
            "n\nn\n",
        );
        assert!(shown.ends_with("Nothing written.\n"), "{shown}");
        assert_eq!(
            std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
            SETTINGS
        );
        assert!(!home.join(".claude/CLAUDE.md").exists() && !paths.installed.exists());
    }

    #[test]
    fn files_it_created_are_removed_again() {
        let (_dir, paths, home) = setup();
        let ats = Path::new("/Applications/My Tools/ats");
        run(
            |i, o| install(&paths, &home, &[Agent::Codex], ats, false, i, o),
            "y\n",
        );
        let hooks = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
        assert!(
            hooks.contains(r"'/Applications/My Tools/ats' hook codex pre-tool-use"),
            "{hooks}"
        );
        assert!(home.join(".codex/AGENTS.md").exists());
        run(
            |i, o| uninstall(&paths, &home, &[Agent::Codex], true, false, i, o),
            "y\n",
        );
        assert!(
            !home.join(".codex/hooks.json").exists() && !home.join(".codex/AGENTS.md").exists()
        );
    }

    #[test]
    fn refuses_a_file_it_cannot_read() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{ not json").unwrap();
        let mut out = Vec::new();
        let err = install(
            &paths,
            &home,
            &[Agent::Claude],
            Path::new("/opt/ats"),
            false,
            &mut "y\n".as_bytes(),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not valid JSON"), "{err:#}");
    }
}
