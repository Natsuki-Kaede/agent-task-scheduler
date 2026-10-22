//! The scheduling loop: a round every second and after every change, which
//! commits leases for the jobs `sched::decide` admits.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::api::AppState;
use crate::procinfo;
use crate::sample::Snapshot;
use crate::sched::{self, Capacity, Dep, Held, Round, Settings, Verdict, Waiting};
use crate::store::Conflict;
use crate::timing;

const ROUND_EVERY: Duration = Duration::from_secs(1);
/// How long a user interaction keeps raising its session's jobs (design
/// 10.3: 10 minutes).
const SESSION_DECAY: f64 = 600.0;

/// Calibrated values, overridable from the dashboard through the settings
/// table (design 9.4).
struct Calibration {
    /// C; the machine's memory when not set.
    memory_budget: Option<u64>,
    cpu_target: f64,
    /// Swap-out pages per second that count as rising pressure.
    swapout_rate: f64,
    settings: Settings,
}

impl Calibration {
    fn from(map: &HashMap<String, String>) -> Self {
        let num = |key: &str| map.get(key).and_then(|v| v.parse::<f64>().ok());
        let mut settings = Settings::default();
        if let Some(v) = num("admission_interval") {
            settings.interval = v;
        }
        if let Some(v) = num("tight_fraction") {
            settings.tight = v;
        }
        Calibration {
            memory_budget: map
                .get("memory_budget")
                .and_then(|v| crate::profile::parse_size(v).ok()),
            cpu_target: num("cpu_target").unwrap_or(1.25),
            swapout_rate: num("swapout_rate").unwrap_or(64.0),
            settings,
        }
    }
}

/// The ats pressure level for admission (design 11): 1 when memory
/// pressure is rising (the system reports critical pressure, or it is
/// swapping out faster than calibrated) or a job uses more than it
/// reserved. The four-level response refines this.
fn pressure(snapshot: &Snapshot, cal: &Calibration, over_reservation: bool) -> u8 {
    let critical = snapshot.system.as_ref().is_some_and(|s| s.pressure >= 4);
    let swapping = snapshot.rates.swap_outs >= cal.swapout_rate;
    u8::from(critical || swapping || over_reservation)
}

