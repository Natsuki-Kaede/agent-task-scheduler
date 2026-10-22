//! The daemon's SQLite store (design 13). Only the daemon opens it; writes
//! are committed before the daemon answers (design 7.3).

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 1;

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
    pub agent: Option<String>,
    pub invocation_key: Option<String>,
    pub request_id: Option<String>,
    pub sandbox_signature: Option<String>,
    pub service: bool,
    /// Options as JSON, kept for status and recovery.
    pub options: serde_json::Value,
    /// The process that will start the executor, as (pid, start time).
    pub launcher: Option<(i64, String)>,
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
    pub state: String,
    /// True when an earlier submission with the same request id was found.
    pub existing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: String,
    pub state: String,
    pub entry: String,
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub service: bool,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub executor_pid: Option<i64>,
    /// Peak private memory measured so far, in bytes.
    pub peak_private: Option<i64>,
    pub cpu_seconds: Option<f64>,
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
    pub agent: Option<String>,
    pub request_id: Option<String>,
    pub exit_code: Option<i64>,
    pub main_exit_code: Option<i64>,
    pub log_path: Option<String>,
    pub canceled: bool,
    pub detached: bool,
    pub final_retrieved: bool,
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
}

fn process(pid: Option<i64>, start: Option<String>) -> Option<(i64, String)> {
    pid.zip(start)
}

/// A 5-second summary of a job's measured usage (design 9.2).
#[derive(Debug, Clone, PartialEq)]
pub struct JobSample {
    pub job: String,
    pub private_resident: u64,
    pub private_total: u64,
    pub peak_private_total: u64,
    pub cpu_seconds: f64,
    pub disk_read: u64,
    pub disk_write: u64,
}

/// A running total of awake time a job spends in a state (design 9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalTotal {
    pub awake_ns: i64,
    /// Part of the time could not be counted, for example across a reboot.
    pub uncertain: bool,
}

