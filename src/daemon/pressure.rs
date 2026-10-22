//! The pressure loop (design 11): once a second, after sampling, it sets
//! the pressure level, slows or freezes jobs and undoes that after a calm
//! period, raises freeze alerts, and reports where the pressure comes from.
//! Processes ats does not manage are never touched; pressure they cause
//! only stops admission and slows managed jobs.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::api::AppState;
use crate::sched::pressure::{self, PressureStatus, Running, Signals, Thresholds, Tracker};
use crate::timing;
use anyhow::Result;

const TICK: Duration = Duration::from_secs(1);
/// Frozen this long in awake time, a job is reported (design 11, user
/// decision 7: 30 minutes). The settings key `freeze_alert_secs`
/// overrides it.
const FREEZE_ALERT_SECS: f64 = 30.0 * 60.0;

fn thresholds(map: &HashMap<String, String>) -> Thresholds {
    let mut t = Thresholds::default();
    let num = |k: &str| map.get(k).and_then(|v| v.parse::<f64>().ok());
    for (key, field) in [
        ("watch_swapouts", &mut t.watch_swapouts),
        ("slow_swapouts", &mut t.slow_swapouts),
        ("freeze_paging", &mut t.freeze_paging),
        ("calm_secs", &mut t.calm_secs),
    ] {
        if let Some(v) = num(key) {
            *field = v;
        }
    }
    t
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

pub fn run(state: &AppState) {
    let mut tracker = Tracker::default();
    let mut last_step = 0.0f64;
    while !state.stopping() {
        if let Err(e) = tick(state, &mut tracker, &mut last_step) {
            tracing::error!("pressure response: {e:#}");
        }
        std::thread::sleep(TICK);
    }
}

fn tick(state: &AppState, tracker: &mut Tracker, last_step: &mut f64) -> Result<()> {
    let snapshot = state
        .measurements
        .lock()
        .expect("measurements lock")
        .clone();
    let Some(system) = snapshot.system.clone() else {
        return Ok(());
    };
    let (held, settings) = {
        let store = state.store();
        (store.held_for_pressure()?, store.settings()?)
    };
    let t = thresholds(&settings);
    let usage = |id: &str| snapshot.jobs.get(id).cloned().unwrap_or_default();

    let signals = Signals {
        system_level: system.pressure,
        swap_ins: snapshot.rates.swap_ins,
        swap_outs: snapshot.rates.swap_outs,
        over_reservation: held
            .iter()
            .any(|h| usage(&h.id).peak_private_total > h.reserved_private),
    };
    let now = timing::awake().as_secs_f64();
    let level = tracker.update(pressure::level(&signals, &t), now, &t);

    // The job that exceeded its prediction keeps running (design 11). When
    // several did, the one started first keeps running: it is the most
    // likely to finish and give its memory back.
    let over: Vec<&crate::store::HeldControl> = held
        .iter()
        .filter(|h| h.state != "admitted" && usage(&h.id).peak_private_total > h.predicted_private)
        .collect();
    let kept = over
        .iter()
        .min_by(|a, b| a.started_at.cmp(&b.started_at))
        .map(|h| h.id.clone());
    let running: Vec<Running> = held
        .iter()
        .filter(|h| h.state != "admitted")
        .map(|h| {
            let u = usage(&h.id);
            let ran = h.started_at.as_deref().and_then(seconds_since);
            Running {
                id: h.id.clone(),
                freezable: h.freezable,
                jobserver: h.jobserver_tokens,
                controls: h.controls.clone(),
                over_prediction: kept.as_ref() == Some(&h.id),
                ending_soon: h
                    .predicted_seconds
                    .zip(ran)
                    .is_some_and(|(p, r)| p - r < t.ending_secs),
                swapped: u.private_total.saturating_sub(u.private_resident),
            }
        })
        .collect();
    let budget = settings
        .get("memory_budget")
        .and_then(|v| crate::profile::parse_size(v).ok())
        .unwrap_or(system.mem_total);
    let headroom = budget as i64 - system.mem_used as i64;
    let changes = pressure::respond(
        level,
        &running,
        tracker.calm_for(now),
        now - *last_step,
        headroom,
        &t,
    );

    if !changes.is_empty() {
        let boot = snapshot.boot_id.clone().unwrap_or_default();
        let awake_ns = timing::awake().as_nanos() as i64;
        for (job, c) in &changes {
            tracing::info!(job = %job, level, controls = ?c, "pressure response");
            if let Err(e) = state
                .store()
                .set_controls(job, c, awake_ns, &boot, &state.instance)
            {
                tracing::warn!(job = %job, "applying controls: {e:#}");
            }
        }
        if level == 0 {
            *last_step = now;
        }
        state.notify();
    }

    let mut alerts = Vec::new();
    let boot = snapshot.boot_id.clone().unwrap_or_default();
    for h in held.iter().filter(|h| h.controls.frozen) {
        let total = state.store().interval_total(
            &h.id,
            "frozen",
            timing::awake().as_nanos() as i64,
            &boot,
        )?;
        let secs = total.map_or(0.0, |t| t.awake_ns as f64 / 1e9);
        let limit = settings
            .get("freeze_alert_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(FREEZE_ALERT_SECS);
        if secs >= limit {
            if state.store().alert_frozen(&h.id, secs)? {
                tracing::warn!(job = %h.id, minutes = secs / 60.0, "frozen for a long time");
            }
            alerts.push(h.id.clone());
        }
    }

    let managed: u64 = snapshot.jobs.values().map(|u| u.private_resident).sum();
    let shared: u64 = snapshot.shared.values().map(|u| u.private_resident).sum();
    let unmanaged = system.mem_used.saturating_sub(managed + shared);
    *state.pressure.lock().expect("pressure lock") = PressureStatus {
        level,
        signals,
        calm_for: tracker.calm_for(now),
        managed,
        shared,
        unmanaged,
        source: if managed + shared >= unmanaged {
            "managed"
        } else {
            "unmanaged"
        },
        changes,
        alerts,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_come_from_settings() {
        let map: HashMap<String, String> = [
            ("freeze_paging", "500"),
            ("calm_secs", "10"),
            ("other", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let t = thresholds(&map);
        assert_eq!(
            (t.freeze_paging, t.calm_secs, t.watch_swapouts),
            (500.0, 10.0, 64.0)
        );
    }
}
