//! The four pressure levels and what each does to running jobs (design
//! 11), as pure functions of one second's inputs.
//!
//! | level | trigger | action |
//! |---|---|---|
//! | 0 | — | schedule normally; after a calm period, undo controls step by step |
//! | 1 | pressure rising, or a job above its reservation | stop admitting |
//! | 2 | pressure rising further | slow other jobs: background QoS, fewer jobserver tokens, a duty cycle for freezable jobs without a jobserver |
//! | 3 | paging above the threshold | freeze freezable jobs; slow the rest |
//!
//! A job using more than predicted and jobs about to end keep running. No
//! job is ever terminated.

use serde::Serialize;

use crate::control::Controls;

/// Calibratable thresholds (design 9.4). Rates are pages per second.
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    pub watch_swapouts: f64,
    pub slow_swapouts: f64,
    pub freeze_paging: f64,
    /// Seconds a raised level must hold before levels 2 and 3 act.
    pub confirm_secs: f64,
    /// Seconds at level 0 before controls are undone (design 11: 60).
    pub calm_secs: f64,
    /// Seconds between recovery steps.
    pub step_secs: f64,
    /// A job predicted to end within this many seconds keeps running.
    pub ending_secs: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            watch_swapouts: 64.0,
            slow_swapouts: 256.0,
            freeze_paging: 1024.0,
            confirm_secs: 3.0,
            calm_secs: 60.0,
            step_secs: 5.0,
            ending_secs: 30.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Signals {
    /// The system's own pressure level (macOS: 1, 2 or 4).
    pub system_level: u8,
    pub swap_ins: f64,
    pub swap_outs: f64,
    /// A job measured above its reservation.
    pub over_reservation: bool,
}

/// The level the signals call for, before confirmation.
pub fn level(s: &Signals, t: &Thresholds) -> u8 {
    if s.swap_ins + s.swap_outs >= t.freeze_paging {
        3
    } else if s.swap_outs >= t.slow_swapouts || s.system_level >= 4 {
        2
    } else if s.swap_outs >= t.watch_swapouts || s.over_reservation {
        1
    } else {
        0
    }
}

/// Levels over time: level 1 acts at once, since it only stops admission;
/// levels 2 and 3 act once they have held for `confirm_secs`.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    level: u8,
    /// The level being confirmed and since when (awake seconds).
    rising: Option<(u8, f64)>,
    /// Since when the level has been 0.
    calm_since: Option<f64>,
}

impl Tracker {
    pub fn update(&mut self, wanted: u8, now: f64, t: &Thresholds) -> u8 {
        if wanted <= 1 || wanted <= self.level {
            self.level = wanted;
            self.rising = None;
        } else {
            match self.rising {
                Some((l, since)) if l <= wanted && now - since >= t.confirm_secs => {
                    self.level = l.max(self.level);
                    self.rising = (wanted > self.level).then_some((wanted, now));
                }
                Some((l, _)) if l <= wanted => {}
                _ => self.rising = Some((wanted, now)),
            }
            self.level = self.level.max(1);
        }
        if self.level == 0 {
            self.calm_since.get_or_insert(now);
        } else {
            self.calm_since = None;
        }
        self.level
    }

    /// Seconds the level has been 0.
    pub fn calm_for(&self, now: f64) -> f64 {
        self.calm_since.map_or(0.0, |s| now - s)
    }
}

/// A job holding a lease, as the response sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub id: String,
    pub freezable: bool,
    /// Its leased jobserver tokens, when it has a pool.
    pub jobserver: Option<u32>,
    pub controls: Controls,
    /// Measured above its prediction: it keeps running (design 11).
    pub over_prediction: bool,
    pub ending_soon: bool,
    /// Private memory compressed or swapped out, which resuming brings back.
    pub swapped: u64,
}

