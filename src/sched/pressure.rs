//! Memory pressure (design 10.2): rising when swap-outs, averaged over 10
//! seconds, pass a threshold, or the system reports critical pressure. It
//! stops admission and moves running jobs to background QoS; 30 seconds
//! after it subsides both are undone. ats never freezes, pauses or
//! terminates a job.

use std::collections::VecDeque;

use serde::Serialize;

/// Swap-out pages per second, averaged, that count as rising pressure.
pub const SWAPOUT_RATE: f64 = 256.0;
const WINDOW: usize = 10;
/// Seconds without pressure before admission and QoS return to normal.
pub const CALM_SECS: f64 = 30.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PressureStatus {
    pub pressured: bool,
    /// Swap-out pages per second, averaged over the last 10 seconds.
    pub swap_outs: f64,
    /// The system's own level (macOS: 1 normal, 2 warning, 4 critical).
    pub system_level: u8,
    /// Memory resident in managed jobs and everything else.
    pub managed: u64,
    pub unmanaged: u64,
}

/// Pressure over time, from one reading a second.
#[derive(Debug, Default)]
pub struct Tracker {
    swap_outs: VecDeque<f64>,
    /// When pressure was last seen, in seconds.
    last_seen: Option<f64>,
}

impl Tracker {
    /// Adds this second's reading; returns the averaged swap-out rate and
    /// whether pressure is up.
    pub fn update(&mut self, swap_outs: f64, system_level: u8, now: f64) -> (f64, bool) {
        self.swap_outs.push_back(swap_outs);
        if self.swap_outs.len() > WINDOW {
            self.swap_outs.pop_front();
        }
        let average = self.swap_outs.iter().sum::<f64>() / self.swap_outs.len() as f64;
        if average >= SWAPOUT_RATE || system_level >= 4 {
            self.last_seen = Some(now);
        }
        let pressured = self.last_seen.is_some_and(|t| now - t < CALM_SECS);
        (average, pressured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_and_waits_for_calm() {
        let mut t = Tracker::default();
        // One spike of 2000 averages to 200 over ten seconds: no pressure.
        for s in 0..9 {
            t.update(0.0, 1, f64::from(s));
        }
        assert!(!t.update(2000.0, 1, 9.0).1);
        // Sustained swapping is pressure.
        for s in 10..20 {
            t.update(400.0, 1, f64::from(s));
        }
        assert!(t.update(400.0, 1, 20.0).1);
        // It stays until 30 calm seconds have passed.
        for s in 21..40 {
            t.update(0.0, 1, f64::from(s));
        }
        assert!(t.update(0.0, 1, 40.0).1);
        assert!(!t.update(0.0, 1, 60.0).1);
        // The system's critical level counts on its own.
        assert!(t.update(0.0, 4, 61.0).1);
    }
}