fn seconds_since(rfc3339: &str) -> Option<f64> {
    let at = humantime::parse_rfc3339(rfc3339).ok()?;
    Some(
        SystemTime::now()
            .duration_since(at)
            .unwrap_or_default()
            .as_secs_f64(),
    )
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
    let (queued, mut held_jobs, interactions, cal) = {
        let store = state.store();
        (
            store.queued_jobs()?,
            store.held_jobs()?,
            store.session_interactions()?,
            Calibration::from(&store.settings()?),
        )
    };
    if queued.is_empty() {
        state.verdicts.lock().expect("verdicts lock").clear();
        return Ok(());
    }

    // Reservations follow measurements upward (design 10.2).
    let mut over = false;
    for h in &mut held_jobs {
        if let Some(u) = snapshot.jobs.get(&h.id)
            && u.peak_private_total > h.reserved_private
        {
            state
                .store()
                .raise_reservation(&h.id, u.peak_private_total)?;
            h.reserved_private = u.peak_private_total;
            over = true;
        }
    }

    let Some(system) = snapshot.system.clone() else {
        // Nothing measured yet: wait for the first sample.
        return Ok(());
    };
    let capacity = Capacity {
        memory: cal.memory_budget.unwrap_or(system.mem_total),
        used: system.mem_used,
        cores: f64::from(system.perf_cores + system.efficiency_cores).max(1.0),
        cpu_target: cal.cpu_target,
    };
    let held: Vec<Held> = held_jobs
        .iter()
        .map(|h| Held {
            id: h.id.clone(),
            reserved: h.reserved_private,
            resident: snapshot.jobs.get(&h.id).map_or(0, |u| u.private_resident),
            extra: h.extra,
            cores: h.predicted_cores,
            locks: h.locks.clone(),
            ends_in: h
                .predicted_seconds
                .zip(h.started_at.as_deref().and_then(seconds_since))
                .map(|(total, ran)| (total - ran).max(0.0)),
        })
        .collect();
    let awaited = state.awaited_jobs();
    let recency = |sessions: &[String]| {
        sessions
            .iter()
            .filter_map(|s| interactions.get(s).and_then(|at| seconds_since(at)))
            .map(|ago| (1.0 - ago / SESSION_DECAY).max(0.0))
            .fold(0.0, f64::max)
    };
    let waiting: Vec<Waiting> = queued
        .iter()
        .map(|q| Waiting {
            id: q.id.clone(),
            age: seconds_since(&q.created_at).unwrap_or(0.0),
            reserved: q.predicted_private,
            extra: q.predicted_extra,
            cores: q.predicted_cores,
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
            session_recency: recency(&q.sessions),
            urgency: q.urgency,
            pinned: q.pinned,
        })
        .collect();

    // Admissions are spaced so the last one's memory shows up in the
    // measurements (design 10.2); a job that already ended needs no wait.
    let now = timing::elapsed();
    admissions.retain(|job, _| held_jobs.iter().any(|h| &h.id == job));
    let since_admission = admissions
        .values()
        .map(|t| now.saturating_sub(*t).as_secs_f64())
        .reduce(f64::min);
    let verdicts = sched::decide(&Round {
        capacity: &capacity,
        held: &held,
        waiting: &waiting,
        pressure: pressure(&snapshot, &cal, over),
        since_admission,
        admitting: state.admitting(),
        settings: &cal.settings,
    });

    let states: HashMap<&str, &str> = queued
        .iter()
        .map(|q| (q.id.as_str(), q.state.as_str()))
        .collect();
    let by_id: HashMap<&str, &Waiting> = waiting.iter().map(|w| (w.id.as_str(), w)).collect();
    let mut changed = false;
    let mut waiting_verdicts = HashMap::new();
    for (id, verdict) in verdicts {
        let current = states[id.as_str()];
        let mut store = state.store();
        match &verdict {
            Verdict::Admit => {
                let w = by_id[id.as_str()];
                match store.admit(&id, w.reserved, w.extra) {
                    Ok(()) => {
                        tracing::info!(job = %id, reserved = w.reserved, "admitted");
                        admissions.insert(id.clone(), now);
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
    use crate::sample::{Rates, System};

    #[test]
    fn pressure_rises_on_critical_level_swapping_or_overuse() {
        let cal = Calibration::from(&HashMap::new());
        let mut snap = Snapshot {
            system: Some(System {
                pressure: 2,
                ..System::default()
            }),
            ..Snapshot::default()
        };
        assert_eq!(
            pressure(&snap, &cal, false),
            0,
            "a steady warning level alone does not stop admission"
        );
        assert_eq!(pressure(&snap, &cal, true), 1);
        snap.rates = Rates {
            swap_outs: 100.0,
            ..Rates::default()
        };
        assert_eq!(pressure(&snap, &cal, false), 1);
        snap.rates = Rates::default();
        snap.system.as_mut().unwrap().pressure = 4;
        assert_eq!(pressure(&snap, &cal, false), 1);
    }

    fn submit(state: &AppState, mem: u64, after: &[&str]) -> String {
        use crate::sched::predict::Prediction;
        use crate::store::{Entry, NewSubmission, Plan};
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
            plan: Plan {
                prediction: Prediction {
                    private: mem,
                    cores: 1.0,
                    ..Prediction::default()
                },
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

    #[test]
    fn rounds_admit_space_block_and_refuse() {
        const G: u64 = 1 << 30;
        let state = AppState::new(
            crate::store::Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        );
        *state.measurements.lock().unwrap() = Snapshot {
            system: Some(System {
                mem_total: 16 * G,
                mem_used: 4 * G,
                perf_cores: 4,
                efficiency_cores: 4,
                pressure: 1,
                ..System::default()
            }),
            ..Snapshot::default()
        };
        let mut admissions = HashMap::new();

        let a = submit(&state, 2 * G, &[]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &a), "admitted");
        assert_eq!(state.store().job(&a).unwrap().unwrap().state, "admitted");

        // While a is running, the next admission waits for the interval.
        let b = submit(&state, G, &[]);
        let too_big = submit(&state, 20 * G, &[]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &b), "queued");
        assert_eq!(
            state.verdicts.lock().unwrap()[&b],
            Verdict::Wait {
                reason: sched::Reason::Interval
            }
        );
        assert_eq!(job_state(&state, &too_big), "capacity_insufficient");

        // A failed predecessor blocks; unblocking queues it again.
        state.store().finish(&a, "completed", Some(1)).unwrap();
        let c = submit(&state, G, &[&a]);
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &c), "blocked");
        assert_eq!(
            job_state(&state, &b),
            "admitted",
            "a ended, so no spacing is needed"
        );
        state.store().unblock(&c).unwrap();
        assert_eq!(job_state(&state, &c), "queued");

        // While executors reconnect after a restart, nothing is admitted.
        state.expect_reconnects(["gone".to_string()].into(), Duration::from_secs(60));
        state.store().finish(&b, "completed", Some(0)).unwrap();
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &c), "queued");
        assert_eq!(
            state.verdicts.lock().unwrap()[&c],
            Verdict::Wait {
                reason: sched::Reason::Recovering
            }
        );
        state.contact("gone");
        round(&state, &mut admissions).unwrap();
        assert_eq!(job_state(&state, &c), "admitted");
    }

    #[test]
    fn calibration_overrides() {
        let map: HashMap<String, String> = [
            ("memory_budget", "12G"),
            ("admission_interval", "1"),
            ("swapout_rate", "10"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cal = Calibration::from(&map);
        assert_eq!(cal.memory_budget, Some(12 << 30));
        assert_eq!(cal.settings.interval, 1.0);
        assert_eq!(cal.swapout_rate, 10.0);
    }
}
