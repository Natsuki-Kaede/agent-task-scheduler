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

/// A request id that is already bound to a different command.
#[derive(Debug, thiserror::Error)]
#[error("request id {0} is already used by a different command")]
pub struct RequestIdConflict(pub String);

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
            "INSERT INTO jobs (id, node_id, state, cwd, project_root, entry, command, service, created_at)
             VALUES (?1, 'local', 'queued', ?2, ?3, ?4, ?5, ?6, ?7)",
            params![job, new.cwd, new.project_root, new.entry.as_str(), new.command, new.service, at],
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
                    j.main_exit_code, j.log_path,
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
                submissions: r.get(10)?,
                created_at: r.get(11)?,
                started_at: r.get(12)?,
                ended_at: r.get(13)?,
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
        }
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
