//! The scheduling loop: a round every second and after every change, which
//! commits leases for the jobs `sched::decide` admits.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::api::AppState;
use crate::procinfo;
use crate::sched::{self, Capacity, Dep, Held, Round, Verdict, Waiting};
use crate::store::Conflict;
use crate::timing;

const ROUND_EVERY: Duration = Duration::from_secs(1);
/// After an admission, the next waits this long for the new job's memory to
/// show in the measurements, unless the job ends first (design 10.1).
const SETTLE: Duration = Duration::from_secs(3);

fn seconds_between(rfc3339: &str, now: SystemTime) -> Option<f64> {
    let at = humantime::parse_rfc3339(rfc3339).ok()?;
    Some(now.duration_since(at).unwrap_or_default().as_secs_f64())
}

pub async fn run(state: AppState) {
    let mut changes = state.changes.subscribe();
    let mut admissions = HashMap::new();
    while !state.stopping() {
        if let Err(e) = round(&state, &mut admissions) {
            tracing::error!("scheduling: {e:#}");
        }
        let _ = tokio::time::timeout(ROUND_EVERY, changes.changed()).await;
    }
}

/// One round. `admissions` holds when each job was admitted, on the clock
/// that counts sleep.
pub(super) fn round(state: &AppState, admissions: &mut HashMap<String, Duration>) -> Result<()> {
    let snapshot = state
        .measurements
        .lock()
        .expect("measurements lock")
        .clone();
    let (queued, held_jobs) = {
        let store = state.store();
        (store.queued_jobs()?, store.held_jobs()?)
    };
    if queued.is_empty() {
        state.verdicts.lock().expect("verdicts lock").clear();
        return Ok(());
    }
    let Some(system) = snapshot.system.clone() else {
        // Nothing measured yet: wait for the first sample.
        return Ok(());
    };
    let capacity = Capacity {
        memory: system.mem_total,
        used: system.mem_used,
    };
    let wall = SystemTime::now();
    let seconds_since = |at: &str| seconds_between(at, wall);
    let held: Vec<Held> = held_jobs
        .iter()
        .map(|h| Held {
            id: h.id.clone(),
            reserved: h.reserved_private,
            resident: snapshot.jobs.get(&h.id).map_or(0, |u| u.private_resident),
            locks: h.locks.clone(),
            ends_in: h
                .predicted_seconds
                .zip(h.started_at.as_deref().and_then(&seconds_since))
                .map(|(total, ran)| (total - ran).max(0.0)),
        })
        .collect();
    let awaited = state.awaited_jobs();
    let waiting: Vec<Waiting> = queued
        .iter()
        .map(|q| Waiting {
            id: q.id.clone(),
            age: seconds_since(&q.created_at).unwrap_or(0.0),
            reserved: q.predicted_private,
            seconds: q.predicted_seconds,
            locks: q.locks.clone(),
            deps: q
                .deps
                .iter()
                .map(|p| {
                    let d = if p.succeeded() {
                        Dep::Succeeded
                    } else if p.failed() {
                        Dep::Failed
                    } else {
                        Dep::Pending
                    };
                    (p.job.clone(), d)
                })
                .collect(),
            now: q.run_now,
            ready: q
                .executor
                .as_ref()
                .is_some_and(|(pid, start)| procinfo::alive(*pid as i32, start)),
            awaited: awaited.contains(&q.id),
        })
        .collect();

    let now = timing::elapsed();
    admissions.retain(|job, at| {
        now.saturating_sub(*at) < SETTLE && held_jobs.iter().any(|h| &h.id == job)
    });
    let verdicts = sched::decide(&Round {
        capacity: &capacity,
        held: &held,
        waiting: &waiting,
        pressure: state.pressure.lock().expect("pressure lock").pressured,
        settling: !admissions.is_empty(),
        admitting: state.admitting(),
    });

    let states: HashMap<&str, &str> = queued
        .iter()
        .map(|q| (q.id.as_str(), q.state.as_str()))
        .collect();
    let by_id: HashMap<&str, &Waiting> = waiting.iter().map(|w| (w.id.as_str(), w)).collect();
    let mut running = held.len() as u32;
    let mut changed = false;
    let mut waiting_verdicts = HashMap::new();
    for (id, verdict) in verdicts {
        let current = states[id.as_str()];
        let mut store = state.store();
        match &verdict {
            Verdict::Admit => {
                let reserved = by_id[id.as_str()].reserved;
                // Cores shared among the running jobs and this one (design 10.5).
                let parallelism = (system.cores / (running + 1)).max(1);
                match store.admit(&id, reserved, parallelism) {
                    Ok(()) => {
                        tracing::info!(job = %id, reserved, parallelism, "admitted");
                        admissions.insert(id.clone(), now);
                        running += 1;
                        changed = true;
                        continue;
                    }
                    // Canceled or ended since the round began.
                    Err(e) if e.downcast_ref::<Conflict>().is_some() => continue,
                    Err(e) => return Err(e),
                }
            }
            Verdict::Blocked { .. } if current != "blocked" => {
                store.set_queue_state(&id, "blocked")?;
                changed = true;
            }
            Verdict::CapacityInsufficient { .. } if current != "capacity_insufficient" => {
                store.set_queue_state(&id, "capacity_insufficient")?;
                changed = true;
            }
            Verdict::Wait { .. } if current != "queued" => {
                store.set_queue_state(&id, "queued")?;
                changed = true;
            }
            _ => {}
        }
        waiting_verdicts.insert(id, verdict);
    }
    *state.verdicts.lock().expect("verdicts lock") = waiting_verdicts;
    if changed {
        state.notify();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{Snapshot, System};

    const G: u64 = 1 << 30;

    fn submit(state: &AppState, mem: u64, after: &[&str]) -> String {
        use crate::sched::predict::Prediction;
        use crate::store::{Entry, NewSubmission, Plan};
        let new = NewSubmission {
            entry: Entry::Sh,
            command: "make".into(),
            cwd: "/w".into(),
            project_root: "/w".into(),
            session_id: None,
            launcher: None,
            plan: Plan {
                prediction: Prediction { mem, seconds: None },
                after: after.iter().map(|s| s.to_string()).collect(),
                ..Plan::default()
            },
        };
        let mut store = state.store();
        let job = store.create_submission(&new).unwrap().job;
        // This test process stands in for the executor.
        let me = std::process::id() as i64;
        store
            .set_executor(&job, me, &procinfo::start_time(me as i32).unwrap())
            .unwrap();
        job
    }

    fn job_state(state: &AppState, job: &str) -> String {
        state.store().job(job).unwrap().unwrap().state
    }

    fn reason(state: &AppState, job: &str) -> sched::Reason {
        match &state.verdicts.lock().unwrap()[job] {
            Verdict::Wait { reason } => reason.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rounds_admit_settle_block_and_refuse() {
        let state = AppState::new(
            crate::store::Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        );
        *state.measurements.lock().unwrap() = Snapshot {
            system: Some(System {
                mem_total: 16 * G,
                mem_used: 4 * G,
                cores: 8,
                pressure: 1,
                ..System::default()
            }),
            ..Snapshot::default()
        };
        let mut admissions = HashMap::new();

        let a = submit(&state, 2 * G, &[]);
        let a2 = submit(&state, 2 * G, &[]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(
            (job_state(&state, &a), job_state(&state, &a2)),
            ("admitted".into(), "admitted".into())
        );
        let parallelism = |job: &str| state.store().lease_terms(job).unwrap().parallelism;
        assert_eq!(
            (parallelism(&a), parallelism(&a2)),
            (Some(8), Some(4)),
            "cores / (running + 1)"
        );

        // While a and a2 settle, the next admission waits.
        let b = submit(&state, G, &[]);
        let too_big = submit(&state, 20 * G, &[]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(reason(&state, &b), sched::Reason::Settling);
        assert_eq!(job_state(&state, &too_big), "capacity_insufficient");

        // A failed predecessor blocks; once the new jobs end, nothing settles.
        state.store().finish(&a, "completed", Some(1)).unwrap();
        state.store().finish(&a2, "completed", Some(0)).unwrap();
        let c = submit(&state, G, &[&a]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &c), "blocked");
        assert_eq!(job_state(&state, &b), "admitted");
        state.store().unblock(&c).unwrap();
        assert_eq!(job_state(&state, &c), "queued");

        // Pressure holds admission.
        state.store().finish(&b, "completed", Some(0)).unwrap();
        state.pressure.lock().unwrap().pressured = true;
        round(&state, &mut admissions).unwrap();
        assert_eq!(reason(&state, &c), sched::Reason::Pressure);
        state.pressure.lock().unwrap().pressured = false;

        // While executors reconnect after a restart, nothing is admitted.
        state.expect_reconnects(["gone".to_string()].into(), Duration::from_secs(60));
        round(&state, &mut admissions).unwrap();
        assert_eq!(reason(&state, &c), sched::Reason::Recovering);
        state.contact("gone");
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &c), "admitted");
    }
}