fn slowed(j: &Running) -> Controls {
    Controls {
        background: true,
        tokens: j.jobserver.map(|_| 1),
        duty: (j.freezable && j.jobserver.is_none()).then_some(0.5),
        frozen: false,
        by_pressure: true,
    }
}

fn frozen(j: &Running) -> Controls {
    Controls {
        frozen: true,
        ..slowed(j)
    }
}

/// The control changes for this second. `headroom` is free memory now;
/// `since_step` is the time since the last recovery step.
pub fn respond(
    level: u8,
    jobs: &[Running],
    calm_for: f64,
    since_step: f64,
    headroom: i64,
    t: &Thresholds,
) -> Vec<(String, Controls)> {
    // Controls set by hand, from the dashboard, are left alone.
    let ours = jobs
        .iter()
        .filter(|j| j.controls.by_pressure || j.controls == Controls::default());
    let mut changes = Vec::new();
    match level {
        2 | 3 => {
            for j in ours.filter(|j| !j.over_prediction && !j.ending_soon) {
                let want = if level == 3 && j.freezable {
                    frozen(j)
                } else if j.controls.frozen {
                    // Unfreezing waits for the calm period.
                    j.controls.clone()
                } else {
                    slowed(j)
                };
                if want != j.controls {
                    changes.push((j.id.clone(), want));
                }
            }
        }
        0 if calm_for >= t.calm_secs && since_step >= t.step_secs => {
            let ours: Vec<&Running> = ours.filter(|j| j.controls.by_pressure).collect();
            // Unfreeze first, the job bringing back the least memory first,
            // and only when memory has room for it; then undo slowing.
            let thaw = ours
                .iter()
                .filter(|j| j.controls.frozen && (j.swapped as i64) <= headroom)
                .min_by_key(|j| j.swapped);
            if let Some(j) = thaw {
                changes.push((j.id.clone(), slowed(j)));
            } else if !ours.iter().any(|j| j.controls.frozen)
                && let Some(j) = ours.first()
            {
                changes.push((j.id.clone(), Controls::default()));
            }
        }
        _ => {}
    }
    changes
}

