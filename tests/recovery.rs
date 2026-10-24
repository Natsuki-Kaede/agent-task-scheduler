//! Fault injection against the real binary (design 7.1): the daemon or an
//! executor is killed while a job runs.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Home {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home");
        Home { _dir: dir, path }
    }

    fn ats(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ats"))
            .args(args)
            .env("ATS_HOME", &self.path)
            // Tests may themselves run inside an ats job.
            .env_remove("ATS_JOB_ID")
            .output()
            .unwrap()
    }

    fn daemon(&self) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_ats"))
            .arg("daemon")
            .env("ATS_HOME", &self.path)
            .env_remove("ATS_JOB_ID")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let socket = self.path.join("ats.sock");
        wait_until("daemon socket", Duration::from_secs(15), || {
            socket.exists() && self.ats(&["status"]).status.success()
        });
        Daemon(child)
    }

    fn job(&self, id: &str) -> Value {
        let out = self.ats(&["status", id, "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

struct Daemon(Child);

impl Daemon {
    fn kill(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Submits without waiting and returns the submission and job ids.
fn submit(home: &Home, source: &str) -> (String, String) {
    let out = home.ats(&["run", "--no-wait", "--sh", source]);
    assert_eq!(out.status.code(), Some(75));
    let line = String::from_utf8(out.stderr).unwrap();
    let id = |prefix: &str| {
        line.split_whitespace()
            .find(|w| w.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} id in {line:?}"))
            .to_string()
    };
    (id("s_"), id("j_"))
}

fn log(home: &Home, job: &str) -> String {
    let dir = home.job(job)["log_path"].as_str().unwrap().to_string();
    std::fs::read_to_string(Path::new(&dir).join("stdout.log")).unwrap()
}

#[test]
fn a_job_survives_a_daemon_crash_and_reports_after_the_restart() {
    let home = Home::new();
    let daemon = home.daemon();
    let (sub, job) = submit(&home, "sleep 2; echo done");
    wait_until("job start", Duration::from_secs(5), || {
        home.job(&job)["state"] == "running"
    });

    daemon.kill();
    // The job ends while the daemon is down; its executor keeps the result
    // until the daemon is back.
    std::thread::sleep(Duration::from_secs(3));
    let _daemon = home.daemon();

    let out = home.ats(&["wait", &sub, "--wait", "20s"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = home.job(&job);
    assert_eq!(
        (row["state"].as_str(), row["exit_code"].as_i64()),
        (Some("completed"), Some(0))
    );
    assert_eq!(log(&home, &job), "done\n");
}

#[test]
fn a_lost_executor_holds_the_job_until_its_processes_exit() {
    let home = Home::new();
    let _daemon = home.daemon();
    let (_, job) = submit(&home, "sleep 3");
    wait_until("job start", Duration::from_secs(5), || {
        home.job(&job)["state"] == "running"
    });

    let executor = home.job(&job)["executor_pid"].as_i64().unwrap() as i32;
    // SAFETY: kills the executor started for this test.
    assert_eq!(unsafe { libc::kill(executor, libc::SIGKILL) }, 0);

    wait_until("awaiting_takeover", Duration::from_secs(5), || {
        home.job(&job)["state"] == "awaiting_takeover"
    });
    // The command was not reported, so its exit code is unknown.
    wait_until("ended_unknown", Duration::from_secs(10), || {
        home.job(&job)["state"] == "ended_unknown"
    });
    assert!(home.job(&job)["exit_code"].is_null());
    // The killed executor could not remove its error log.
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("ats/executors/{job}.log")));
}
