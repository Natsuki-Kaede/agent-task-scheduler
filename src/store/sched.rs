//! Store operations for admission and ordering (design 10).

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Conflict, Store, is_terminal, no_job, now, process};
use crate::sched::locks::Lock;

/// States before a lease: the job has not run.
pub const PRE_LEASE: &[&str] = &["queued", "blocked", "capacity_insufficient"];
/// States holding a lease, a reservation and locks.
pub const HELD: &[&str] = &[
    "admitted",
    "running",
    "throttled",
    "frozen",
    "awaiting_takeover",
];

fn in_list(states: &[&str]) -> String {
    states
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// A predecessor named by `--after`, as its dependents see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predecessor {
    pub job: String,
    pub state: String,
    pub exit_code: Option<i64>,
}

impl Predecessor {
    /// Only completion with exit code 0 satisfies `--after` (design 5.1).
    pub fn succeeded(&self) -> bool {
        self.state == "completed" && self.exit_code == Some(0)
    }

    pub fn failed(&self) -> bool {
        is_terminal(&self.state) && !self.succeeded()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueuedJob {
    pub id: String,
    pub state: String,
    pub created_at: String,
    pub predicted_private: u64,
    pub predicted_extra: u64,
    pub predicted_cores: f64,
    pub predicted_seconds: Option<f64>,
    pub run_now: bool,
    pub urgency: i8,
    pub pinned: Option<i64>,
    pub executor: Option<(i64, String)>,
    pub locks: Vec<Lock>,
    pub deps: Vec<Predecessor>,
    /// Sessions of its uncanceled submissions.
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeldJob {
    pub id: String,
    pub reserved_private: u64,
    pub extra: u64,
    pub predicted_cores: f64,
    pub predicted_seconds: Option<f64>,
    pub started_at: Option<String>,
    pub locks: Vec<Lock>,
}

fn lock_from(kind: String, resource: String) -> Lock {
    if kind == "path" {
        Lock::Path(resource)
    } else {
        Lock::Named(resource)
    }
}

impl Store {
    fn locks_of(&self, job: &str) -> Result<Vec<Lock>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT kind, resource FROM locks WHERE job_id = ?1")?;
        let rows = stmt.query_map(params![job], |r| Ok(lock_from(r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn predecessors(&self, job: &str) -> Result<Vec<Predecessor>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.after_job_id, j.state, j.exit_code FROM dependencies d JOIN jobs j ON j.id = d.after_job_id
             WHERE d.job_id = ?1",
        )?;
        let rows = stmt.query_map(params![job], |r| {
            Ok(Predecessor {
                job: r.get(0)?,
                state: r.get(1)?,
                exit_code: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Jobs waiting for a lease, oldest first.
    pub fn queued_jobs(&self) -> Result<Vec<QueuedJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, state, created_at, predicted_private, predicted_extra, predicted_cores, predicted_seconds,
                    run_now, urgency, pinned, executor_pid, executor_start
             FROM jobs WHERE state IN ({}) ORDER BY created_at, rowid",
            in_list(PRE_LEASE)
        ))?;
        let rows: Vec<QueuedJob> = stmt
            .query_map([], |r| {
                Ok(QueuedJob {
                    id: r.get(0)?,
                    state: r.get(1)?,
                    created_at: r.get(2)?,
                    predicted_private: r.get::<_, i64>(3)? as u64,
                    predicted_extra: r.get::<_, i64>(4)? as u64,
                    predicted_cores: r.get::<_, Option<f64>>(5)?.unwrap_or(1.0),
                    predicted_seconds: r.get(6)?,
                    run_now: r.get(7)?,
                    urgency: r.get(8)?,
                    pinned: r.get(9)?,
                    executor: process(r.get(10)?, r.get(11)?),
                    locks: Vec::new(),
                    deps: Vec::new(),
                    sessions: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for mut j in rows {
            j.locks = self.locks_of(&j.id)?;
            j.deps = self.predecessors(&j.id)?;
            let mut stmt = self
                .conn
                .prepare_cached("SELECT session_id FROM submissions WHERE job_id = ?1 AND canceled = 0 AND session_id IS NOT NULL")?;
            j.sessions = stmt
                .query_map(params![j.id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            out.push(j);
        }
        Ok(out)
    }

    /// Jobs holding a lease.
    pub fn held_jobs(&self) -> Result<Vec<HeldJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT j.id, j.reserved_private, j.predicted_cores, j.predicted_seconds, j.started_at,
                    COALESCE((SELECT SUM(bytes) FROM budget_records b WHERE b.owner_kind = 'job' AND b.owner_id = j.id), 0)
             FROM jobs j WHERE j.state IN ({})",
            in_list(HELD)
        ))?;
        let rows: Vec<HeldJob> = stmt
            .query_map([], |r| {
                Ok(HeldJob {
                    id: r.get(0)?,
                    reserved_private: r.get::<_, i64>(1)? as u64,
                    predicted_cores: r.get::<_, Option<f64>>(2)?.unwrap_or(1.0),
                    predicted_seconds: r.get(3)?,
                    started_at: r.get(4)?,
                    extra: r.get::<_, i64>(5)? as u64,
                    locks: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|mut h| {
                h.locks = self.locks_of(&h.id)?;
                Ok(h)
            })
            .collect()
    }

    /// Commits a lease with its reservation and budget record (design 7.3:
    /// before the executor is told). Only a queued job with a registered
    /// executor can be admitted.
    pub fn admit(&mut self, job: &str, reserved: u64, extra: u64) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (state, executor): (String, Option<i64>) = tx
            .query_row(
                "SELECT state, executor_pid FROM jobs WHERE id = ?1",
                params![job],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if !PRE_LEASE.contains(&state.as_str()) || executor.is_none() {
            return Err(Conflict(format!("job {job} is {state} and cannot be admitted")).into());
        }
        let at = now();
        tx.execute(
            "INSERT INTO leases (job_id, granted_at) VALUES (?1, ?2)",
            params![job, at],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'admitted', reserved_private = ?2 WHERE id = ?1",
            params![job, reserved as i64],
        )?;
        if extra > 0 {
            tx.execute(
                "INSERT INTO budget_records (id, owner_kind, owner_id, bytes, updated_at) VALUES (?1, 'job', ?2, ?3, ?4)",
                params![format!("b_{job}"), job, extra as i64, at],
            )?;
        }
        Self::event(&tx, job, "admitted", Some(&reserved.to_string()))?;
        tx.commit()?;
        Ok(())
    }

    /// Moves a job between the states before its lease (queued, blocked,
    /// capacity insufficient).
    pub fn set_queue_state(&mut self, job: &str, state: &str) -> Result<()> {
        let n = self.conn.execute(
            &format!(
                "UPDATE jobs SET state = ?2 WHERE id = ?1 AND state IN ({}) AND state != ?2",
                in_list(PRE_LEASE)
            ),
            params![job, state],
        )?;
        if n > 0 {
            self.conn.execute(
                "INSERT INTO job_events (job_id, at, kind) VALUES (?1, ?2, ?3)",
                params![job, now(), state],
            )?;
        }
        Ok(())
    }

    /// Raises a reservation to what the job was measured using (design
    /// 10.2: the larger of prediction and measurement).
    pub fn raise_reservation(&mut self, job: &str, bytes: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET reserved_private = ?2 WHERE id = ?1 AND reserved_private < ?2",
            params![job, bytes as i64],
        )?;
        Ok(())
    }

    /// A job id, or the job of a submission id, and whether it is a
    /// service.
    pub fn resolve_job(&self, id: &str) -> Result<Option<(String, bool)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, service FROM jobs WHERE id = ?1
                 UNION ALL
                 SELECT j.id, j.service FROM submissions s JOIN jobs j ON j.id = s.job_id WHERE s.id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// `ats unblock`: drops the failed predecessors so the job can run
    /// (design 5.4).
    pub fn unblock(&mut self, job: &str) -> Result<()> {
        let failed: Vec<String> = self
            .predecessors(job)?
            .into_iter()
            .filter(|p| p.failed())
            .map(|p| p.job)
            .collect();
        let tx = self.conn.transaction()?;
        for p in &failed {
            tx.execute(
                "DELETE FROM dependencies WHERE job_id = ?1 AND after_job_id = ?2",
                params![job, p],
            )?;
        }
        tx.execute(
            "UPDATE jobs SET state = 'queued' WHERE id = ?1 AND state = 'blocked'",
            params![job],
        )?;
        Self::event(&tx, job, "unblocked", Some(&failed.join(",")))?;
        tx.commit()?;
        Ok(())
    }

    /// Manual position from the dashboard (design 10.3); `None` unpins.
    pub fn pin(&mut self, job: &str, rank: Option<i64>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE jobs SET pinned = ?2 WHERE id = ?1",
            params![job, rank],
        )?;
        if n == 0 {
            return Err(no_job(job));
        }
        Ok(())
    }

    /// Calibrated values and dashboard overrides.
    pub fn settings(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// When the user last interacted with each session (design 10.3).
    pub fn session_interactions(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, last_interaction FROM sessions WHERE last_interaction IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
