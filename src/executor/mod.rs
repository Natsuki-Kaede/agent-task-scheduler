//! The executor (design 7): a process detached from the agent's tool call
//! that waits for its lease, runs the command, keeps its output, and reports
//! the main execution process result and the job's terminal state.

mod jobserver;
mod logwriter;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::client;
use crate::paths::Paths;
use crate::procinfo;
use crate::runspec::RunSpec;
use logwriter::CappedLog;

/// Per-stream log cap: a 24 MB head on disk plus a 1 MB tail kept in memory,
/// so one job's two streams stay within 50 MB (design 7.2).
const LOG_HEAD: usize = 24 << 20;
const LOG_TAIL: usize = 1 << 20;
const CONTROL_POLL: Duration = Duration::from_millis(500);
const GROUP_POLL: Duration = Duration::from_millis(250);
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Tells hooks and `ats run` inside a job not to queue nested commands
/// (design 6.3).
pub const JOB_ENV: &str = "ATS_JOB_ID";

const RETRY: Duration = Duration::from_millis(500);

/// Sends a report until the daemon answers it; the daemon may be
/// restarting, and the job carries on meanwhile (design 7.1). Reports are
/// idempotent, so a repeat after a lost answer is harmless.
async fn report(paths: &Paths, path: &str, body: serde_json::Value) -> client::Answer {
    loop {
        match client::call(paths, "POST", path, Some(body.clone())).await {
            Ok((answer, _)) if answer.status < 500 => return answer,
            Ok((answer, _)) => eprintln!("ats executor: {path}: daemon answered {}", answer.status),
            Err(e) => eprintln!("ats executor: {path}: {e:#}"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn post(paths: &Paths, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
    let answer = report(paths, path, body).await;
    if !(200..300).contains(&answer.status) {
        bail!("{path}: daemon answered {}: {}", answer.status, answer.body);
    }
    Ok(answer.body)
}

/// Registers this executor and waits for the lease. Returns the lease's
/// terms, or `None` when the job ended before it could start, for example
/// by being canceled.
async fn acquire_lease(paths: &Paths, job: &str) -> Result<Option<serde_json::Value>> {
    let pid = std::process::id() as i32;
    let start = procinfo::start_time(pid).context("reading this process's start time")?;
    let answer = report(
        paths,
        &format!("/v1/jobs/{job}/executor"),
        json!({ "pid": pid, "start": start }),
    )
    .await;
    match answer.status {
        204 => {}
        409 => return Ok(None),
        s => bail!(
            "registering the executor: daemon answered {s}: {}",
            answer.body
        ),
    }
    loop {
        let answer = report(
            paths,
            &format!("/v1/jobs/{job}/lease"),
            json!({ "pid": pid }),
        )
        .await;
        match answer.status {
            200 if answer.body["granted"] == true => return Ok(Some(answer.body)),
            // The daemon already held the request while it waited.
            200 => {}
            409 => return Ok(None),
            s => bail!("asking for the lease: daemon answered {s}: {}", answer.body),
        }
    }
}

/// Chooses the log directory: the data directory when writable, else the
/// temporary directory, which sandboxed agents can write (design 4).
fn log_dir(paths: &Paths, job: &str) -> Result<PathBuf> {
    let preferred = paths.logs.join(job);
    if std::fs::create_dir_all(&preferred).is_ok() {
        return Ok(preferred);
    }
    let fallback = paths.temporary.join("logs").join(job);
    crate::channel::prepare(&fallback)?;
    Ok(fallback)
}

async fn pump(mut src: impl tokio::io::AsyncRead + Unpin, path: PathBuf) -> Result<()> {
    let mut log = CappedLog::create(&path, LOG_HEAD, LOG_TAIL)?;
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        log.write(&buf[..n])?;
    }
    log.finish()?;
    Ok(())
}

fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|s| 128 + i64::from(s)))
        .unwrap_or(-1)
}

