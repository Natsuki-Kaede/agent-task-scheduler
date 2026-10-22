//! Clocks (design 9.6). `elapsed` includes system sleep and drives the
//! bounded wait; `awake` excludes sleep and drives the freeze alert. Both
//! ignore wall-clock adjustments. Their origins differ (on macOS by a few
//! seconds even without sleep), so only compare readings of the same clock.

use std::time::Duration;

#[cfg(target_os = "macos")]
const ELAPSED_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC; // keeps counting during sleep on macOS
#[cfg(target_os = "macos")]
const AWAKE_CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;

#[cfg(target_os = "linux")]
const ELAPSED_CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
#[cfg(target_os = "linux")]
const AWAKE_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;

fn read(clock: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the provided timespec.
    let r = unsafe { libc::clock_gettime(clock, &mut ts) };
    assert_eq!(r, 0, "clock_gettime({clock}) failed");
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Time since boot, including sleep.
pub fn elapsed() -> Duration {
    read(ELAPSED_CLOCK)
}

/// Time since boot, excluding sleep.
pub fn awake() -> Duration {
    read(AWAKE_CLOCK)
}

/// A point in time measured on the elapsed clock.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Option<Duration>);

impl Deadline {
    /// A deadline `after` from now; `None` never expires.
    pub fn after(after: Option<Duration>) -> Self {
        Deadline(after.map(|d| elapsed() + d))
    }

    /// Time left, or `None` for a deadline that never expires.
    pub fn remaining(&self) -> Option<Duration> {
        self.0.map(|at| at.saturating_sub(elapsed()))
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_some_and(|r| r.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_advance() {
        let (e1, a1) = (elapsed(), awake());
        std::thread::sleep(Duration::from_millis(20));
        let (e2, a2) = (elapsed(), awake());
        assert!(e2 > e1 && a2 > a1);
    }

    #[test]
    fn deadlines() {
        assert!(Deadline::after(Some(Duration::ZERO)).expired());
        assert!(!Deadline::after(Some(Duration::from_secs(60))).expired());
        assert!(Deadline::after(None).remaining().is_none());
    }
}
