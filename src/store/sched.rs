//! Store operations for admission (design 10).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Conflict, Store, is_terminal, no_job, now, process};
use crate::sched::locks::Lock;

/// States before a lease: the job has not run.
pub const PRE_LEASE: &[&str] = &["queued", "blocked", "capacity_insufficient"];
/// States holding a lease and a reservation.
pub const HELD: &[&str] = &["admitted", "running", "awaiting_takeover"];

pub(super) fn in_list(states: &[&str]) -> String {
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
    /// Only completion with exit code 0 satisfies `--after` (design 10.4).
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
    pub predicted_seconds: Option<f64>,
    pub run_now: bool,
    pub executor: Option<(i64, String)>,
    pub locks: Vec<Lock>,
    pub deps: Vec<Predecessor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeldJob {
    pub id: String,
    pub reserved_private: u64,
    pub predicted_seconds: Option<f64>,
    pub started_at: Option<String>,
    pub locks: Vec<Lock>,
}

/// What an executor is given with its lease (design 10.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseTerms {
    pub parallelism: Option<u32>,
    /// Tokens for the jobserver pool, when the line runs jobserver clients.
    pub jobserver_tokens: Option<u32>,
    /// Variables to set to `parallelism`.
    pub env: Vec<String>,
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
            "SELECT id, state, created_at, predicted_private, predicted_seconds, run_now, executor_pid, executor_start
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
                    predicted_seconds: r.get(4)?,
                    run_now: r.get(5)?,
                    executor: process(r.get(6)?, r.get(7)?),
                    locks: Vec::new(),
                    deps: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|mut j| {
                j.locks = self.locks_of(&j.id)?;
                j.deps = self.predecessors(&j.id)?;
                Ok(j)
            })
            .collect()
    }

    /// Jobs holding a lease.
    pub fn held_jobs(&self) -> Result<Vec<HeldJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, reserved_private, predicted_seconds, started_at FROM jobs WHERE state IN ({})",
            in_list(HELD)
        ))?;
        let rows: Vec<HeldJob> = stmt
            .query_map([], |r| {
                Ok(HeldJob {
                    id: r.get(0)?,
                    reserved_private: r.get::<_, i64>(1)? as u64,
                    predicted_seconds: r.get(2)?,
                    started_at: r.get(3)?,
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

    /// Commits a lease with its reservation (design 7.1: before the
    /// executor is told). Only a waiting job with a registered executor can
    /// be admitted. A jobserver pool gets one token fewer than the
    /// parallelism: each client runs one job without a token (design 10.5).
    pub fn admit(&mut self, job: &str, reserved: u64, parallelism: u32) -> Result<()> {
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
        tx.execute(
            "INSERT INTO leases (job_id, granted_at, parallelism, jobserver_tokens)
             SELECT ?1, ?2, ?3, CASE WHEN jobserver THEN ?3 - 1 END FROM jobs WHERE id = ?1",
            params![job, now(), parallelism],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'admitted', reserved_private = ?2 WHERE id = ?1",
            params![job, reserved as i64],
        )?;
        Self::event(&tx, job, "admitted", Some(&reserved.to_string()))?;
        tx.commit()?;
        Ok(())
    }

    pub fn lease_terms(&self, job: &str) -> Result<LeaseTerms> {
        let row: Option<(Option<u32>, Option<u32>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT l.parallelism, l.jobserver_tokens, j.parallel_env FROM jobs j
                 LEFT JOIN leases l ON l.job_id = j.id WHERE j.id = ?1",
                params![job],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (parallelism, jobserver_tokens, env) = row.ok_or_else(|| no_job(job))?;
        Ok(LeaseTerms {
            parallelism,
            jobserver_tokens,
            env: env
                .and_then(|e| serde_json::from_str(&e).ok())
                .unwrap_or_default(),
        })
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

    /// A job id, or the job of a submission id.
    pub fn resolve_job(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM jobs WHERE id = ?1 UNION ALL SELECT job_id FROM submissions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `ats unblock`: drops the failed predecessors so the job can run.
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
}
