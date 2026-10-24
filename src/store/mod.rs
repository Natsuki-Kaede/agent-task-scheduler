//! The daemon's SQLite store (design 11). Only the daemon opens it; writes
//! are committed before the daemon answers (design 7.1).

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::sched::locks::Lock;
use crate::sched::predict::Prediction;

mod history;
mod sched;

pub use sched::{HELD, PRE_LEASE};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 2;

/// How long ended jobs, their records and logs are kept (design 11).
pub const KEEP: std::time::Duration = std::time::Duration::from_secs(90 * 24 * 3600);

/// Current time as an RFC 3339 string with milliseconds.
pub fn now() -> String {
    humantime::format_rfc3339_millis(SystemTime::now()).to_string()
}

pub struct Store {
    conn: Connection,
}

/// Data needed to register a submission and its job.
#[derive(Debug, Clone)]
pub struct NewSubmission {
    pub entry: Entry,
    /// Redacted display form of the command.
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub session_id: Option<String>,
    /// The process that will start the executor, as (pid, start time).
    pub launcher: Option<(i64, String)>,
    pub plan: Plan,
}

/// What admission needs to know about a new job (design 10).
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub profiles: Vec<String>,
    pub prediction: Prediction,
    pub locks: Vec<Lock>,
    /// Predecessor job ids from `--after`.
    pub after: Vec<String>,
    pub run_now: bool,
    /// Tool and subcommand of a one-command line, for history lookups.
    pub history_key: Option<String>,
    /// Environment variables set to the leased parallelism (design 10.5).
    pub parallel_env: Vec<String>,
    /// The line runs jobserver clients, so the executor offers a pool.
    pub jobserver: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Entry {
    Argv,
    Sh,
}

