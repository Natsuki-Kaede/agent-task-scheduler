//! Recoverable process controls (design 10.6, 11): background QoS,
//! freezing with SIGSTOP/SIGCONT, and duty-cycle slowing. Each can be
//! undone by the same user; the system cannot report the QoS state, so the
//! caller keeps track of what it set.

use serde::{Deserialize, Serialize};

/// What a running job is held to (design 11: the slowing and freezing
/// steps). The default is no control.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Controls {
    /// Background QoS.
    pub background: bool,
    pub frozen: bool,
    /// Fraction of each second the job may run, by SIGSTOP/SIGCONT.
    pub duty: Option<f64>,
    /// Jobserver tokens to leave in the job's pool; the executor takes the
    /// rest back as they are returned.
    pub tokens: Option<u32>,
}

impl Controls {
    pub fn slowed(&self) -> bool {
        self.background || self.duty.is_some() || self.tokens.is_some()
    }

    /// Needs SIGSTOP, which some jobs must never get (design 11).
    pub fn stops(&self) -> bool {
        self.frozen || self.duty.is_some()
    }

    /// The job state these controls put a running job in.
    pub fn state(&self) -> &'static str {
        if self.frozen {
            "frozen"
        } else if self.slowed() {
            "throttled"
        } else {
            "running"
        }
    }
}

/// Moves a process to or from background QoS: lower CPU priority, the
/// efficiency cores and lower disk priority on macOS. Children started
/// afterwards inherit it. Returns false when the process is gone.
#[cfg(target_os = "macos")]
pub fn set_background(pid: i32, on: bool) -> bool {
    const PRIO_DARWIN_PROCESS: i32 = 4;
    const PRIO_DARWIN_BG: i32 = 0x1000;
    // SAFETY: changes the scheduling policy of a process of this user.
    unsafe {
        libc::setpriority(
            PRIO_DARWIN_PROCESS,
            pid as u32,
            if on { PRIO_DARWIN_BG } else { 0 },
        ) == 0
    }
}

/// Other platforms have no recoverable equivalent wired up yet (design
/// 10.6: Linux needs cgroup delegation).
#[cfg(not(target_os = "macos"))]
pub fn set_background(_pid: i32, _on: bool) -> bool {
    false
}

pub fn stop(pid: i32) -> bool {
    // SAFETY: sends a signal to a job's own process.
    unsafe { libc::kill(pid, libc::SIGSTOP) == 0 }
}

pub fn resume(pid: i32) -> bool {
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) == 0 }
}

/// Whether a duty-cycled job runs at `phase` seconds into its cycle: it
/// runs for the first `fraction` of each `period`.
pub fn duty_runs(phase: f64, period: f64, fraction: f64) -> bool {
    phase.rem_euclid(period) < period * fraction.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(pid: u32, field: &str) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn freezes_and_resumes() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(stop(pid as i32));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            ps(pid, "state").starts_with('T'),
            "stopped: {}",
            ps(pid, "state")
        );
        assert!(resume(pid as i32));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!ps(pid, "state").starts_with('T'));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!stop(pid as i32), "a reaped process cannot be signaled");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn background_qos_lowers_priority_and_reverts() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        let normal = ps(pid, "pri");
        assert!(set_background(pid as i32, true));
        assert_eq!(ps(pid, "pri"), "4", "background QoS (P2: 31 -> 4)");
        assert!(set_background(pid as i32, false));
        assert_eq!(ps(pid, "pri"), normal);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn duty_cycle() {
        assert!(duty_runs(0.1, 1.0, 0.25));
        assert!(!duty_runs(0.3, 1.0, 0.25));
        assert!(duty_runs(2.2, 1.0, 0.25), "phase wraps every period");
        assert!(!duty_runs(0.0, 1.0, 0.0));
        assert!(duty_runs(0.99, 1.0, 1.0));
    }
}
