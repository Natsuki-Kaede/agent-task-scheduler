//! `ats install` and `ats uninstall` (design 5.3, 6.1, 12): the
//! instructions that tell agents which commands to send to `ats run`, the
//! reminder hooks, and the login service that keeps the daemon running.
//! Every change is shown as a diff and written only after confirmation,
//! with a backup of each file first. Earlier versions moved other hooks
//! into a chain of their own and kept them in `installed.json`; install and
//! uninstall put them back.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::{Agent, quote};
use crate::paths::Paths;

mod service;

/// How install and uninstall confirm their changes.
pub enum Confirm<'a> {
    /// Ask on the terminal.
    Ask(&'a mut dyn BufRead),
    /// Show the changes and write nothing.
    DryRun,
    /// Write without a question: the user agreed to the changes shown
    /// before, as an agent shows them.
    Yes,
}

const BEGIN: &str = "<!-- ats:begin -->";
const END: &str = "<!-- ats:end -->";
/// The line in CLAUDE.md that imports the instructions file.
const INCLUDE: &str = "@ats.md";
const INSTRUCTIONS: &str = "\
## Agent Task Scheduler (ats)

ats is a local queue for heavy shell commands. It starts each command when the machine has memory for it, so parallel agent sessions do not slow each other down.

Send a command through ats when it can use much CPU, memory, disk or network, or when it can run for more than about 30 seconds:

- builds, test suites, packaging and large lint runs: `cargo build`, `cargo test`, `npm run build`, `make`, `xcodebuild`, `gradle`, `pytest`
- dependency installs, large downloads and container builds: `npm install`, `pip install`, `docker build`
- model training or inference, data processing, and conversion or compression of large files

Write it as `ats run -- <program> <args>` or `ats run --sh '<shell command>'`. Put a `cd` before `ats run`, not in it: `cd sub && ats run -- make`.

Run other commands directly: `ls`, `cat`, `cd`, `mkdir`, `cp`, `mv`, `git status`, `git diff`, small `git` operations, `grep` or `rg` in a small tree, `ping`, version and help output, editors and other interactive commands, and servers that do not stop.

- When the command ends in 90 seconds or less, ats gives its output and exit code unchanged.
- Exit code 75 and an `[ats] submission ...` line: the command continues in the background. Run `ats wait <submission>` to get its output and exit code. Each call waits for up to 90 seconds. If you do not need the result now, run `ats detach <submission>`. You are told at your next turn when it ends.
- Exit code 76: a predecessor given with `--after` failed. 77: ats did not accept the submission. 78: ats failed.
- Run `ats why <submission>` to see why a command waits. Do not use `ats run --now` unless the user asks to run something immediately.";

/// Hook events ats answers, with their `ats hook` names.
const EVENTS: [(&str, &str); 2] = [("Stop", "stop"), ("UserPromptSubmit", "user-prompt-submit")];
/// `ats hook` names of earlier versions, removed on install and uninstall.
const OLD_EVENTS: [(&str, &str); 1] = [("PreToolUse", "pre-tool-use")];

/// A hook an earlier `ats install` moved into its chain, as it was and
/// where it was.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Moved {
    pub group: usize,
    pub index: usize,
    pub hook: Value,
}

/// What earlier versions of `ats install` moved, per agent.
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