/// A request id that is already bound to a different command.
#[derive(Debug, thiserror::Error)]
#[error("request id {0} is already used by a different command")]
pub struct RequestIdConflict(pub String);

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
            SCHEMA_VERSION => {}
            v => bail!("database schema version {v} is newer than this ats ({SCHEMA_VERSION})"),
        }
        Ok(Self { conn })
    }

    /// Registers a submission with a new queued job, in one transaction.
    /// A repeated request id returns the earlier submission when the command
    /// matches, and fails with [`RequestIdConflict`] when it differs.
    pub fn create_submission(&mut self, new: &NewSubmission) -> Result<Created> {
        let tx = self.conn.transaction()?;
        if let Some(rid) = &new.request_id {
            let found: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT s.id, j.id, j.state, j.command FROM submissions s JOIN jobs j ON j.id = s.job_id
                     WHERE s.request_id = ?1",
                    params![rid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((submission, job, state, command)) = found {
                if command != new.command {
                    return Err(RequestIdConflict(rid.clone()).into());
                }
                return Ok(Created {
                    submission,
                    job,
                    state,
                    existing: true,
                });
            }
        }
        let job = crate::ids::new("j");
        let submission = crate::ids::new("s");
        let at = now();
        tx.execute(
            "INSERT INTO jobs (id, node_id, state, cwd, project_root, entry, command, service,
                               launcher_pid, launcher_start, created_at)
             VALUES (?1, 'local', 'queued', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                job,
                new.cwd,
                new.project_root,
                new.entry.as_str(),
                new.command,
                new.service,
                new.launcher.as_ref().map(|l| l.0),
                new.launcher.as_ref().map(|l| &l.1),
                at
            ],
        )?;
        tx.execute(
            "INSERT INTO submissions (id, job_id, session_id, agent, invocation_key, request_id,
                                      sandbox_signature, options, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                submission,
                job,
                new.session_id,
                new.agent,
                new.invocation_key,
                new.request_id,
                new.sandbox_signature,
                new.options.to_string(),
                at
            ],
        )?;
        tx.execute(
            "INSERT INTO job_events (job_id, at, kind) VALUES (?1, ?2, 'submitted')",
            params![job, at],
        )?;
        tx.commit()?;
        Ok(Created {
            submission,
            job,
            state: "queued".into(),
            existing: false,
        })
    }

    /// Lists jobs, newest first, optionally filtered by state.
    pub fn list_jobs(&self, states: &[String], limit: u32) -> Result<Vec<JobRow>> {
        let mut sql = String::from(
            "SELECT j.id, j.state, j.entry, j.command, j.cwd, j.project_root, j.service, j.exit_code,
                    j.main_exit_code, j.log_path, j.executor_pid, j.peak_private, j.cpu_seconds,
                    (SELECT COUNT(*) FROM submissions s WHERE s.job_id = j.id),
                    j.created_at, j.started_at, j.ended_at
             FROM jobs j",
        );
        if !states.is_empty() {
            let marks = vec!["?"; states.len()].join(",");
            sql.push_str(&format!(" WHERE j.state IN ({marks})"));
        }
        sql.push_str(" ORDER BY j.created_at DESC, j.rowid DESC LIMIT ?");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<&dyn rusqlite::ToSql> =
            states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        values.push(&limit);
        let rows = stmt.query_map(values.as_slice(), |r| {
            Ok(JobRow {
                id: r.get(0)?,
                state: r.get(1)?,
                entry: r.get(2)?,
                command: r.get(3)?,
                cwd: r.get(4)?,
                project_root: r.get(5)?,
                service: r.get(6)?,
                exit_code: r.get(7)?,
                main_exit_code: r.get(8)?,
                log_path: r.get(9)?,
                executor_pid: r.get(10)?,
                peak_private: r.get(11)?,
                cpu_seconds: r.get(12)?,
                submissions: r.get(13)?,
                created_at: r.get(14)?,
                started_at: r.get(15)?,
                ended_at: r.get(16)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns one submission with its job's state.
    pub fn submission(&self, id: &str) -> Result<Option<SubmissionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.id, s.job_id, j.state, s.session_id, s.agent, s.request_id, j.exit_code,
                        j.main_exit_code, j.log_path, s.canceled, s.detached, s.final_retrieved
                 FROM submissions s JOIN jobs j ON j.id = s.job_id WHERE s.id = ?1",
                params![id],
                |r| {
                    Ok(SubmissionRow {
                        id: r.get(0)?,
                        job: r.get(1)?,
                        job_state: r.get(2)?,
                        session_id: r.get(3)?,
                        agent: r.get(4)?,
                        request_id: r.get(5)?,
                        exit_code: r.get(6)?,
                        main_exit_code: r.get(7)?,
                        log_path: r.get(8)?,
                        canceled: r.get(9)?,
                        detached: r.get(10)?,
                        final_retrieved: r.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    /// Returns one job.
    pub fn job(&self, id: &str) -> Result<Option<JobRow>> {
        Ok(self
            .list_jobs_where("j.id = ?", &[&id as &dyn rusqlite::ToSql])?
            .into_iter()
            .next())
    }

    fn list_jobs_where(&self, cond: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Vec<JobRow>> {
        let sql = format!(
            "SELECT j.id, j.state, j.entry, j.command, j.cwd, j.project_root, j.service, j.exit_code,
                    j.main_exit_code, j.log_path, j.executor_pid, j.peak_private, j.cpu_seconds,
                    (SELECT COUNT(*) FROM submissions s WHERE s.job_id = j.id),
                    j.created_at, j.started_at, j.ended_at
             FROM jobs j WHERE {cond}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(values, |r| {
            Ok(JobRow {
                id: r.get(0)?,
                state: r.get(1)?,
                entry: r.get(2)?,
                command: r.get(3)?,
                cwd: r.get(4)?,
                project_root: r.get(5)?,
                service: r.get(6)?,
                exit_code: r.get(7)?,
                main_exit_code: r.get(8)?,
                log_path: r.get(9)?,
                executor_pid: r.get(10)?,
                peak_private: r.get(11)?,
                cpu_seconds: r.get(12)?,
                submissions: r.get(13)?,
                created_at: r.get(14)?,
                started_at: r.get(15)?,
                ended_at: r.get(16)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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

    /// Updates a job that is not in a terminal state and records an event,
    /// in one transaction.
    fn update_job(
        &mut self,
        job: &str,
        set: &str,
        values: &[&dyn rusqlite::ToSql],
        event: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let state = Self::state_in(&tx, job)?;
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

    fn state_in(tx: &rusqlite::Transaction, job: &str) -> Result<String> {
        tx.query_row("SELECT state FROM jobs WHERE id = ?1", params![job], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| no_job(job))
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

    /// Persists a lease and moves the job to admitted (design 7.3: the
    /// lease is committed before the executor is told). Asking again after
    /// a lost answer gets the same lease.
    pub fn grant_lease(&mut self, job: &str, pid: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (state, executor): (String, Option<i64>) = tx
            .query_row(
                "SELECT state, executor_pid FROM jobs WHERE id = ?1",
                params![job],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if executor != Some(pid) {
            return Err(Conflict(format!("process {pid} is not the executor of job {job}")).into());
        }
        match state.as_str() {
            "queued" => {}
            "admitted" => return Ok(()),
            _ => return Err(Conflict(format!("job {job} is {state}, not queued")).into()),
        }
        tx.execute(
            "INSERT INTO leases (job_id, granted_at) VALUES (?1, ?2)",
            params![job, now()],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'admitted' WHERE id = ?1",
            params![job],
        )?;
        Self::event(&tx, job, "admitted", None)?;
        tx.commit()?;
        Ok(())
    }

    /// Records that the command started in process group `pgid`.
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

    /// Records the main execution process result; the job may still be
    /// running (design 5.5).
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
            "state = ?1, exit_code = ?2, ended_at = ?3, merge_fingerprint = NULL",
            &[&state, &exit_code, &at],
            state,
            exit_code.map(|c| c.to_string()).as_deref(),
        )
    }

    /// Marks a job whose executor is gone while its processes still run
    /// (design 7.3 ⑦). It keeps its lease until they exit.
    pub fn await_takeover(&mut self, job: &str) -> Result<()> {
        self.update_job(
            job,
            "state = 'awaiting_takeover'",
            &[],
            "executor_lost",
            None,
        )
    }

    /// Stores summaries and keeps each job's peak private memory and CPU
    /// time, in one transaction.
    pub fn record_samples(&mut self, samples: &[JobSample]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let at = now();
        for s in samples {
            tx.execute(
                "INSERT INTO job_samples (job_id, at, private_resident, private_total, cpu_seconds, disk_read, disk_write)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![s.job, at, s.private_resident as i64, s.private_total as i64, s.cpu_seconds, s.disk_read as i64, s.disk_write as i64],
            )?;
            tx.execute(
                "UPDATE jobs SET peak_private = MAX(COALESCE(peak_private, 0), ?2), cpu_seconds = MAX(COALESCE(cpu_seconds, 0), ?3)
                 WHERE id = ?1",
                params![s.job, s.peak_private_total as i64, s.cpu_seconds],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records the usage an executor reports at the end. CPU time is exact
    /// for processes that were waited for. The largest resident size also
    /// counts shared and file pages, so it only fills in for a job too short
    /// to be sampled, as an upper bound.
    pub fn record_exit_usage(
        &mut self,
        job: &str,
        cpu_seconds: f64,
        max_rss: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET cpu_seconds = MAX(COALESCE(cpu_seconds, 0), ?2),
                             peak_private = COALESCE(peak_private, ?3)
             WHERE id = ?1",
            params![job, cpu_seconds, max_rss],
        )?;
        Ok(())
    }

    /// Starts counting awake time in `kind` (frozen or throttled), if not
    /// already counting. Readings are only comparable within one boot.
    #[allow(dead_code)] // used by the freeze alert (T9)
    pub fn open_interval(
        &mut self,
        job: &str,
        kind: &str,
        awake_ns: i64,
        boot: &str,
        instance: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO time_intervals (job_id, kind, open_since_awake_ns, open_state, boot_id, daemon_instance)
             VALUES (?1, ?2, ?3, 'open', ?4, ?5)
             ON CONFLICT (job_id, kind) DO UPDATE SET
                 open_since_awake_ns = excluded.open_since_awake_ns, open_state = 'open',
                 boot_id = excluded.boot_id, daemon_instance = excluded.daemon_instance
             WHERE open_since_awake_ns IS NULL",
            params![job, kind, awake_ns, boot, instance],
        )?;
        Ok(())
    }

    /// Stops counting and adds the open part to the settled total. A part
    /// opened in another boot cannot be measured and marks the total as
    /// uncertain instead of being dropped silently.
    #[allow(dead_code)] // used by the freeze alert (T9)
    pub fn close_interval(
        &mut self,
        job: &str,
        kind: &str,
        awake_ns: i64,
        boot: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE time_intervals SET
                 settled_awake_ns = settled_awake_ns
                     + CASE WHEN boot_id = ?4 THEN MAX(0, ?3 - open_since_awake_ns) ELSE 0 END,
                 uncertain = uncertain OR boot_id IS NOT ?4,
                 open_since_awake_ns = NULL, open_state = NULL
             WHERE job_id = ?1 AND kind = ?2 AND open_since_awake_ns IS NOT NULL",
            params![job, kind, awake_ns, boot],
        )?;
        Ok(())
    }

    /// The settled total plus the open part, as of `awake_ns`.
    #[allow(dead_code)] // used by the freeze alert (T9)
    pub fn interval_total(
        &self,
        job: &str,
        kind: &str,
        awake_ns: i64,
        boot: &str,
    ) -> Result<Option<IntervalTotal>> {
        Ok(self
            .conn
            .query_row(
                "SELECT settled_awake_ns
                        + CASE WHEN open_since_awake_ns IS NOT NULL AND boot_id = ?4
                               THEN MAX(0, ?3 - open_since_awake_ns) ELSE 0 END,
                        uncertain OR (open_since_awake_ns IS NOT NULL AND boot_id IS NOT ?4)
                 FROM time_intervals WHERE job_id = ?1 AND kind = ?2",
                params![job, kind, awake_ns, boot],
                |r| {
                    Ok(IntervalTotal {
                        awake_ns: r.get(0)?,
                        uncertain: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// At daemon start: open intervals from this boot keep counting under
    /// the new daemon instance; those from an earlier boot are closed as
    /// uncertain, since the clock they were read from restarted. Returns how
    /// many were closed.
    pub fn recover_intervals(&mut self, boot: &str, instance: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE time_intervals SET daemon_instance = ?2
             WHERE open_since_awake_ns IS NOT NULL AND boot_id = ?1",
            params![boot, instance],
        )?;
        let closed = tx.execute(
            "UPDATE time_intervals SET uncertain = 1, open_since_awake_ns = NULL, open_state = NULL
             WHERE open_since_awake_ns IS NOT NULL AND boot_id IS NOT ?1",
            params![boot],
        )?;
        tx.commit()?;
        Ok(closed)
    }

    /// Jobs that have not reached a terminal state, with what recovery needs.
    pub fn active_jobs(&self) -> Result<Vec<ActiveJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, state, executor_pid, executor_start, pgid, launcher_pid, launcher_start,
                    main_exit_code
             FROM jobs WHERE state NOT IN (
                 'completed', 'start_failed', 'canceled', 'replaced',
                 'externally_terminated', 'ended_unknown')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ActiveJob {
                id: r.get(0)?,
                state: r.get(1)?,
                executor: process(r.get(2)?, r.get(3)?),
                pgid: r.get(4)?,
                launcher: process(r.get(5)?, r.get(6)?),
                main_exit_code: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn update_submission(&mut self, id: &str, set: &str) -> Result<String> {
        let tx = self.conn.transaction()?;
        let job: String = tx
            .query_row(
                "SELECT job_id FROM submissions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| no_submission(id))?;
        tx.execute(
            &format!("UPDATE submissions SET {set} WHERE id = ?1"),
            params![id],
        )?;
        tx.commit()?;
        Ok(job)
    }

    /// Cancels a submission. Returns the job id. A job left with no
    /// uncanceled submission ends here while still queued; once running,
    /// its executor stops it.
    pub fn cancel_submission(&mut self, id: &str) -> Result<String> {
        let job = self.update_submission(id, "canceled = 1")?;
        if self.cancel_requested(&job)? && self.job(&job)?.is_some_and(|j| j.state == "queued") {
            self.finish(&job, "canceled", None)?;
        }
        Ok(job)
    }

    pub fn detach_submission(&mut self, id: &str) -> Result<String> {
        self.update_submission(id, "detached = 1")
    }

    /// Records that the submission's terminal state reached its caller.
    pub fn mark_retrieved(&mut self, id: &str) -> Result<String> {
        let row = self.submission(id)?.ok_or_else(|| no_submission(id))?;
        if !is_terminal(&row.job_state) {
            return Err(Conflict(format!(
                "job {} is {}, not in a terminal state",
                row.job, row.job_state
            ))
            .into());
        }
        self.update_submission(id, "final_retrieved = 1")
    }

    /// Finds the submission registered under a caller's request id.
    pub fn submission_by_request(&self, request_id: &str) -> Result<Option<SubmissionRow>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM submissions WHERE request_id = ?1",
                params![request_id],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.submission(&id),
            None => Ok(None),
        }
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
pub fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "completed"
            | "start_failed"
            | "canceled"
            | "replaced"
            | "externally_terminated"
            | "ended_unknown"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(request_id: Option<&str>, command: &str) -> NewSubmission {
        NewSubmission {
            entry: Entry::Sh,
            command: command.into(),
            cwd: "/tmp/p".into(),
            project_root: "/tmp/p".into(),
            session_id: Some("sess".into()),
            agent: Some("claude".into()),
            invocation_key: None,
            request_id: request_id.map(str::to_string),
            sandbox_signature: None,
            service: false,
            options: serde_json::json!({}),
            launcher: None,
        }
    }

    #[test]
    fn intervals_count_awake_time_and_flag_reboots() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample(None, "make")).unwrap().job;
        let total = |s: &Store, now, boot| {
            s.interval_total(&job, "frozen", now, boot)
                .unwrap()
                .unwrap()
        };

        store
            .open_interval(&job, "frozen", 1_000, "b1", "d1")
            .unwrap();
        store
            .open_interval(&job, "frozen", 1_500, "b1", "d1")
            .unwrap(); // already open
        assert_eq!(
            total(&store, 3_000, "b1"),
            IntervalTotal {
                awake_ns: 2_000,
                uncertain: false
            }
        );
        store.close_interval(&job, "frozen", 4_000, "b1").unwrap();
        assert_eq!(total(&store, 9_000, "b1").awake_ns, 3_000);

        // A daemon restart in the same boot keeps counting.
        store
            .open_interval(&job, "frozen", 10_000, "b1", "d1")
            .unwrap();
        assert_eq!(store.recover_intervals("b1", "d2").unwrap(), 0);
        store.close_interval(&job, "frozen", 11_000, "b1").unwrap();
        assert_eq!(
            total(&store, 0, "b1"),
            IntervalTotal {
                awake_ns: 4_000,
                uncertain: false
            }
        );

        // Across a reboot the open part is unknown: kept as uncertain.
        store
            .open_interval(&job, "frozen", 20_000, "b1", "d2")
            .unwrap();
        assert_eq!(
            total(&store, 5, "b2"),
            IntervalTotal {
                awake_ns: 4_000,
                uncertain: true
            }
        );
        assert_eq!(store.recover_intervals("b2", "d3").unwrap(), 1);
        assert_eq!(
            total(&store, 5, "b2"),
            IntervalTotal {
                awake_ns: 4_000,
                uncertain: true
            }
        );
    }

    #[test]
    fn samples_keep_the_peak() {
        let mut store = Store::open_in_memory().unwrap();
        let job = store.create_submission(&sample(None, "make")).unwrap().job;
        let s = |peak, cpu| JobSample {
            job: job.clone(),
            private_resident: 1,
            private_total: 1,
            peak_private_total: peak,
            cpu_seconds: cpu,
            disk_read: 0,
            disk_write: 0,
        };
        store.record_samples(&[s(500, 1.0)]).unwrap();
        store.record_samples(&[s(300, 2.5)]).unwrap();
        let row = store.job(&job).unwrap().unwrap();
        assert_eq!((row.peak_private, row.cpu_seconds), (Some(500), Some(2.5)));
    }

    #[test]
    fn creates_and_reads_back() {
        let mut store = Store::open_in_memory().unwrap();
        let c = store
            .create_submission(&sample(None, "cargo build"))
            .unwrap();
        assert_eq!(c.state, "queued");
        let jobs = store.list_jobs(&[], 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].command, "cargo build");
        assert_eq!(jobs[0].submissions, 1);
        let s = store.submission(&c.submission).unwrap().unwrap();
        assert_eq!(s.job, c.job);
        assert_eq!(s.job_state, "queued");
        assert!(store.submission("s_missing").unwrap().is_none());
    }

    #[test]
    fn request_id_is_idempotent_and_conflicts_on_different_command() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .create_submission(&sample(Some("r1"), "make"))
            .unwrap();
        let b = store
            .create_submission(&sample(Some("r1"), "make"))
            .unwrap();
        assert!(b.existing);
        assert_eq!(a.submission, b.submission);
        let err = store
            .create_submission(&sample(Some("r1"), "make test"))
            .unwrap_err();
        assert!(err.downcast_ref::<RequestIdConflict>().is_some());
    }

    #[test]
    fn filters_by_state() {
        let mut store = Store::open_in_memory().unwrap();
        store.create_submission(&sample(None, "a")).unwrap();
        assert_eq!(store.list_jobs(&["queued".into()], 10).unwrap().len(), 1);
        assert!(store.list_jobs(&["running".into()], 10).unwrap().is_empty());
    }

    #[test]
    fn survives_reopen_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ats.db");
        let id = {
            let mut store = Store::open(&path).unwrap();
            store
                .create_submission(&sample(None, "a"))
                .unwrap()
                .submission
        };
        let store = Store::open(&path).unwrap();
        assert!(store.submission(&id).unwrap().is_some());
    }
}
