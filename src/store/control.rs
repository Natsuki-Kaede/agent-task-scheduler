//! Store operations for process control (design 10.5, 10.6, 11).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Conflict, HELD, Store, no_job};
use crate::control::Controls;

/// A job holding a lease, as the pressure response needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldControl {
    pub id: String,
    pub state: String,
    pub freezable: bool,
    pub jobserver_tokens: Option<u32>,
    pub controls: Controls,
    pub predicted_private: u64,
    pub reserved_private: u64,
    pub predicted_seconds: Option<f64>,
    pub started_at: Option<String>,
}

/// What an executor is given with its lease.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseTerms {
    pub parallelism: Option<u32>,
    /// Tokens to put in the jobserver pool, when the line runs jobserver
    /// clients.
    pub jobserver_tokens: Option<u32>,
    /// Variables to set to `parallelism`.
    pub env: Vec<String>,
}

impl Store {
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

    pub fn held_for_pressure(&self) -> Result<Vec<HeldControl>> {
        let held = HELD
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT j.id, j.state, j.freezable, l.jobserver_tokens, j.controls, j.predicted_private,
                    j.reserved_private, j.predicted_seconds, j.started_at
             FROM jobs j LEFT JOIN leases l ON l.job_id = j.id WHERE j.state IN ({held})"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(HeldControl {
                id: r.get(0)?,
                state: r.get(1)?,
                freezable: r.get(2)?,
                jobserver_tokens: r.get(3)?,
                controls: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
                predicted_private: r.get::<_, i64>(5)? as u64,
                reserved_private: r.get::<_, i64>(6)? as u64,
                predicted_seconds: r.get(7)?,
                started_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a freeze alert once per job (design 11: frozen for 30 minutes
    /// of awake time). Returns true when it was new.
    pub fn alert_frozen(&mut self, job: &str, awake_secs: f64) -> Result<bool> {
        let seen: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM job_events WHERE job_id = ?1 AND kind = 'freeze_alert')",
            params![job],
            |r| r.get(0),
        )?;
        if !seen {
            self.conn.execute(
                "INSERT INTO job_events (job_id, at, kind, detail) VALUES (?1, ?2, 'freeze_alert', ?3)",
                params![job, super::now(), format!("{:.0}", awake_secs)],
            )?;
        }
        Ok(!seen)
    }

    /// Whether SIGSTOP may be used on the job (design 11).
    pub fn freezable(&self, job: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT freezable FROM jobs WHERE id = ?1",
                params![job],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| no_job(job))
    }

    pub fn controls(&self, job: &str) -> Result<Controls> {
        let text: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT controls FROM jobs WHERE id = ?1",
                params![job],
                |r| r.get(0),
            )
            .optional()?;
        Ok(text
            .ok_or_else(|| no_job(job))?
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    /// Jobs holding a lease that have controls set, for the controller to
    /// apply, also after a restart.
    pub fn controlled_jobs(&self) -> Result<Vec<(String, Controls)>> {
        let held = HELD
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, controls FROM jobs WHERE controls IS NOT NULL AND state IN ({held})"
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, text) = row?;
            if let Ok(c) = serde_json::from_str::<Controls>(&text) {
                out.push((id, c));
            }
        }
        Ok(out)
    }

    /// Sets a job's controls and moves a running job between running,
    /// throttled and frozen, counting awake time in each (design 9.6).
    /// Freezing and duty cycles are refused for jobs that must not be
    /// stopped.
    pub fn set_controls(
        &mut self,
        job: &str,
        c: &Controls,
        awake_ns: i64,
        boot: &str,
        instance: &str,
    ) -> Result<()> {
        if c.stops() && !self.freezable(job)? {
            return Err(Conflict(format!(
                "job {job} must not be stopped: it is not marked freezable, or was submitted with --no-freeze or --now"
            ))
            .into());
        }
        let state: String = self
            .conn
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![job], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| no_job(job))?;
        if !HELD.contains(&state.as_str()) {
            return Err(Conflict(format!(
                "job {job} is {state}; only jobs holding a lease can be controlled"
            ))
            .into());
        }
        let text = (c != &Controls::default())
            .then(|| serde_json::to_string(c))
            .transpose()?;
        self.conn.execute(
            "UPDATE jobs SET controls = ?2 WHERE id = ?1",
            params![job, text],
        )?;
        if matches!(state.as_str(), "running" | "throttled" | "frozen") && c.state() != state {
            let next = c.state();
            self.conn.execute(
                "UPDATE jobs SET state = ?2 WHERE id = ?1",
                params![job, next],
            )?;
            self.conn.execute(
                "INSERT INTO job_events (job_id, at, kind) VALUES (?1, ?2, ?3)",
                params![job, super::now(), next],
            )?;
        }
        for kind in ["frozen", "throttled"] {
            let on = if kind == "frozen" {
                c.frozen
            } else {
                c.slowed() && !c.frozen
            };
            if on {
                self.open_interval(job, kind, awake_ns, boot, instance)?;
            } else {
                self.close_interval(job, kind, awake_ns, boot)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, NewSubmission, Plan};

    #[test]
    fn freeze_alerts_are_recorded_once() {
        let mut store = Store::open_in_memory().unwrap();
        let new = NewSubmission {
            entry: Entry::Sh,
            command: "make".into(),
            cwd: "/w".into(),
            project_root: "/w".into(),
            session_id: None,
            agent: None,
            invocation_key: None,
            request_id: None,
            sandbox_signature: None,
            service: false,
            options: serde_json::json!({}),
            launcher: None,
            plan: Plan::default(),
        };
        let job = store.create_submission(&new).unwrap().job;
        assert!(store.alert_frozen(&job, 1800.0).unwrap());
        assert!(!store.alert_frozen(&job, 1900.0).unwrap());
    }
}