/// What the dashboard and `ats why` show about pressure.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PressureStatus {
    pub level: u8,
    pub signals: Signals,
    /// Seconds at level 0; controls are undone after the calm period.
    pub calm_for: f64,
    /// Memory in use by managed jobs, shared build daemons, and everything
    /// else.
    pub managed: u64,
    pub shared: u64,
    pub unmanaged: u64,
    /// Whether managed jobs or other processes hold most of the memory.
    pub source: &'static str,
    /// Changes made in the last second.
    pub changes: Vec<(String, Controls)>,
    /// Jobs frozen for more than 30 minutes of awake time.
    pub alerts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn levels_from_signals() {
        let s = |ins, outs, sys, over| Signals {
            system_level: sys,
            swap_ins: ins,
            swap_outs: outs,
            over_reservation: over,
        };
        assert_eq!(
            level(&s(0.0, 0.0, 2, false), &t()),
            0,
            "a steady warning level alone"
        );
        assert_eq!(level(&s(0.0, 0.0, 1, true), &t()), 1);
        assert_eq!(level(&s(0.0, 100.0, 1, false), &t()), 1);
        assert_eq!(level(&s(0.0, 300.0, 1, false), &t()), 2);
        assert_eq!(level(&s(0.0, 0.0, 4, false), &t()), 2);
        assert_eq!(level(&s(800.0, 300.0, 1, false), &t()), 3);
    }

    #[test]
    fn higher_levels_need_confirmation_and_calm_is_timed() {
        let mut tr = Tracker::default();
        assert_eq!(tr.update(1, 0.0, &t()), 1, "admission stops at once");
        assert_eq!(tr.update(3, 1.0, &t()), 1);
        assert_eq!(tr.update(3, 2.0, &t()), 1);
        assert_eq!(tr.update(3, 4.5, &t()), 3, "held for 3 seconds");
        assert_eq!(tr.update(2, 5.0, &t()), 2, "falling is immediate");
        assert_eq!(tr.update(0, 6.0, &t()), 0);
        assert_eq!(tr.calm_for(66.0), 60.0);
        assert_eq!(
            tr.update(3, 67.0, &t()),
            1,
            "a brief spike only stops admission"
        );
        assert_eq!(tr.calm_for(67.0), 0.0);
    }

    fn job(id: &str, freezable: bool, jobserver: Option<u32>) -> Running {
        Running {
            id: id.into(),
            freezable,
            jobserver,
            controls: Controls::default(),
            over_prediction: false,
            ending_soon: false,
            swapped: 0,
        }
    }

    fn change<'a>(c: &'a [(String, Controls)], id: &str) -> Option<&'a Controls> {
        c.iter().find(|(j, _)| j == id).map(|(_, c)| c)
    }

    #[test]
    fn slowing_and_freezing() {
        let mut over = job("over", true, None);
        over.over_prediction = true;
        let mut ending = job("ending", true, None);
        ending.ending_soon = true;
        let mut manual = job("manual", true, None);
        manual.controls.background = true;
        let jobs = [
            job("build", true, Some(9)),
            job("script", true, None),
            job("test", false, None),
            over,
            ending,
            manual,
        ];

        let c = respond(2, &jobs, 0.0, 0.0, 0, &t());
        let b = change(&c, "build").unwrap();
        assert!(b.background && b.tokens == Some(1) && b.duty.is_none() && !b.frozen);
        assert_eq!(
            change(&c, "script").unwrap().duty,
            Some(0.5),
            "no jobserver: duty cycle"
        );
        let tc = change(&c, "test").unwrap();
        assert!(
            tc.background && tc.duty.is_none(),
            "never stopped, only background"
        );
        for kept in ["over", "ending", "manual"] {
            assert!(change(&c, kept).is_none(), "{kept} is left running");
        }

        let c = respond(3, &jobs, 0.0, 0.0, 0, &t());
        assert!(change(&c, "build").unwrap().frozen && change(&c, "script").unwrap().frozen);
        assert!(!change(&c, "test").unwrap().frozen);
    }

    #[test]
    fn recovery_thaws_first_one_at_a_time_then_undoes_slowing() {
        let mut a = job("a", true, None);
        a.controls = frozen(&a);
        a.swapped = 2 << 30;
        let mut b = job("b", true, None);
        b.controls = frozen(&b);
        b.swapped = 1 << 30;
        let mut c = job("c", false, None);
        c.controls = slowed(&c);
        let jobs = [a, b, c];

        assert!(
            respond(0, &jobs, 30.0, 10.0, i64::MAX, &t()).is_empty(),
            "not calm long enough"
        );
        assert!(
            respond(0, &jobs, 61.0, 1.0, i64::MAX, &t()).is_empty(),
            "too soon after the last step"
        );
        let step = respond(0, &jobs, 61.0, 6.0, i64::MAX, &t());
        assert_eq!(step.len(), 1);
        assert_eq!(step[0].0, "b", "least memory to bring back first");
        assert!(
            !step[0].1.frozen && step[0].1.background,
            "thawed, still slowed"
        );
        assert!(
            respond(0, &jobs, 61.0, 6.0, (1 << 30) - 1, &t()).is_empty(),
            "no room to thaw, and slowing stays while frozen jobs remain"
        );

        let thawed: Vec<Running> = jobs
            .iter()
            .cloned()
            .map(|mut j| {
                j.controls.frozen = false;
                j
            })
            .collect();
        let step = respond(0, &thawed, 61.0, 6.0, i64::MAX, &t());
        assert_eq!(step, vec![("a".to_string(), Controls::default())]);
        // Level 2 keeps frozen jobs frozen until the calm period.
        assert!(change(&respond(2, &jobs, 0.0, 0.0, 0, &t()), "a").is_none());
    }
}
