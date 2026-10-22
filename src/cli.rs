//! Client commands: `ats run`, `wait`, `status`, `logs`, `cancel`, `detach`
//! and the return contract of design 5.5.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::client::{self, Answer};
use crate::executor;
use crate::paths::Paths;
use crate::procinfo;
use crate::runspec::RunSpec;
use crate::store::is_terminal;
use crate::timing::Deadline;

/// The command was not finished within the wait; it continues in the
/// background.
pub const EXIT_DETACHED: u8 = 75;
/// The command cannot run because a predecessor failed.
pub const EXIT_BLOCKED: u8 = 76;
/// The submission was rejected.
pub const EXIT_REJECTED: u8 = 77;

/// Lines of log attached when a failed terminal state is retrieved.
const TAIL_LINES: usize = 50;
/// Longest single wait request, so a request-file answer stays bounded.
const WAIT_CHUNK: Duration = Duration::from_secs(30);
const FOLLOW_POLL: Duration = Duration::from_millis(500);
const RECONNECT_PAUSE: Duration = Duration::from_millis(500);

/// Identity and correlation fields an adapter's hook adds (design 6.7).
pub struct Identity {
    pub session: Option<String>,
    pub tool: Option<String>,
    pub call: Option<String>,
    pub request_id: Option<String>,
}

/// Scheduling options of `ats run` (design 5.3).
#[derive(clap::Args, Debug, Default)]
pub struct RunOptions {
    /// Run only after these jobs or submissions succeed (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub after: Vec<String>,
    /// Run at once, skipping the queue. Only when explicitly asked to run
    /// now; refused when a dependency or lock conflicts.
    #[arg(long)]
    pub now: bool,
    /// Also lock this path or named resource, such as a device or port.
    #[arg(long, value_delimiter = ',')]
    pub lock: Vec<String>,
    /// Expected use, such as mem=6G,cpu=4; used where higher than predicted.
    #[arg(long)]
    pub hint: Option<String>,
    #[arg(long, value_parser = ["low", "normal", "high"])]
    pub urgency: Option<String>,
    /// Why the urgency is needed.
    #[arg(long, requires = "urgency")]
    pub reason: Option<String>,
    /// Under memory pressure, slow it down but never freeze it.
    #[arg(long)]
    pub no_freeze: bool,
    /// Do not share a run with an equal job already running.
    #[arg(long)]
    pub no_merge: bool,
    /// A long-running service rather than a command that ends.
    #[arg(long)]
    pub service: bool,
}

fn expect_ok(answer: Answer) -> Result<Value> {
    if !(200..300).contains(&answer.status) {
        bail!("{}", error_text(&answer));
    }
    Ok(answer.body)
}

fn error_text(answer: &Answer) -> String {
    match answer.body["error"].as_str() {
        Some(e) => e.to_string(),
        None => format!("daemon answered {}: {}", answer.status, answer.body),
    }
}

async fn get(paths: &Paths, path: &str) -> Result<Value> {
    expect_ok(client::call(paths, "GET", path, None).await?.0)
}

async fn post(paths: &Paths, path: &str, body: Value) -> Result<Value> {
    expect_ok(client::call(paths, "POST", path, Some(body)).await?.0)
}

/// Waits for a submission to reach `until` ("main" or "terminal"), in
/// bounded requests. Returns whether it was reached, and the submission.
async fn wait_for(
    paths: &Paths,
    submission: &str,
    until: &str,
    wait: Option<Duration>,
) -> Result<(bool, Value)> {
    let deadline = Deadline::after(wait);
    let mut last: Option<Value> = None;
    loop {
        let chunk = deadline.remaining().unwrap_or(WAIT_CHUNK).min(WAIT_CHUNK);
        let called = client::call_with_timeout(
            paths,
            "POST",
            &format!("/v1/submissions/{submission}/wait"),
            Some(json!({ "wait_seconds": chunk.as_secs_f64(), "until": until })),
            chunk + Duration::from_secs(10),
        )
        .await;
        let answer = match (called, &last) {
            (Ok((answer, _)), _) => answer,
            // Once the daemon has answered, an unreachable daemon is
            // restarting; keep waiting within the time limit.
            (Err(_), Some(last)) if deadline.expired() => return Ok((false, last.clone())),
            (Err(_), Some(_)) => {
                tokio::time::sleep(RECONNECT_PAUSE).await;
                continue;
            }
            (Err(e), None) => return Err(e),
        };
        let body = expect_ok(answer)?;
        let reached = body["reached"] == true;
        if reached || deadline.expired() {
            return Ok((reached, body["submission"].clone()));
        }
        last = Some(body["submission"].clone());
    }
}