impl Entry {
    fn as_str(self) -> &'static str {
        match self {
            Entry::Argv => "argv",
            Entry::Sh => "sh",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Created {
    pub submission: String,
    pub job: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: String,
    pub state: String,
    pub entry: String,
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub executor_pid: Option<i64>,
    /// Peak private memory measured so far, in bytes.
    pub peak_private: Option<i64>,
    pub submissions: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmissionRow {
    pub id: String,
    pub job: String,
    pub job_state: String,
    pub session_id: Option<String>,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub canceled: bool,
    /// `ats detach`: the `Stop` reminder does not hold the agent for it.
    pub detached: bool,
    /// Its end is not reported: the result already reached the agent, or
    /// it was reported once (design 6.2).
    pub quiet: bool,
}

/// A submission a reminder names.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Reminder {
    pub submission: String,
    pub job: String,
    pub state: String,
    pub exit_code: Option<i64>,
    pub command: String,
}

/// A job that has not ended, as the recovery check sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveJob {
    pub id: String,
    pub state: String,
    /// The executor process, as (pid, start time).
    pub executor: Option<(i64, String)>,
    pub pgid: Option<i64>,
    /// The process that starts the executor, as (pid, start time).
    pub launcher: Option<(i64, String)>,
    pub main_exit_code: Option<i64>,
    /// Every submission of the job is canceled.
    pub canceled: bool,
}

fn process(pid: Option<i64>, start: Option<String>) -> Option<(i64, String)> {
    pid.zip(start)
}

/// The named job or submission does not exist.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotFound(pub String);

/// The request does not fit the job's current state.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Conflict(pub String);

fn no_job(job: &str) -> anyhow::Error {
    NotFound(format!("no job {job}")).into()
}

fn no_submission(id: &str) -> anyhow::Error {
    NotFound(format!("no submission {id}")).into()
}

const JOB_COLUMNS: &str = "j.id, j.state, j.entry, j.command, j.cwd, j.project_root, j.exit_code,
    j.main_exit_code, j.log_path, j.executor_pid, j.peak_private,
    (SELECT COUNT(*) FROM submissions s WHERE s.job_id = j.id), j.created_at, j.started_at, j.ended_at";

fn job_row(r: &rusqlite::Row) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        state: r.get(1)?,
        entry: r.get(2)?,
        command: r.get(3)?,
        cwd: r.get(4)?,
        project_root: r.get(5)?,
        exit_code: r.get(6)?,
        main_exit_code: r.get(7)?,
        log_path: r.get(8)?,
        executor_pid: r.get(9)?,
        peak_private: r.get(10)?,
        submissions: r.get(11)?,
        created_at: r.get(12)?,
        started_at: r.get(13)?,
        ended_at: r.get(14)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            // Version 1 had no detach flag apart from quiet.
            1 => {
                conn.execute_batch(
                    "ALTER TABLE submissions ADD COLUMN detached INTEGER NOT NULL DEFAULT 0;
                     PRAGMA user_version = 2;",
                )?;
            }
            SCHEMA_VERSION => {}
            v => bail!("database schema version {v} is newer than this ats ({SCHEMA_VERSION})"),
        }
        Ok(Self { conn })
    }

    /// Registers a submission with a new queued job, its locks and its
    /// dependencies, in one transaction.
    pub fn create_submission(&mut self, new: &NewSubmission) -> Result<Created> {
        let tx = self.conn.transaction()?;
        let job = crate::ids::new("j");
        let submission = crate::ids::new("s");
        let at = now();
        let p = &new.plan;
        tx.execute(
            "INSERT INTO jobs (id, state, cwd, project_root, entry, command, profile, history_key,
                               predicted_private, predicted_seconds, run_now, parallel_env, jobserver,
                               launcher_pid, launcher_start, created_at)
             VALUES (?1, 'queued', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                job,
                new.cwd,
                new.project_root,
                new.entry.as_str(),
                new.command,
                serde_json::to_string(&p.profiles)?,
                p.history_key,
                p.prediction.mem as i64,
                p.prediction.seconds,
                p.run_now,
                serde_json::to_string(&p.parallel_env)?,
                p.jobserver,
                new.launcher.as_ref().map(|l| l.0),
                new.launcher.as_ref().map(|l| &l.1),
                at,
            ],
        )?;
        tx.execute(
            "INSERT INTO submissions (id, job_id, session_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![submission, job, new.session_id, at],
        )?;
        for lock in &p.locks {
            let (kind, resource) = match lock {
                Lock::Path(p) => ("path", p),
                Lock::Named(n) => ("named", n),
            };
            tx.execute(
                "INSERT OR IGNORE INTO locks (job_id, resource, kind) VALUES (?1, ?2, ?3)",
                params![job, resource, kind],
            )?;
        }
        for after in &p.after {
            tx.execute(
                "INSERT OR IGNORE INTO dependencies (job_id, after_job_id) VALUES (?1, ?2)",
                params![job, after],
            )?;
        }
        Self::event(&tx, &job, "submitted", None)?;
        tx.commit()?;
        Ok(Created { submission, job })
    }

    /// Lists jobs, newest first, optionally filtered by state.
    pub fn list_jobs(&self, states: &[String], limit: u32) -> Result<Vec<JobRow>> {
        let mut sql = format!("SELECT {JOB_COLUMNS} FROM jobs j");
        if !states.is_empty() {
            let marks = vec!["?"; states.len()].join(",");
            sql.push_str(&format!(" WHERE j.state IN ({marks})"));
        }
        sql.push_str(" ORDER BY j.created_at DESC, j.rowid DESC LIMIT ?");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<&dyn rusqlite::ToSql> =
            states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        values.push(&limit);
        let rows = stmt.query_map(values.as_slice(), job_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn job(&self, id: &str) -> Result<Option<JobRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs j WHERE j.id = ?1"),
                params![id],
                job_row,
            )
            .optional()?)
    }

    /// One submission with its job's state.
    pub fn submission(&self, id: &str) -> Result<Option<SubmissionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.id, s.job_id, j.state, s.session_id, j.exit_code, j.main_exit_code, j.log_path,
                        s.canceled, s.detached, s.quiet
                 FROM submissions s JOIN jobs j ON j.id = s.job_id WHERE s.id = ?1",
                params![id],
                |r| {
                    Ok(SubmissionRow {
                        id: r.get(0)?,
                        job: r.get(1)?,
                        job_state: r.get(2)?,
                        session_id: r.get(3)?,
                        exit_code: r.get(4)?,
                        main_exit_code: r.get(5)?,
                        log_path: r.get(6)?,
                        canceled: r.get(7)?,
                        detached: r.get(8)?,
                        quiet: r.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    fn event(
        tx: &rusqlite::Transaction,
        job: &str,
        kind: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO job_events (job_id, at, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![job, now(), kind, detail],
        )?;
        Ok(())
    }

    /// Updates a job that has not ended and records an event, in one
    /// transaction.
    fn update_job(
        &mut self,
        job: &str,
        set: &str,
        values: &[&dyn rusqlite::ToSql],
        event: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let state: String = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![job], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if is_terminal(&state) {
            return Err(Conflict(format!("job {job} has already ended ({state})")).into());
        }
        let mut all: Vec<&dyn rusqlite::ToSql> = values.to_vec();
        all.push(&job);
        tx.execute(
            &format!("UPDATE jobs SET {set} WHERE id = ?{}", all.len()),
            all.as_slice(),
        )?;
        Self::event(&tx, job, event, detail)?;
        tx.commit()?;
        Ok(())
    }

    /// Records the executor that owns a job. The submitting CLI registers it
    /// right after starting it and the executor registers itself again;
    /// both name the same process. Another process is refused, so a job
    /// never gets two executors.
    pub fn set_executor(&mut self, job: &str, pid: i64, start: &str) -> Result<()> {
        let current: Option<(Option<i64>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT executor_pid, executor_start FROM jobs WHERE id = ?1",
                params![job],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match current.ok_or_else(|| no_job(job))? {
            (Some(p), Some(s)) if p == pid && s == start => return Ok(()),
            (Some(p), _) => {
                return Err(Conflict(format!("job {job} already has executor {p}")).into());
            }
            _ => {}
        }
        self.update_job(
            job,
            "executor_pid = ?1, executor_start = ?2",
            &[&pid, &start],
            "executor",
            Some(&pid.to_string()),
        )
    }

    /// Records that the command started in process group `pgid`, with the
    /// environment the executor added (design 10.5).
    pub fn set_started(&mut self, job: &str, log_path: &str, pgid: Option<i64>) -> Result<()> {
        if self.job(job)?.is_some_and(|j| j.state == "running") {
            return Ok(());
        }
        let at = now();
        self.update_job(
            job,
            "state = 'running', started_at = ?1, log_path = ?2, pgid = ?3",
            &[&at, &log_path, &pgid],
            "started",
            None,
        )
    }

    /// Records the main process result; the job may still be running
    /// (design 5.4).
    pub fn set_main_result(&mut self, job: &str, code: i64) -> Result<()> {
        self.update_job(
            job,
            "main_exit_code = ?1",
            &[&code],
            "main_exited",
            Some(&code.to_string()),
        )
    }

    /// Moves a job to a terminal state. Repeating the same terminal state
    /// is accepted; a different one is a conflict.
    pub fn finish(&mut self, job: &str, state: &str, exit_code: Option<i64>) -> Result<()> {
        if !is_terminal(state) {
            bail!("{state} is not a terminal state");
        }
        if self.job(job)?.is_some_and(|j| j.state == state) {
            return Ok(());
        }
        let at = now();
        self.update_job(
            job,
            "state = ?1, exit_code = ?2, ended_at = ?3",
            &[&state, &exit_code, &at],
            state,
            exit_code.map(|c| c.to_string()).as_deref(),
        )
    }

    /// Marks a job whose executor is gone while its processes still run
    /// (design 7.1). It keeps its reservation until they exit.
    pub fn await_takeover(&mut self, job: &str) -> Result<()> {
        self.update_job(
            job,
            "state = 'awaiting_takeover'",
            &[],
            "executor_lost",
            None,
        )
    }

    /// Keeps each job's measured peak private memory, and raises the
    /// reservation of a job holding a lease to it (design 9).
    pub fn record_peaks(&mut self, peaks: &[(String, u64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (job, peak) in peaks {
            tx.execute(
                &format!(
                    "UPDATE jobs SET peak_private = MAX(COALESCE(peak_private, 0), ?2),
                     reserved_private = CASE WHEN state IN ({}) THEN MAX(reserved_private, ?2) ELSE reserved_private END
                     WHERE id = ?1",
                    sched::in_list(HELD)
                ),
                params![job, *peak as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The largest resident size an executor reports at the end. It also
    /// counts shared and file pages, so it only fills in for a job too short
    /// to be sampled, as an upper bound (design 8.3).
    pub fn record_exit_usage(&mut self, job: &str, max_rss: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET peak_private = COALESCE(peak_private, ?2) WHERE id = ?1",
            params![job, max_rss],
        )?;
        Ok(())
    }

    /// Jobs that have not ended, with what recovery needs.
    pub fn active_jobs(&self) -> Result<Vec<ActiveJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, state, executor_pid, executor_start, pgid, launcher_pid, launcher_start, main_exit_code,
                    NOT EXISTS (SELECT 1 FROM submissions s WHERE s.job_id = jobs.id AND s.canceled = 0)
             FROM jobs WHERE state NOT IN ('completed', 'start_failed', 'canceled', 'ended_unknown')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ActiveJob {
                id: r.get(0)?,
                state: r.get(1)?,
                executor: process(r.get(2)?, r.get(3)?),
                pgid: r.get(4)?,
                launcher: process(r.get(5)?, r.get(6)?),
                main_exit_code: r.get(7)?,
                canceled: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn update_submission(&mut self, id: &str, set: &str) -> Result<String> {
        let job: String = self
            .conn
            .query_row(
                "SELECT job_id FROM submissions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| no_submission(id))?;
        self.conn.execute(
            &format!("UPDATE submissions SET {set} WHERE id = ?1"),
            params![id],
        )?;
        Ok(job)
    }

    /// Cancels a submission. Returns the job id. A job left with no
    /// uncanceled submission ends here while it waits; once running, its
    /// executor stops it.
    pub fn cancel_submission(&mut self, id: &str) -> Result<String> {
        let job = self.update_submission(id, "canceled = 1")?;
        if self.cancel_requested(&job)?
            && self
                .job(&job)?
                .is_some_and(|j| PRE_LEASE.contains(&j.state.as_str()))
        {
            self.finish(&job, "canceled", None)?;
        }
        Ok(job)
    }

    pub fn quiet_submission(&mut self, id: &str) -> Result<String> {
        self.update_submission(id, "quiet = 1")
    }

    pub fn detach_submission(&mut self, id: &str) -> Result<String> {
        self.update_submission(id, "detached = 1")
    }

    /// Running submissions the agent did not detach, or ended ones not yet
    /// reported.
    fn reminders(conn: &Connection, session: &str, ended: bool) -> Result<Vec<Reminder>> {
        let (open, state) = if ended {
            ("s.quiet = 0", "")
        } else {
            ("s.detached = 0", "NOT")
        };
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT s.id, s.job_id, j.state, j.exit_code, j.command FROM submissions s JOIN jobs j ON j.id = s.job_id
             WHERE s.session_id = ?1 AND {open} AND s.canceled = 0 AND j.state {state} IN ({})
             ORDER BY s.created_at",
            sched::in_list(TERMINAL)
        ))?;
        let rows = stmt.query_map(params![session], |r| {
            Ok(Reminder {
                submission: r.get(0)?,
                job: r.get(1)?,
                state: r.get(2)?,
                exit_code: r.get(3)?,
                command: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A session's submissions that have not ended and are not detached.
    pub fn running_reminders(&self, session: &str) -> Result<Vec<Reminder>> {
        Self::reminders(&self.conn, session, false)
    }

    /// Deletes jobs that ended before `before` (RFC 3339) with their
    /// submissions and records, keeping predecessors of jobs that have not
    /// ended (design 11). Returns the log directories of the deleted jobs.
    pub fn prune(&mut self, before: &str) -> Result<Vec<(String, Option<String>)>> {
        let terminal = sched::in_list(TERMINAL);
        let tx = self.conn.transaction()?;
        let old: Vec<(String, Option<String>)> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id, log_path FROM jobs j WHERE state IN ({terminal}) AND ended_at < ?1
                 AND NOT EXISTS (SELECT 1 FROM dependencies d JOIN jobs k ON k.id = d.job_id
                                 WHERE d.after_job_id = j.id AND k.state NOT IN ({terminal}))"
            ))?;
            let rows = stmt.query_map(params![before], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (job, _) in &old {
            for sql in [
                "DELETE FROM job_events WHERE job_id = ?1",
                "DELETE FROM locks WHERE job_id = ?1",
                "DELETE FROM leases WHERE job_id = ?1",
                "DELETE FROM dependencies WHERE job_id = ?1 OR after_job_id = ?1",
                "DELETE FROM submissions WHERE job_id = ?1",
                "DELETE FROM jobs WHERE id = ?1",
            ] {
                tx.execute(sql, params![job])?;
            }
        }
        tx.commit()?;
        Ok(old)
    }

    /// A session's ended submissions that are not quiet; they are quiet
    /// afterwards, so each is reported once.
    pub fn take_ended_reminders(&mut self, session: &str) -> Result<Vec<Reminder>> {
        let tx = self.conn.transaction()?;
        let ended = Self::reminders(&tx, session, true)?;
        for r in &ended {
            tx.execute(
                "UPDATE submissions SET quiet = 1 WHERE id = ?1",
                params![r.submission],
            )?;
        }
        tx.commit()?;
        Ok(ended)
    }

    /// True when every submission of the job is canceled, so the job
    /// should stop.
    pub fn cancel_requested(&self, job: &str) -> Result<bool> {
        let open: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM submissions WHERE job_id = ?1 AND canceled = 0",
            params![job],
            |r| r.get(0),
        )?;
        Ok(open == 0)
    }
}

/// Terminal job states (design 5.1).
pub const TERMINAL: &[&str] = &["completed", "start_failed", "canceled", "ended_unknown"];

pub fn is_terminal(state: &str) -> bool {
    TERMINAL.contains(&state)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(command: &str) -> NewSubmission {
        NewSubmission {
            entry: Entry::Sh,
            command: command.into(),
            cwd: "/tmp/p".into(),
            project_root: "/tmp/p".into(),
            session_id: Some("sess".into()),
            launcher: None,
            plan: Plan::default(),
        }
    }

    #[test]
    fn creates_and_reads_back() {
        let mut store = Store::open_in_memory().unwrap();
        let c = store.create_submission(&sample("cargo build")).unwrap();
        let jobs = store.list_jobs(&[], 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (
                jobs[0].command.as_str(),
                jobs[0].state.as_str(),
                jobs[0].submissions
            ),
            ("cargo build", "queued", 1)
        );
        let s = store.submission(&c.submission).unwrap().unwrap();
        assert_eq!((s.job, s.job_state.as_str()), (c.job, "queued"));
        assert!(store.submission("s_missing").unwrap().is_none());
    }

    #[test]
    fn peaks_only_grow() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample("make")).unwrap().job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 400, 1).unwrap();
        store.record_peaks(&[(job.clone(), 500)]).unwrap();
        store.record_peaks(&[(job.clone(), 300)]).unwrap();
        store.record_exit_usage(&job, 900).unwrap();
        assert_eq!(
            store.job(&job).unwrap().unwrap().peak_private,
            Some(500),
            "a sampled peak is kept"
        );
        assert_eq!(
            store.held_jobs().unwrap()[0].reserved_private,
            500,
            "the reservation follows the peak"
        );
    }

    #[test]
    fn reminders_name_each_submission_until_it_is_quiet() {
        let mut store = Store::open_in_memory().unwrap();
        let mut submit = |session: Option<&str>| {
            let mut new = sample("make");
            new.session_id = session.map(str::to_string);
            store.create_submission(&new).unwrap()
        };
        let (running, ended, delivered, canceled, detached) = (
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
            submit(Some("s")),
        );
        submit(Some("another session"));
        store.detach_submission(&detached.submission).unwrap();
        store.finish(&ended.job, "completed", Some(0)).unwrap();
        store.finish(&delivered.job, "completed", Some(1)).unwrap();
        store.quiet_submission(&delivered.submission).unwrap();
        store.cancel_submission(&canceled.submission).unwrap();
        let ids = |r: Vec<Reminder>| r.into_iter().map(|r| r.submission).collect::<Vec<_>>();
        assert_eq!(
            ids(store.running_reminders("s").unwrap()),
            std::slice::from_ref(&running.submission)
        );
        assert_eq!(
            ids(store.take_ended_reminders("s").unwrap()),
            std::slice::from_ref(&ended.submission)
        );
        assert!(
            store.take_ended_reminders("s").unwrap().is_empty(),
            "reported once"
        );
        // A detached submission does not hold Stop, but its end is reported.
        store.finish(&detached.job, "completed", Some(0)).unwrap();
        assert_eq!(
            ids(store.take_ended_reminders("s").unwrap()),
            std::slice::from_ref(&detached.submission)
        );
        assert!(store.submission(&ended.submission).unwrap().unwrap().quiet);
    }

    #[test]
    fn prunes_old_jobs_but_keeps_awaited_predecessors() {
        let mut store = Store::open_in_memory().unwrap();
        let mut job = |after: &[&str]| {
            let mut new = sample("make");
            new.plan.after = after.iter().map(|s| s.to_string()).collect();
            store.create_submission(&new).unwrap().job
        };
        let (old, recent, awaited) = (job(&[]), job(&[]), job(&[]));
        let waiting = job(&[&awaited]);
        for j in [&old, &recent, &awaited] {
            store.finish(j, "completed", Some(0)).unwrap();
        }
        for j in [&old, &awaited] {
            store
                .conn
                .execute(
                    "UPDATE jobs SET ended_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                    params![j],
                )
                .unwrap();
        }
        let pruned: Vec<String> = store
            .prune("2026-07-01T00:00:00.000Z")
            .unwrap()
            .into_iter()
            .map(|(j, _)| j)
            .collect();
        assert_eq!(pruned, std::slice::from_ref(&old));
        assert!(store.job(&old).unwrap().is_none());
        for j in [&recent, &awaited, &waiting] {
            assert!(store.job(j).unwrap().is_some());
        }
    }

    #[test]
    fn filters_by_state() {
        let mut store = Store::open_in_memory().unwrap();
        store.create_submission(&sample("a")).unwrap();
        assert_eq!(store.list_jobs(&["queued".into()], 10).unwrap().len(), 1);
        assert!(store.list_jobs(&["running".into()], 10).unwrap().is_empty());
    }

    #[test]
    fn upgrades_a_version_1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let old = SCHEMA.replace(
            "    detached            INTEGER NOT NULL DEFAULT 0, -- Stop does not wait for it\n",
            "",
        );
        assert_ne!(old, SCHEMA);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&old).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let id = store.create_submission(&sample("make")).unwrap().submission;
        store.detach_submission(&id).unwrap();
        assert!(store.submission(&id).unwrap().unwrap().detached);
    }

    #[test]
    fn survives_reopen_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let id = Store::open(&path)
            .unwrap()
            .create_submission(&sample("a"))
            .unwrap()
            .submission;
        assert!(
            Store::open(&path)
                .unwrap()
                .submission(&id)
                .unwrap()
                .is_some()
        );
    }
}
