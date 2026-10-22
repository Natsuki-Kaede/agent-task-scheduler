//! The executor (design 7): a process detached from the agent's tool call
//! that waits for its lease, runs the command, keeps its output, and reports
//! the main execution process result and the job's terminal state.

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
/// so one job's two streams stay within 50 MB (design 13).
const LOG_HEAD: usize = 24 << 20;
const LOG_TAIL: usize = 1 << 20;
const CONTROL_POLL: Duration = Duration::from_millis(500);
const GROUP_POLL: Duration = Duration::from_millis(250);
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Environment variable that marks processes belonging to a job (design 9.1)
/// and tells hooks and shims not to queue nested commands (design 6.5).
pub const JOB_ENV: &str = "ATS_JOB_ID";

const RETRY: Duration = Duration::from_millis(500);

/// Sends a report until the daemon answers it; the daemon may be
/// restarting, and the job carries on meanwhile (design 7.3 ⑥). Reports are
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

/// Registers this executor and waits for the lease. Returns false when the
/// job ended before it could start, for example by being canceled.
async fn acquire_lease(paths: &Paths, job: &str) -> Result<bool> {
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
        409 => return Ok(false),
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
            200 if answer.body["granted"] == true => return Ok(true),
            // The daemon already held the request while it waited.
            200 => {}
            409 => return Ok(false),
            s => bail!("asking for the lease: daemon answered {s}: {}", answer.body),
        }
    }
}

/// Chooses the log directory: the data directory when writable, else the
/// temporary directory, which sandboxed agents can write (design 6.8).
fn log_dir(paths: &Paths, job: &str) -> Result<PathBuf> {
    let preferred = paths.logs.join(job);
    if std::fs::create_dir_all(&preferred).is_ok() {
        return Ok(preferred);
    }
    let fallback = std::env::temp_dir().join("ats").join("logs").join(job);
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
    if !acquire_lease(&paths, &job).await? {
        return Ok(());
    }

    let dir = log_dir(&paths, &job)?;
    let mut cmd = tokio::process::Command::from(spec.command());
    cmd.current_dir(&cwd)
        .env(JOB_ENV, &job)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            std::fs::write(
                dir.join("stderr.log"),
                format!("ats: could not start the command: {e}\n"),
            )?;
            post(
                &paths,
                &format!("/v1/jobs/{job}/started"),
                json!({ "log_path": dir }),
            )
            .await?;
            post(
                &paths,
                &format!("/v1/jobs/{job}/finished"),
                json!({ "state": "start_failed", "exit_code": 127 }),
            )
            .await?;
            return Ok(());
        }
    };
    let pgid = child.id().context("child has no pid")? as i32;
    post(
        &paths,
        &format!("/v1/jobs/{job}/started"),
        json!({ "log_path": dir, "pgid": pgid }),
    )
    .await?;

    let out = tokio::spawn(pump(
        child.stdout.take().expect("piped stdout"),
        dir.join("stdout.log"),
    ));
    let err = tokio::spawn(pump(
        child.stderr.take().expect("piped stderr"),
        dir.join("stderr.log"),
    ));

    let mut canceled = false;
    let main = tokio::select! {
        status = child.wait() => Some(exit_code(status?)),
        () = wait_for_cancel(&paths, &job) => { canceled = true; None }
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
    // and both output streams to have reached the end (design 5.5).
    out.await??;
    err.await??;
    post(
        &paths,
        &format!("/v1/jobs/{job}/main-result"),
        json!({ "exit_code": main }),
    )
    .await?;

    // The job ends when every process in its group has exited (design 7.3).
    while procinfo::group_alive(pgid) {
        tokio::select! {
            () = tokio::time::sleep(GROUP_POLL) => {}
            () = wait_for_cancel(&paths, &job) => {
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

/// The end report, with the usage of every process the command waited for:
/// sampling misses commands shorter than its one-second interval
/// (design 9.2).
fn ended(state: &str, exit_code: Option<i64>) -> serde_json::Value {
    // SAFETY: getrusage fills the struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    // ru_maxrss is in bytes on macOS and kilobytes on Linux.
    let max_rss = if cfg!(target_os = "macos") {
        ru.ru_maxrss
    } else {
        ru.ru_maxrss * 1024
    };
    json!({
        "state": state,
        "exit_code": exit_code,
        "cpu_seconds": secs(ru.ru_utime) + secs(ru.ru_stime),
        "max_rss": max_rss,
    })
}

async fn wait_for_cancel(paths: &Paths, job: &str) {
    loop {
        tokio::time::sleep(CONTROL_POLL).await;
        if let Ok((answer, _)) =
            client::call(paths, "GET", &format!("/v1/jobs/{job}/control"), None).await
            && answer.body["cancel"] == true
        {
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
pub fn error_log(job: &str) -> PathBuf {
    std::env::temp_dir()
        .join("ats")
        .join("executors")
        .join(format!("{job}.log"))
}

/// Starts an executor for `job` as a detached process in its own session,
/// so that it outlives the agent's tool call (design 7.1). The executor
/// inherits the caller's environment, working directory and stdin. Returns
/// its process id.
pub fn spawn_detached(job: &str, spec: &RunSpec, cwd: &Path) -> Result<i32> {
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
            let path = error_log(job);
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

        stop_tx.send(()).unwrap();
        d.await.unwrap().unwrap();
    }
}
