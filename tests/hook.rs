//! The hook and nested runs through the real binary (design 6.1, 6.3).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn ats(home: &std::path::Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ats"))
        .args(args)
        .env("ATS_HOME", home)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn rewrites_through_the_configured_chain() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Another rewriting hook that prefixes every command with `wrap`.
    std::fs::write(
        home.join("config.toml"),
        r#"
[recognize]
wrappers = ["wrap"]

[hook.claude]
chain = ["python3 -c 'import json,sys; c = json.load(sys.stdin)[\"tool_input\"][\"command\"]; print(json.dumps({\"hookSpecificOutput\": {\"hookEventName\": \"PreToolUse\", \"updatedInput\": {\"command\": \"wrap \" + c}}}))'"]
"#,
    )
    .unwrap();
    let input = |command: &str| {
        serde_json::json!({
            "session_id": "s1", "cwd": dir.path(), "hook_event_name": "PreToolUse",
            "tool_name": "Bash", "tool_input": { "command": command },
        })
        .to_string()
    };
    let out = ats(
        &home,
        &["hook", "claude", "pre-tool-use"],
        &input("cargo build"),
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let command = answer["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap_or_default();
    assert!(
        command.ends_with(" run --session s1 --sh 'wrap cargo build'"),
        "{command}"
    );

    // Codex has no chain configured: a light command is left alone.
    let out = ats(&home, &["hook", "codex", "pre-tool-use"], &input("ls"), &[]);
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
}

#[test]
fn runs_directly_inside_a_job() {
    // No daemon is running: inside a job, `ats run` never contacts one.
    let dir = tempfile::tempdir().unwrap();
    let out = ats(
        dir.path(),
        &["run", "--sh", "echo nested; exit 7"],
        "",
        &[("ATS_JOB_ID", "j_parent")],
    );
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "nested\n");
}
