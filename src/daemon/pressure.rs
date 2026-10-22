//! The pressure response (design 10.2), run after every sample: while
//! pressure is up the scheduler admits nothing, and the processes of
//! running jobs are on background QoS; 30 seconds after it subsides both
//! are undone. ats never freezes, pauses or terminates a job, and never
//! touches processes it does not manage.

use std::collections::HashSet;

use crate::api::AppState;
use crate::control;
use crate::procinfo;
use crate::sample::Snapshot;
use crate::sched::pressure::{PressureStatus, Tracker};
use crate::timing;

#[derive(Default)]
pub struct Response {
    tracker: Tracker,
    /// Processes put on background QoS, as (pid, start time).
    background: HashSet<(i32, String)>,
}

impl Response {
    pub fn update(&mut self, state: &AppState, snapshot: &Snapshot) {
        let Some(system) = &snapshot.system else {
            return;
        };
        let now = timing::elapsed().as_secs_f64();
        let (swap_outs, pressured) = self
            .tracker
            .update(snapshot.swap_outs, system.pressure, now);
        let was = state.pressure.lock().expect("pressure lock").pressured;
        if pressured {
            for pid in snapshot.jobs.values().flat_map(|u| &u.processes) {
                if let Some(start) = procinfo::start_time(*pid)
                    && !self.background.contains(&(*pid, start.clone()))
                    && control::set_background(*pid, true)
                {
                    self.background.insert((*pid, start));
                }
            }
        } else {
            for (pid, start) in self.background.drain() {
                if procinfo::alive(pid, &start) {
                    control::set_background(pid, false);
                }
            }
        }
        if pressured != was {
            tracing::warn!(
                pressured,
                swap_outs,
                system_level = system.pressure,
                "memory pressure"
            );
        }
        let managed: u64 = snapshot.jobs.values().map(|u| u.private_resident).sum();
        *state.pressure.lock().expect("pressure lock") = PressureStatus {
            pressured,
            swap_outs,
            system_level: system.pressure,
            managed,
            unmanaged: system.mem_used.saturating_sub(managed),
        };
        if pressured != was {
            state.notify();
        }
    }
}
