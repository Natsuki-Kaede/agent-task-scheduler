//! The hook and nested runs through the real binary (design 6.1, 6.3).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn ats(home: &std::path::Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ats"))
        .args(args)
        .env("ATS_HOME", home)
        // Tests may themselves run inside an ats job.
        .env_remove("ATS_JOB_ID")
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
    // Another rewriting hook that prefixes every command with `wrap`,
    // moved into ats's chain by `ats install`.
    std::fs::write(
        home.join("config.toml"),
        "[recognize]\nwrappers = [\"wrap\"]\n",
    )
    .unwrap();
    let wrap = r#"python3 -c 'import json,sys; c = json.load(sys.stdin)["tool_input"]["command"]; print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": {"command": "wrap " + c}}}))'"#;
    let record = serde_json::json!({ "agents": { "claude": [
        { "group": 0, "index": 0, "hook": { "type": "command", "command": wrap } }
    ] } });
    std::fs::write(home.join("installed.json"), record.to_string()).unwrap();
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

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "timed out: {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn reminds_the_session_of_its_submissions() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hook = |event: &str, input: serde_json::Value| -> serde_json::Value {
        let out = ats(&home, &["hook", "claude", event], &input.to_string(), &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_default()
    };
    let stop = |active: bool| {
        hook(
            "stop",
            serde_json::json!({"session_id": "s1", "stop_hook_active": active}),
        )
    };
    let prompt = || {
        hook(
            "user-prompt-submit",
            serde_json::json!({"session_id": "s1"}),
        )
    };

    // Without a daemon, every reminder lets the agent through.
    assert!(stop(false).is_null() && prompt().is_null());

    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_ats"))
            .arg("daemon")
            .env("ATS_HOME", &home)
            .env_remove("ATS_JOB_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("daemon", || {
        ats(&home, &["status"], "", &[]).status.success()
    });
    let run = |args: &[&str]| {
        let mut all = vec!["run", "--session", "s1"];
        all.extend_from_slice(args);
        ats(&home, &all, "", &[])
    };
    // A result the agent already got is not mentioned; one left running is.
    assert!(run(&["--sh", "echo done"]).status.success());
    let out = run(&["--no-wait", "--sh", "sleep 2"]);
    assert_eq!(out.status.code(), Some(75));
    let line = String::from_utf8_lossy(&out.stderr).to_string();
    let submission = line.split_whitespace().nth(2).unwrap().to_string();

    let blocked = stop(false);
    assert_eq!(blocked["decision"], "block");
    let reason = blocked["reason"].as_str().unwrap();
    assert!(
        reason.contains(&submission) && reason.contains("sleep 2") && !reason.contains("echo done"),
        "{reason}"
    );
    assert!(stop(true).is_null(), "a second stop goes through");
    assert!(prompt().is_null(), "nothing has ended yet");

    wait_until("job end", || stop(false).is_null());
    let context = prompt()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        context.contains(&submission) && context.contains("exit code 0"),
        "{context}"
    );
    assert!(prompt().is_null(), "reported once");
}

/// Stops the daemon even when an assertion fails, so it does not outlive
/// the test.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
