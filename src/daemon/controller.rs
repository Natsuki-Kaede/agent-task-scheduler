//! Applies each job's controls to its processes (design 10.6, 11): sets and
//! reverts background QoS, and stops and resumes processes for freezing and
//! duty cycles. Controls live in the store, so a restarted daemon picks
//! them up again; the processes it touched are tracked here, since the
//! system cannot report the QoS state.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::api::AppState;
use crate::control::{self, Controls};
use crate::procinfo;
use crate::timing;

const TICK: Duration = Duration::from_millis(100);
/// Duty cycles run in periods of this many seconds.
const PERIOD: f64 = 1.0;

type Key = (i32, String);

#[derive(Default)]
struct Applied {
    background: HashSet<Key>,
    stopped: HashSet<Key>,
}

impl Applied {
    /// Undoes what was set, on processes that still exist.
    fn release_background(&mut self) {
        for (pid, start) in self.background.drain() {
            if procinfo::alive(pid, &start) {
                control::set_background(pid, false);
            }
        }
    }

    fn resume_all(&mut self) {
        for (pid, start) in self.stopped.drain() {
            if procinfo::alive(pid, &start) {
                control::resume(pid);
            }
        }
    }
}

fn apply(c: &Controls, pids: &[Key], awake: f64, applied: &mut Applied) {
    if c.background {
        for key in pids {
            if !applied.background.contains(key) && control::set_background(key.0, true) {
                applied.background.insert(key.clone());
            }
        }
    } else {
        applied.release_background();
    }
    let runs = !c.frozen && c.duty.is_none_or(|f| control::duty_runs(awake, PERIOD, f));
    if runs {
        applied.resume_all();
    } else {
        for key in pids {
            if !applied.stopped.contains(key) && control::stop(key.0) {
                applied.stopped.insert(key.clone());
            }
        }
    }
}

pub fn run(state: &AppState) {
    let mut applied: HashMap<String, Applied> = HashMap::new();
    let mut desired: Vec<(String, Controls)> = Vec::new();
    let mut seen_change = None;
    let mut since_read = Duration::MAX;
    while !state.stopping() {
        // Re-read after every change and every second.
        let change = *state.changes.borrow();
        if seen_change != Some(change) || since_read >= Duration::from_secs(1) {
            match state.store().controlled_jobs() {
                Ok(d) => desired = d,
                Err(e) => tracing::error!("reading job controls: {e:#}"),
            }
            seen_change = Some(change);
            since_read = Duration::ZERO;
        }
        let jobs = state
            .measurements
            .lock()
            .expect("measurements lock")
            .jobs
            .clone();
        let awake = timing::awake().as_secs_f64();
        for (job, c) in &desired {
            let pids: Vec<Key> = jobs
                .get(job)
                .map(|u| {
                    u.processes
                        .iter()
                        .filter_map(|&p| procinfo::start_time(p).map(|s| (p, s)))
                        .collect()
                })
                .unwrap_or_default();
            apply(c, &pids, awake, applied.entry(job.clone()).or_default());
        }
        let wanted: HashSet<&String> = desired.iter().map(|(j, _)| j).collect();
        applied.retain(|job, a| {
            if wanted.contains(job) {
                return true;
            }
            a.release_background();
            a.resume_all();
            false
        });
        std::thread::sleep(TICK);
        since_read += TICK;
    }
    // Frozen jobs stay frozen across a restart; a duty cycle must not leave
    // its job stopped while nothing runs it.
    for (job, c) in &desired {
        if !c.frozen
            && let Some(a) = applied.get_mut(job)
        {
            a.resume_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_of(pid: u32) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn applies_and_releases_controls() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let pids = vec![(pid, procinfo::start_time(pid).unwrap())];
        let mut a = Applied::default();

        let frozen = Controls {
            frozen: true,
            background: true,
            ..Controls::default()
        };
        apply(&frozen, &pids, 0.0, &mut a);
        std::thread::sleep(Duration::from_millis(50));
        assert!(state_of(child.id()).starts_with('T'));
        assert_eq!((a.stopped.len(), a.background.len()), (1, 1));

        // A quarter-duty cycle runs for the first quarter of each second.
        let duty = Controls {
            duty: Some(0.25),
            ..Controls::default()
        };
        apply(&duty, &pids, 10.1, &mut a);
        assert!(a.stopped.is_empty() && a.background.is_empty());
        apply(&duty, &pids, 10.5, &mut a);
        assert_eq!(a.stopped.len(), 1);

        apply(&Controls::default(), &pids, 11.0, &mut a);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!state_of(child.id()).starts_with('T'));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