/// Runs one job to its terminal state.
pub async fn run(paths: Paths, job: String, spec: RunSpec, cwd: PathBuf) -> Result<()> {
    let Some(terms) = acquire_lease(&paths, &job).await? else {
        return Ok(());
    };

    let dir = log_dir(&paths, &job)?;
    let mut cmd = tokio::process::Command::from(spec.command());
    // Parallelism from the lease (design 10.5); what the submitter set
    // itself stays.
    if let Some(n) = terms["parallelism"].as_u64() {
        for var in terms["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            if std::env::var_os(var).is_none() {
                cmd.env(var, n.to_string());
            }
        }
    }
    let own_jobserver = ["MAKEFLAGS", "CARGO_MAKEFLAGS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
    let pool = match terms["jobserver_tokens"].as_u64() {
        Some(tokens) if !own_jobserver => {
            let pool =
                jobserver::Pool::create(&paths.temporary.join("jobserver"), &job, tokens as u32)?;
            cmd.env("MAKEFLAGS", pool.makeflags(tokens as u32));
            Some(pool)
        }
        _ => None,
    };
    let result = run_command(&paths, &job, cmd, &cwd, &dir).await;
    drop(pool);
    result
}

/// Starts the command and follows it to its terminal state.
async fn run_command(
    paths: &Paths,
    job: &str,
    mut cmd: tokio::process::Command,
    cwd: &Path,
    dir: &Path,
) -> Result<()> {
    let (paths, job) = (paths.clone(), job.to_string());
    // Keep the group alive until its id is committed and the command has
    // joined it. If this executor dies before registration, EOF makes the
    // leader exit without having started any user command.
    let mut leader = tokio::process::Command::new("/bin/sh")
        .args(["-c", "read -r _"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting the job's process group")?;
    let pgid = leader.id().context("group leader has no pid")? as i32;
    post(
        &paths,
        &format!("/v1/jobs/{job}/started"),
        json!({ "log_path": dir, "pgid": pgid }),
    )
    .await?;
    cmd.current_dir(cwd)
        .env(JOB_ENV, &job)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(pgid);
    let spawned = cmd.spawn();
    drop(leader.stdin.take());
    leader.wait().await?;
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            std::fs::write(
                dir.join("stderr.log"),
                format!("ats: could not start the command: {e}\n"),
            )?;
            post(
                &paths,
                &format!("/v1/jobs/{job}/finished"),
                json!({ "state": "start_failed", "exit_code": 127 }),
            )
            .await?;
            return Ok(());
        }
    };
    let out = tokio::spawn(pump(
        child.stdout.take().expect("piped stdout"),
        dir.join("stdout.log"),
    ));
    let err = tokio::spawn(pump(
        child.stderr.take().expect("piped stderr"),
        dir.join("stderr.log"),
    ));

    let mut canceled = false;
    let cancel = wait_for_cancel(&paths, &job);
    tokio::pin!(cancel);
    let main = tokio::select! {
        status = child.wait() => Some(exit_code(status?)),
        () = &mut cancel => { canceled = true; None }
    };
    if canceled {
        stop_group(pgid).await;
        let _ = child.wait().await;
        let _ = out.await;
        let _ = err.await;
        post(
            &paths,
            &format!("/v1/jobs/{job}/finished"),
            ended("canceled", None),
        )
        .await?;
        return Ok(());
    }
    let main = main.expect("main result");

    // The main execution process result needs the process to have exited
    // and both output streams to have reached the end (design 3).
    let streams = async {
        out.await??;
        err.await??;
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(streams);
    tokio::select! {
        result = &mut streams => result?,
        () = &mut cancel => {
            stop_group(pgid).await;
            streams.await?;
            post(&paths, &format!("/v1/jobs/{job}/finished"), ended("canceled", Some(main))).await?;
            return Ok(());
        }
    }
    post(
        &paths,
        &format!("/v1/jobs/{job}/main-result"),
        json!({ "exit_code": main }),
    )
    .await?;

    // The job ends when every process in its group has exited (design 3).
    while procinfo::group_alive(pgid) {
        tokio::select! {
            () = tokio::time::sleep(GROUP_POLL) => {}
            () = &mut cancel => {
                stop_group(pgid).await;
                post(&paths, &format!("/v1/jobs/{job}/finished"), ended("canceled", Some(main))).await?;
                return Ok(());
            }
        }
    }
    post(
        &paths,
        &format!("/v1/jobs/{job}/finished"),
        ended("completed", Some(main)),
    )
    .await?;
    Ok(())
}

/// The end report, with the largest resident size of any process the
/// command waited for, in bytes: sampling misses commands shorter than its
/// interval (design 8.3).
fn ended(state: &str, exit_code: Option<i64>) -> serde_json::Value {
    // SAFETY: getrusage fills the struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    // macOS reports bytes, Linux kilobytes.
    let unit = if cfg!(target_os = "linux") { 1024 } else { 1 };
    json!({ "state": state, "exit_code": exit_code, "max_rss": ru.ru_maxrss * unit })
}

/// Polls the daemon until the job is canceled.
async fn wait_for_cancel(paths: &Paths, job: &str) {
    loop {
        tokio::time::sleep(CONTROL_POLL).await;
        let Ok((answer, _)) =
            client::call(paths, "GET", &format!("/v1/jobs/{job}/control"), None).await
        else {
            continue;
        };
        if answer.body["cancel"] == true {
            return;
        }
    }
}

async fn stop_group(pgid: i32) {
    procinfo::signal_group(pgid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + TERM_GRACE;
    while procinfo::group_alive(pgid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(GROUP_POLL).await;
    }
    if procinfo::group_alive(pgid) {
        procinfo::signal_group(pgid, libc::SIGKILL);
    }
}

/// Where a detached executor writes its own errors; removed when it ends
/// cleanly.
pub fn error_log(paths: &Paths, job: &str) -> PathBuf {
    paths.temporary.join("executors").join(format!("{job}.log"))
}

/// Starts an executor for `job` as a detached process in its own session,
/// so that it outlives the agent's tool call (design 7.1). The executor
/// inherits the caller's environment, working directory and stdin. Returns
/// its process id.
pub fn spawn_detached(paths: &Paths, job: &str, spec: &RunSpec, cwd: &Path) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("locating the ats binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__executor")
        .arg("--job")
        .arg(job)
        .arg("--spec")
        .arg(serde_json::to_string(spec)?)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr({
            crate::channel::prepare(&paths.temporary)?;
            let path = error_log(paths, job);
            crate::channel::prepare(path.parent().expect("has a parent"))?;
            std::fs::File::create(path)?
        });
    // SAFETY: setsid is async-signal-safe and runs in the child before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("starting the executor")?;
    Ok(child.id() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon;
    use crate::daemon::tests::{test_paths, wait_for_socket};

    async fn submit(paths: &Paths, source: &str) -> (String, String) {
        let body = json!({"entry": "sh", "source": source, "cwd": "/"});
        let created = post(paths, "/v1/jobs", body).await.unwrap();
        (
            created["submission"].as_str().unwrap().into(),
            created["job"].as_str().unwrap().into(),
        )
    }

    async fn job(paths: &Paths, job: &str) -> serde_json::Value {
        client::call(paths, "GET", &format!("/v1/jobs/{job}"), None)
            .await
            .unwrap()
            .0
            .body
    }

    fn sh(source: &str) -> RunSpec {
        RunSpec::Sh {
            source: source.into(),
            shell: "/bin/sh".into(),
        }
    }

    #[tokio::test]
    async fn runs_jobs_to_their_terminal_states() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let d = tokio::spawn(daemon::run(paths.clone(), async {
            let _ = stop_rx.await;
        }));
        wait_for_socket(&paths).await;

        // Streams are kept apart and the main exit code is reported.
        let (_, j) = submit(&paths, "x").await;
        let spec = sh("echo out; echo err >&2; exit 3");
        run(paths.clone(), j.clone(), spec, "/".into())
            .await
            .unwrap();
        let row = job(&paths, &j).await;
        assert_eq!(
            (row["state"].as_str(), row["exit_code"].as_i64()),
            (Some("completed"), Some(3))
        );
        assert_eq!(row["main_exit_code"], 3);
        let logs = PathBuf::from(row["log_path"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(logs.join("stdout.log")).unwrap(),
            "out\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("stderr.log")).unwrap(),
            "err\n"
        );

        // Registering the group first preserves command-spawn failures.
        let (_, j) = submit(&paths, "x").await;
        let spec = RunSpec::Argv {
            argv: vec![
                dir.path()
                    .join("missing-command")
                    .to_string_lossy()
                    .into_owned(),
            ],
        };
        run(paths.clone(), j.clone(), spec, "/".into())
            .await
            .unwrap();
        let row = job(&paths, &j).await;
        assert_eq!(row["state"], "start_failed");
        assert_eq!(row["exit_code"], 127);
        let logs = PathBuf::from(row["log_path"].as_str().unwrap());
        assert!(
            std::fs::read_to_string(logs.join("stderr.log"))
                .unwrap()
                .contains("could not start the command")
        );

        // A background child keeps the job running after the main result.
        let (_, j) = submit(&paths, "x").await;
        let spec = sh("sleep 1 >/dev/null 2>&1 & echo started");
        let task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
        let mut saw_main_while_running = false;
        while !task.is_finished() {
            let row = job(&paths, &j).await;
            if row["main_exit_code"] == 0 && row["state"] == "running" {
                saw_main_while_running = true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        task.await.unwrap().unwrap();
        assert!(saw_main_while_running);
        assert_eq!(job(&paths, &j).await["state"], "completed");

        // Canceling the only submission stops the whole process group.
        let (s, j) = submit(&paths, "x").await;
        let marker = dir.path().join("child.pid");
        let spec = sh(&format!("sleep 30 & echo $! > {}; wait", marker.display()));
        let task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        post(&paths, &format!("/v1/submissions/{s}/cancel"), json!({}))
            .await
            .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(job(&paths, &j).await["state"], "canceled");
        let child: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks for existence.
        assert_ne!(
            unsafe { libc::kill(child, 0) },
            0,
            "background child survived"
        );

        // Cancellation works after the shell exits, both while draining
        // inherited output pipes and while waiting on a quiet group.
        for redirect in ["", ">/dev/null 2>&1"] {
            let (s, j) = submit(&paths, "x").await;
            let marker = dir.path().join("background.pids");
            let _ = std::fs::remove_file(&marker);
            let spec = sh(&format!(
                "sleep 30 {redirect} & echo $$ $! > {}; exit 0",
                crate::hook::quote(&marker.to_string_lossy())
            ));
            let mut task = tokio::spawn(run(paths.clone(), j.clone(), spec, "/".into()));
            let pids = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&marker) {
                        let pids: Vec<i32> = text
                            .split_whitespace()
                            .filter_map(|p| p.parse().ok())
                            .collect();
                        if pids.len() == 2 && procinfo::start_time(pids[0]).is_none() {
                            break pids;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            post(&paths, &format!("/v1/submissions/{s}/cancel"), json!({}))
                .await
                .unwrap();
            let finished = tokio::time::timeout(Duration::from_secs(8), &mut task).await;
            if finished.is_err() {
                // SAFETY: stops the child this test started before failing.
                unsafe { libc::kill(pids[1], libc::SIGKILL) };
                task.abort();
            }
            finished.unwrap().unwrap().unwrap();
            assert_eq!(job(&paths, &j).await["state"], "canceled", "{redirect}");
            assert!(
                procinfo::start_time(pids[1]).is_none(),
                "background child survived: {redirect}"
            );
        }

        stop_tx.send(()).unwrap();
        d.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn executor_loss_before_registration_never_starts_the_command() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route(
            "/v1/jobs/j_gate/started",
            post(move |Json(body): Json<serde_json::Value>| {
                let tx = tx.clone();
                async move {
                    tx.send(body["pgid"].as_i64().unwrap() as i32)
                        .await
                        .unwrap();
                    // Model a daemon that has not committed the registration.
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let marker = dir.path().join("command-started");
        let cmd = tokio::process::Command::from(
            sh(&format!(
                "echo started > {}",
                crate::hook::quote(&marker.to_string_lossy())
            ))
            .command(),
        );
        let task = tokio::spawn(async move {
            run_command(&paths, "j_gate", cmd, Path::new("/"), &paths.home).await
        });
        let pgid = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !marker.exists(),
            "command ran before registration was committed"
        );
        // Dropping this task closes the same descriptors as executor exit.
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while procinfo::group_alive(pgid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(!marker.exists());
        server.abort();
    }
}
