//! Agent hooks (design 6.2). Agents send heavy commands to `ats run`
//! themselves, as their instructions tell them; the hooks only remind them
//! of their session's submissions. `Stop` holds the agent once while
//! submissions still run, and `UserPromptSubmit` reports the ones that
//! ended. When the daemon cannot be reached they let everything through
//! and change nothing.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client;
use crate::paths::Paths;
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
    /// Answers nothing. Earlier versions of `ats install` registered it, and
    /// an agent blocks the command when its hook fails.
    PreToolUse,
    Stop,
    UserPromptSubmit,
}

/// What the hook tells the agent.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Nothing,
    /// Print this JSON.
    Json(Value),
}

/// Reads the hook input from stdin and answers it.
pub async fn run(paths: &Paths, event: Event) -> Result<Answer> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let input: Value = serde_json::from_str(&text).context("reading the hook input")?;
    Ok(match event {
        Event::PreToolUse => Answer::Nothing,
        Event::Stop => stop(paths, &input).await,
        Event::UserPromptSubmit => user_prompt_submit(paths, &input).await,
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
/// once (design 6.2).
async fn user_prompt_submit(paths: &Paths, input: &Value) -> Answer {
    let Some(ended) = reminders(paths, "POST", input, "ended")
        .await
        .filter(|r| !r.is_empty())
    else {
        return Answer::Nothing;
    };
    let lines: Vec<String> = ended.iter().map(line).collect();
    Answer::Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": format!(
                "ats: submissions from this session ended since your last turn; `ats wait <submission>` shows the result:\n{}",
                lines.join("\n")
            ),
        }
    }))
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

    #[test]
    fn quoting() {
        assert_eq!(quote("s-1_a.b"), "s-1_a.b");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