fn exit_code(code: i64) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The one-line status printed instead of command output (design 5.5 ③).
fn status_line(sub: &Value) -> String {
    let log = sub["log_path"].as_str().unwrap_or("not started yet");
    format!(
        "[ats] submission {} job {} is {}; log: {log}; check with: ats wait {}",
        sub["id"].as_str().unwrap_or("?"),
        sub["job"].as_str().unwrap_or("?"),
        sub["job_state"].as_str().unwrap_or("?"),
        sub["id"].as_str().unwrap_or("?"),
    )
}

fn read_log(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_default()
}

/// Writes the command's own output unchanged (design 5.5 ②).
fn print_output(sub: &Value) -> Result<()> {
    if let Some(dir) = sub["log_path"].as_str() {
        let dir = Path::new(dir);
        std::io::stdout().write_all(&read_log(dir, "stdout.log"))?;
        std::io::stdout().flush()?;
        std::io::stderr().write_all(&read_log(dir, "stderr.log"))?;
    }
    Ok(())
}

/// Prints the last lines of each non-empty stream, for failed jobs.
fn print_tails(log_path: Option<&str>) {
    let Some(dir) = log_path else { return };
    for name in ["stdout.log", "stderr.log"] {
        let data = read_log(Path::new(dir), name);
        if data.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(TAIL_LINES);
        eprintln!("[ats] last {} lines of {name}:", lines.len() - start);
        for line in &lines[start..] {
            eprintln!("{line}");
        }
    }
}

fn failed(sub: &Value) -> bool {
    sub["job_state"] != "completed" || sub["exit_code"].as_i64() != Some(0)
}

