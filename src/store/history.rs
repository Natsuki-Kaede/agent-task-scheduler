//! Measured history for prediction (design 8.3): completed jobs from the
//! last 90 days, most recent first.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use rusqlite::params;

use super::Store;
use crate::sched::predict::Stats;

/// History older than this is not used (design 13: kept 90 days).
const WINDOW: Duration = Duration::from_secs(90 * 24 * 3600);
/// Enough recent runs for a percentile.
const LIMIT: i64 = 200;

fn seconds_between(start: &str, end: &str) -> Option<f64> {
    let (s, e) = (
        humantime::parse_rfc3339(start).ok()?,
        humantime::parse_rfc3339(end).ok()?,
    );
    Some(e.duration_since(s).ok()?.as_secs_f64())
}

impl Store {
    /// Stats over completed jobs matching every given field.
    pub fn history(
        &self,
        project: Option<&str>,
        command: Option<&str>,
        key: Option<&str>,
    ) -> Result<Stats> {
        let since = humantime::format_rfc3339_millis(SystemTime::now() - WINDOW).to_string();
        let mut stmt = self.conn.prepare_cached(
            "SELECT peak_private, started_at, ended_at FROM jobs
             WHERE state = 'completed' AND ended_at >= ?1
               AND (?2 IS NULL OR project_root = ?2)
               AND (?3 IS NULL OR command = ?3)
               AND (?4 IS NULL OR history_key = ?4)
             ORDER BY ended_at DESC LIMIT ?5",
        )?;
        let rows = stmt.query_map(params![since, project, command, key, LIMIT], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let (mut mem, mut secs) = (Vec::new(), Vec::new());
        for row in rows {
            let (peak, start, end) = row?;
            mem.extend(peak.map(|p| p as u64));
            secs.extend(start.zip(end).and_then(|(s, e)| seconds_between(&s, &e)));
        }
        Ok(Stats::from_samples(&mem, &secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, NewSubmission, Plan};

    fn run(store: &mut Store, project: &str, command: &str, key: &str, peak: i64, state: &str) {
        let new = NewSubmission {
            entry: Entry::Sh,
            command: command.into(),
            cwd: project.into(),
            project_root: project.into(),
            session_id: None,
            agent: None,
            invocation_key: None,
            request_id: None,
            sandbox_signature: None,
            service: false,
            options: serde_json::json!({}),
            launcher: None,
            plan: Plan {
                history_key: Some(key.into()),
                ..Plan::default()
            },
        };
        let job = store.create_submission(&new).unwrap().job;
        store.set_executor(&job, 1, "t").unwrap();
        store.admit(&job, 0, 0, 1).unwrap();
        store.set_started(&job, "/l", None, None).unwrap();
        store.record_exit_usage(&job, 1.0, Some(peak)).unwrap();
        store.finish(&job, state, Some(0)).unwrap();
    }

    #[test]
    fn matches_project_command_and_key() {
        let mut store = Store::open_in_memory().unwrap();
        run(
            &mut store,
            "/a",
            "cargo test",
            "cargo test",
            100,
            "completed",
        );
        run(
            &mut store,
            "/a",
            "cargo test -p x",
            "cargo test",
            200,
            "completed",
        );
        run(
            &mut store,
            "/b",
            "cargo test",
            "cargo test",
            400,
            "completed",
        );
        run(
            &mut store,
            "/a",
            "cargo test",
            "cargo test",
            9999,
            "canceled",
        );

        let line = store.history(Some("/a"), Some("cargo test"), None).unwrap();
        assert_eq!(
            (line.samples, line.mem),
            (1, Some(130)),
            "canceled runs are not history"
        );
        let project = store.history(Some("/a"), None, Some("cargo test")).unwrap();
        assert_eq!((project.samples, project.mem), (2, Some(260)));
        let global = store.history(None, None, Some("cargo test")).unwrap();
        assert_eq!((global.samples, global.mem), (3, Some(520)));
        assert!(global.seconds.is_some());
        assert_eq!(
            store.history(None, None, Some("make")).unwrap(),
            Stats::default()
        );

        // With five runs or more, the 95th percentile replaces max x 1.3.
        for peak in [10, 20, 30, 40, 50, 60] {
            run(&mut store, "/c", "make", "make", peak, "completed");
        }
        assert_eq!(
            store.history(Some("/c"), None, Some("make")).unwrap().mem,
            Some(60)
        );
    }
}
