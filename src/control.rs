//! Background QoS (design 10.2): lower CPU priority, the efficiency cores
//! and lower disk priority. The same user can undo it; the system cannot
//! report it, so the caller keeps track of what it set.

/// Moves a process to or from background QoS. Children started afterwards
/// inherit it. Returns false when the process is gone.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pri(pid: u32) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "pri=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn background_qos_lowers_priority_and_reverts() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        let normal = pri(pid);
        assert!(set_background(pid as i32, true));
        assert_eq!(pri(pid), "4");
        assert!(set_background(pid as i32, false));
        assert_eq!(pri(pid), normal);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