/// `ats run`: submits, starts the executor, and returns the main execution
/// process result or a status line.
pub async fn run(
    paths: &Paths,
    spec: RunSpec,
    wait: Option<Duration>,
    id: Identity,
    options: RunOptions,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let mut body = json!({
        "cwd": cwd,
        "session": id.session,
        "agent": id.tool,
        "invocation_key": id.call,
        "request_id": id.request_id,
        "launcher_pid": std::process::id(),
        "launcher_start": procinfo::start_time(std::process::id() as i32),
        "after": options.after,
        "now": options.now,
        "lock": options.lock,
        "hint": options.hint,
        "urgency": options.urgency,
        "reason": options.reason,
        "no_freeze": options.no_freeze,
        "no_merge": options.no_merge,
        "service": options.service,
    });
    match &spec {
        RunSpec::Argv { argv } => {
            body["entry"] = json!("argv");
            body["argv"] = json!(argv);
        }
        RunSpec::Sh { source, shell } => {
            body["entry"] = json!("sh");
            body["source"] = json!(source);
            body["shell"] = json!(shell);
        }
    }
    let (answer, _) = client::call(paths, "POST", "/v1/jobs", Some(body)).await?;
    if matches!(answer.status, 400 | 409) {
        eprintln!("ats: submission rejected: {}", error_text(&answer));
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let created = expect_ok(answer)?;
    let (submission, job) = (
        created["submission"].as_str().unwrap_or_default(),
        created["job"].as_str().unwrap_or_default(),
    );

    // A repeated request id returns the earlier submission; its job already
    // has an executor (design 6.7 ⑥).
    if created["existing"] != true {
        match executor::spawn_detached(job, &spec, &cwd) {
            // Register the executor before this process can exit, so the
            // job always has a known starter (the executor registers too).
            Ok(pid) => {
                if let Some(start) = procinfo::start_time(pid) {
                    let body = json!({ "pid": pid, "start": start });
                    client::call(
                        paths,
                        "POST",
                        &format!("/v1/jobs/{job}/executor"),
                        Some(body),
                    )
                    .await?;
                }
            }
            Err(e) => {
                post(
                    paths,
                    &format!("/v1/jobs/{job}/finished"),
                    json!({ "state": "start_failed", "exit_code": null }),
                )
                .await?;
                return Err(e);
            }
        }
    }

    let (reached, sub) = wait_for(paths, submission, "main", wait).await?;
    if sub["job_state"] == "blocked" {
        return blocked(paths, &sub).await;
    }
    if reached {
        if let Some(code) = sub["main_exit_code"].as_i64() {
            print_output(&sub)?;
            return Ok(exit_code(code));
        }
        if sub["job_state"] == "start_failed" {
            print_output(&sub)?;
            return Ok(exit_code(sub["exit_code"].as_i64().unwrap_or(127)));
        }
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

fn terminal_line(sub: &Value) -> String {
    let code = sub["exit_code"]
        .as_i64()
        .map_or("unknown".to_string(), |c| c.to_string());
    format!(
        "[ats] submission {} job {} is {}, exit code {code}",
        sub["id"].as_str().unwrap_or("?"),
        sub["job"].as_str().unwrap_or("?"),
        sub["job_state"].as_str().unwrap_or("?"),
    )
}

/// Reports a submission's terminal state, attaches log tails on failure,
/// and records that the state was retrieved (design 5.5).
async fn report_terminal(paths: &Paths, sub: &Value) -> Result<ExitCode> {
    eprintln!("{}", terminal_line(sub));
    if failed(sub) {
        print_tails(sub["log_path"].as_str());
    }
    let id = sub["id"].as_str().unwrap_or_default();
    post(paths, &format!("/v1/submissions/{id}/retrieved"), json!({})).await?;
    Ok(match sub["exit_code"].as_i64() {
        Some(code) => exit_code(code),
        None => ExitCode::from(1),
    })
}

/// Reports a job held back by a failed predecessor (design 5.5: 76).
async fn blocked(paths: &Paths, sub: &Value) -> Result<ExitCode> {
    let job = sub["job"].as_str().unwrap_or_default();
    let why = get(paths, &format!("/v1/jobs/{job}/why")).await?;
    let predecessor = why["verdict"]["job"].as_str().unwrap_or("a predecessor");
    eprintln!(
        "[ats] submission {} job {job} did not run: {predecessor} failed. Run `ats unblock {job}` to run it anyway, or `ats cancel {}`.",
        sub["id"].as_str().unwrap_or("?"),
        sub["id"].as_str().unwrap_or("?"),
    );
    Ok(ExitCode::from(EXIT_BLOCKED))
}

/// `ats wait`: waits for the job's terminal state (design 5.4, 6.7 ⑧).
pub async fn wait(paths: &Paths, submission: &str, wait: Option<Duration>) -> Result<ExitCode> {
    let (reached, sub) = wait_for(paths, submission, "terminal", wait).await?;
    if sub["job_state"] == "blocked" {
        return blocked(paths, &sub).await;
    }
    if reached {
        return report_terminal(paths, &sub).await;
    }
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::from(EXIT_DETACHED))
}

/// `ats status <id>` / `ats status --request <id>`.
pub async fn status_one(
    paths: &Paths,
    id: Option<&str>,
    request: Option<&str>,
    json_out: bool,
) -> Result<ExitCode> {
    let row = match (id, request) {
        (_, Some(rid)) => get(paths, &format!("/v1/requests/{rid}")).await?,
        (Some(id), None) if id.starts_with("j_") => get(paths, &format!("/v1/jobs/{id}")).await?,
        (Some(id), None) => get(paths, &format!("/v1/submissions/{id}")).await?,
        (None, None) => unreachable!("caller passes an id or a request id"),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&row)?);
    }
    // A job row has no submission; the retrieved marker is per submission.
    if row.get("job_state").is_none() {
        if !json_out {
            println!(
                "{} {} {}",
                row["id"].as_str().unwrap_or("?"),
                row["state"].as_str().unwrap_or("?"),
                row["command"].as_str().unwrap_or("")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if row["job_state"].as_str().is_some_and(is_terminal) {
        if json_out {
            let id = row["id"].as_str().unwrap_or_default();
            post(paths, &format!("/v1/submissions/{id}/retrieved"), json!({})).await?;
            return Ok(ExitCode::SUCCESS);
        }
        report_terminal(paths, &row).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if !json_out {
        println!("{}", status_line(&row));
    }
    Ok(ExitCode::SUCCESS)
}

/// The log directory and state of a submission or job.
async fn log_target(paths: &Paths, id: &str) -> Result<(Option<PathBuf>, String, String)> {
    let job = if id.starts_with("j_") {
        get(paths, &format!("/v1/jobs/{id}")).await?
    } else {
        let sub = get(paths, &format!("/v1/submissions/{id}")).await?;
        get(
            paths,
            &format!("/v1/jobs/{}", sub["job"].as_str().unwrap_or_default()),
        )
        .await?
    };
    Ok((
        job["log_path"].as_str().map(PathBuf::from),
        job["state"].as_str().unwrap_or_default().to_string(),
        job["id"].as_str().unwrap_or_default().to_string(),
    ))
}

/// `ats logs <id> [-f]`: prints the logs, stdout to stdout and stderr to
/// stderr; with `follow`, keeps printing until the job ends.
pub async fn logs(paths: &Paths, id: &str, follow: bool) -> Result<ExitCode> {
    let mut offsets = [0usize; 2];
    loop {
        let (dir, state, job) = log_target(paths, id).await?;
        let done = is_terminal(&state);
        if let Some(dir) = &dir {
            for (i, name) in ["stdout.log", "stderr.log"].into_iter().enumerate() {
                let data = read_log(dir, name);
                if data.len() > offsets[i] {
                    let new = &data[offsets[i]..];
                    if i == 0 {
                        std::io::stdout().write_all(new)?;
                        std::io::stdout().flush()?;
                    } else {
                        std::io::stderr().write_all(new)?;
                    }
                    offsets[i] = data.len();
                }
            }
        } else if !follow {
            eprintln!("[ats] job {job} is {state} and has no log yet");
        }
        if !follow || done {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
}

/// `ats cancel` / `ats detach`.
pub async fn mark(paths: &Paths, submission: &str, action: &str) -> Result<ExitCode> {
    post(
        paths,
        &format!("/v1/submissions/{submission}/{action}"),
        json!({}),
    )
    .await?;
    let sub = get(paths, &format!("/v1/submissions/{submission}")).await?;
    eprintln!("{}", status_line(&sub));
    Ok(ExitCode::SUCCESS)
}

/// A submission or job id to its job id.
async fn job_of(paths: &Paths, id: &str) -> Result<String> {
    if id.starts_with("j_") {
        return Ok(id.to_string());
    }
    let sub = get(paths, &format!("/v1/submissions/{id}")).await?;
    Ok(sub["job"].as_str().unwrap_or_default().to_string())
}

fn gib(bytes: f64) -> String {
    format!("{:.1}G", bytes / (1u64 << 30) as f64)
}

/// One sentence for a scheduler verdict.
fn explain(v: &Value) -> String {
    let r = &v["reason"];
    match (v["verdict"].as_str(), r["kind"].as_str()) {
        (Some("admit"), _) => "it is being admitted now".into(),
        (Some("blocked"), _) => format!(
            "predecessor {} failed; `ats unblock` runs it anyway",
            v["job"].as_str().unwrap_or("?")
        ),
        (Some("capacity_insufficient"), _) => format!(
            "it is predicted to need {}, more than the memory budget of {}; cancel it, give a lower --hint, or run it with --now",
            gib(v["need"].as_f64().unwrap_or(0.0)),
            gib(v["budget"].as_f64().unwrap_or(0.0))
        ),
        (_, Some("recovering")) => {
            "the daemon restarted and is waiting for running jobs to reconnect".into()
        }
        (_, Some("pressure")) => "memory pressure is rising, so nothing new is admitted".into(),
        (_, Some("interval")) => {
            "the last admitted job is still settling; admissions are spaced a few seconds apart"
                .into()
        }
        (_, Some("no_executor")) => "its executor has not registered yet".into(),
        (_, Some("dependency")) => format!(
            "waiting for {} to succeed",
            r["job"].as_str().unwrap_or("?")
        ),
        (_, Some("lock")) => format!(
            "{} is locked by {}",
            r["resource"].as_str().unwrap_or("?"),
            r["holder"].as_str().unwrap_or("?")
        ),
        (_, Some("memory")) => format!(
            "it needs {} of memory and {} is free",
            gib(r["need"].as_f64().unwrap_or(0.0)),
            gib(r["free"].as_f64().unwrap_or(0.0).max(0.0))
        ),
        (_, Some("cpu")) => format!(
            "it needs {:.1} cores and {:.1} are free",
            r["need"].as_f64().unwrap_or(0.0),
            r["free"].as_f64().unwrap_or(0.0)
        ),
        (_, Some("behind")) => format!(
            "{} is first in line and this would delay it",
            r["head"].as_str().unwrap_or("?")
        ),
        _ => v.to_string(),
    }
}

/// `ats why`: why a job is still waiting (design 5.4).
pub async fn why(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    let w = get(paths, &format!("/v1/jobs/{job}/why")).await?;
    let state = w["state"].as_str().unwrap_or("?");
    match w.get("verdict").filter(|v| !v.is_null()) {
        Some(v) => println!("job {job} is {state}: {}", explain(v)),
        None => println!("job {job} is {state}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// `ats unblock`: runs a job whose predecessor failed (design 5.4).
pub async fn unblock(paths: &Paths, id: &str) -> Result<ExitCode> {
    let job = job_of(paths, id).await?;
    post(paths, &format!("/v1/jobs/{job}/unblock"), json!({})).await?;
    println!("job {job} is queued again");
    Ok(ExitCode::SUCCESS)
}