/// The instructions file CLAUDE.md imports. Codex does not expand imports
/// in AGENTS.md, so its instructions are written in place.
fn include_file(home: &Path) -> PathBuf {
    home.join(".claude/ats.md")
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
            .chain(&OLD_EVENTS)
            .any(|(_, e)| command.ends_with(&format!(" hook {} {e}", name(agent))))
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
    for (event, _) in EVENTS.iter().chain(&OLD_EVENTS) {
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
    for (event, hook_event) in EVENTS {
        let command = format!(
            "{} hook {} {hook_event}",
            quote(&ats.to_string_lossy()),
            name(agent)
        );
        let list = hooks
            .as_object_mut()
            .expect("an object")
            .entry(event)
            .or_insert_with(|| json!([]));
        list.as_array_mut()
            .expect("an array")
            .push(json!({ "hooks": [{ "type": "command", "command": command }] }));
    }
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

/// `text` with ats's instruction block at the end.
fn with_block(text: &str) -> String {
    let base = without_block(text);
    let block = instructions_block();
    if base.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{block}", base.trim_end_matches('\n'))
    }
}

/// The instructions between markers, for an agent to add to its own
/// instructions file and to find again for removal (`ats instructions`).
pub fn instructions_block() -> String {
    format!("{BEGIN}\n{INSTRUCTIONS}\n{END}\n")
}

/// `text` without the import line, or a block an earlier version wrote.
fn without_include(text: &str) -> String {
    let base = without_block(text);
    let line = format!("{INCLUDE}\n");
    if base == line {
        return String::new();
    }
    for added in [format!("\n\n{line}"), format!("\n{line}")] {
        if let Some(i) = base.find(&added) {
            return format!("{}\n{}", &base[..i], &base[i + added.len()..]);
        }
    }
    base
}

/// `text` with the import line at the end.
fn with_include(text: &str) -> String {
    let base = without_include(text);
    if base.trim().is_empty() {
        format!("{INCLUDE}\n")
    } else {
        format!("{}\n\n{INCLUDE}\n", base.trim_end_matches('\n'))
    }
}

/// In Claude Code modes that ask before shell commands, each `ats run`
/// asks too. An allow rule for `ats run` would allow any command.
fn claude_asks(doc: &Value) -> bool {
    !matches!(
        doc["permissions"]["defaultMode"].as_str(),
        Some("auto" | "bypassPermissions")
    )
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

fn ask(input: &mut dyn BufRead, out: &mut impl Write, question: &str) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// With `start`, the login service is loaded once it is written.
pub fn install(
    paths: &Paths,
    home: &Path,
    agents: &[Agent],
    ats: &Path,
    start: bool,
    confirm: Confirm,
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
        let moved = record.agents.remove(name(agent)).unwrap_or_default();
        if !moved.is_empty() {
            notes.push(format!(
                "An earlier ats install moved these hooks of {}; they go back: {}",
                hooks_file.display(),
                moved
                    .iter()
                    .filter_map(|m| m.hook["command"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        restore(&mut doc, &moved);
        add_ats(&mut doc, agent, ats);
        if agent == Agent::Claude && claude_asks(&doc) {
            notes.push("Claude Code asks before each `ats run` command in this permission mode. Do not allow `Bash(ats run:*)`: it would allow any command.".into());
        }
        changes.push(Change {
            path: hooks_file,
            new: Some(json_text(&doc)),
            old,
        });
        let old = read(&instructions)?;
        let new = match agent {
            Agent::Claude => {
                let file = include_file(home);
                changes.push(Change {
                    old: read(&file)?,
                    new: Some(format!("{INSTRUCTIONS}\n")),
                    path: file,
                });
                with_include(old.as_deref().unwrap_or_default())
            }
            Agent::Codex => with_block(old.as_deref().unwrap_or_default()),
        };
        changes.push(Change {
            path: instructions,
            new: Some(new),
            old,
        });
    }
    let service = service::file(home);
    let (old, new) = (read(&service)?, service::text(paths, ats));
    let changed = old.as_deref() != Some(new.as_str());
    changes.push(Change {
        old,
        new: Some(new),
        path: service,
    });
    if agents.contains(&Agent::Codex) {
        notes.push("Codex asks you to trust the changed hooks the next time it starts.".into());
    }
    // Shown with the diffs, before the question.
    for note in &notes {
        writeln!(out, "Note: {note}")?;
    }
    if let Some(changes) = review(changes, confirm, out)? {
        write(paths, &changes, &record, out)?;
        if start {
            service::load(home, changed, out)?;
        }
    }
    Ok(())
}

/// With `all`, the login service goes too; with `start`, it is unloaded
/// first.
pub fn uninstall(
    paths: &Paths,
    home: &Path,
    agents: &[Agent],
    all: bool,
    start: bool,
    confirm: Confirm,
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
            let new = match agent {
                Agent::Claude => without_include(&old),
                Agent::Codex => without_block(&old),
            };
            changes.push(Change {
                path: instructions,
                new: Some(new).filter(|t| !t.trim().is_empty()),
                old: Some(old),
            });
        }
        if agent == Agent::Claude {
            let file = include_file(home);
            if let Some(old) = read(&file)? {
                changes.push(Change {
                    path: file,
                    old: Some(old),
                    new: None,
                });
            }
        }
    }
    let service = service::file(home);
    if all && let Some(old) = read(&service)? {
        changes.push(Change {
            path: service,
            old: Some(old),
            new: None,
        });
    }
    if let Some(changes) = review(changes, confirm, out)? {
        if all && start {
            service::unload(home)?;
        }
        write(paths, &changes, &record, out)?;
    }
    Ok(())
}

/// Shows the changes and asks once. Returns the changes to write, if the
/// user agreed.
fn review(
    changes: Vec<Change>,
    confirm: Confirm,
    out: &mut impl Write,
) -> Result<Option<Vec<Change>>> {
    let changes: Vec<Change> = changes.into_iter().filter(|c| c.old != c.new).collect();
    if changes.is_empty() {
        writeln!(out, "Nothing to change.")?;
        return Ok(None);
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
    let agreed = match confirm {
        Confirm::Ask(input) => ask(input, out, "Write these changes?")?,
        Confirm::DryRun => {
            writeln!(out, "Nothing written (dry run).")?;
            return Ok(None);
        }
        Confirm::Yes => true,
    };
    if !agreed {
        writeln!(out, "Nothing written.")?;
        return Ok(None);
    }
    Ok(Some(changes))
}

/// Backs up and writes each file, then the record.
fn write(paths: &Paths, changes: &[Change], record: &Record, out: &mut impl Write) -> Result<()> {
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
    Ok(())
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

    fn run(f: impl FnOnce(&mut dyn BufRead, &mut Vec<u8>) -> Result<()>, answers: &str) -> String {
        let mut out = Vec::new();
        f(&mut answers.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
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

        let shown = run(
            |i, o| install(&paths, &home, &agents, ats, false, Confirm::Ask(i), o),
            "y\n",
        );
        assert!(
            shown.contains("asks before each `ats run`"),
            "default mode: {shown}"
        );
        assert!(
            std::fs::read_to_string(service::file(&home))
                .unwrap()
                .contains("/opt/ats")
        );
        let doc = read_json(&settings);
        let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "hooks", "theme"], "key order kept");
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            read_json_str(SETTINGS)["hooks"]["PreToolUse"],
            "other hooks stay"
        );
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/opt/ats hook claude stop"
        );
        assert_eq!(
            doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "/opt/ats hook claude user-prompt-submit"
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Mine\n\nKeep this.\n\n@ats.md\n"
        );
        assert!(
            std::fs::read_to_string(include_file(&home))
                .unwrap()
                .contains("ats run --")
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
            run(
                |i, o| install(&paths, &home, &agents, ats, false, Confirm::Ask(i), o),
                ""
            )
            .contains("Nothing to change.")
        );

        run(
            |i, o| uninstall(&paths, &home, &agents, true, false, Confirm::Ask(i), o),
            "y\n",
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            original,
            "byte for byte"
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Mine\n\nKeep this.\n"
        );
        assert!(!include_file(&home).exists() && !service::file(&home).exists());
    }

    fn read_json_str(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn replaces_what_an_earlier_version_installed() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = home.join(".claude/settings.json");
        // An earlier install moved the rewriting hook into its chain, added
        // a PreToolUse hook, and wrote its instructions in place.
        let mut earlier = read_json_str(SETTINGS);
        earlier["hooks"]["PreToolUse"][1] = json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "/opt/ats hook claude pre-tool-use" }] });
        earlier["permissions"] = json!({ "defaultMode": "auto" });
        std::fs::write(&settings, json_text(&earlier)).unwrap();
        let record = json!({ "agents": { "claude": [
            { "group": 1, "index": 0, "hook": { "type": "command", "command": "wrap hook claude", "timeout": 5 } }
        ] } });
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(&paths.installed, record.to_string()).unwrap();
        std::fs::write(
            home.join(".claude/CLAUDE.md"),
            format!("# Mine\n\n{BEGIN}\nold\n{END}\n"),
        )
        .unwrap();

        let shown = run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &[Agent::Claude],
                    Path::new("/opt/ats"),
                    false,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        assert!(
            shown.contains("they go back: wrap hook claude") && !shown.contains("asks before each"),
            "{shown}"
        );
        let doc = read_json(&settings);
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            read_json_str(SETTINGS)["hooks"]["PreToolUse"],
            "the moved hook is back"
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
            "# Mine\n\n@ats.md\n"
        );
        assert_eq!(Record::load(&paths).unwrap(), Record::default());
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
                    Confirm::Ask(i),
                    o,
                )
            },
            "n\n",
        );
        assert!(shown.ends_with("Nothing written.\n"), "{shown}");
        assert_eq!(
            std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
            SETTINGS
        );
        assert!(
            !home.join(".claude/CLAUDE.md").exists()
                && !include_file(&home).exists()
                && !paths.installed.exists()
        );
    }

    #[test]
    fn codex_gets_its_instructions_in_place_and_loses_them_again() {
        let (_dir, paths, home) = setup();
        let ats = Path::new("/Applications/My Tools/ats");
        run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &[Agent::Codex],
                    ats,
                    false,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        let hooks = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
        assert!(
            hooks.contains(r"'/Applications/My Tools/ats' hook codex stop"),
            "{hooks}"
        );
        let agents_md = std::fs::read_to_string(home.join(".codex/AGENTS.md")).unwrap();
        assert!(agents_md.starts_with(BEGIN) && agents_md.contains("ats run --"));
        run(
            |i, o| {
                uninstall(
                    &paths,
                    &home,
                    &[Agent::Codex],
                    true,
                    false,
                    Confirm::Ask(i),
                    o,
                )
            },
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
            Confirm::Ask(&mut "y\n".as_bytes()),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not valid JSON"), "{err:#}");
    }

    #[test]
    fn an_agent_shows_the_changes_then_writes_them() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let ats = Path::new("/opt/ats");
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &[Agent::Claude],
            ats,
            false,
            Confirm::DryRun,
            &mut out,
        )
        .unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(
            shown.contains("+@ats.md") && shown.ends_with("Nothing written (dry run).\n"),
            "{shown}"
        );
        assert!(!home.join(".claude/settings.json").exists() && !include_file(&home).exists());
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &[Agent::Claude],
            ats,
            false,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert!(include_file(&home).exists() && home.join(".claude/settings.json").exists());
    }

    #[test]
    fn the_service_alone_touches_no_agent() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &[],
            Path::new("/opt/ats"),
            false,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert!(service::file(&home).exists());
        assert!(
            !home.join(".claude/settings.json").exists()
                && !home.join(".claude/CLAUDE.md").exists()
        );
        assert!(
            instructions_block().starts_with(BEGIN)
                && instructions_block().ends_with(&format!("{END}\n"))
        );
    }

    #[test]
    fn the_import_line_comes_and_goes_cleanly() {
        for (text, back) in [
            ("", ""),
            ("# Mine\n", "# Mine\n"),
            ("# Mine\n\n\n", "# Mine\n"),
            ("@ats.md\n", ""),
        ] {
            let with = with_include(text);
            assert!(
                with.ends_with("@ats.md\n") && with.matches(INCLUDE).count() == 1,
                "{text:?}"
            );
            assert_eq!(without_include(&with), back, "{text:?}");
        }
    }
}
