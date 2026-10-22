//! Process facts the daemon uses to recognize executors and job processes
//! across restarts (design 7.3, 9.1): a process is identified by its id and
//! start time, and job processes carry the job id in their environment.

/// Start time of a process, or `None` when it does not exist. Together with
/// the process id it identifies one process, since ids are reused.
pub fn start_time(pid: i32) -> Option<String> {
    imp::start_time(pid)
}

/// True when the process with this id and start time still exists.
pub fn alive(pid: i32, start: &str) -> bool {
    start_time(pid).is_some_and(|s| s == start)
}

/// True when any process in the group still exists.
pub fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

pub fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: sends a signal to a job's own process group.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// Processes whose environment has `name=value`.
pub fn with_env(name: &str, value: &str) -> Vec<i32> {
    let wanted = format!("{name}={value}");
    imp::all_pids()
        .into_iter()
        .filter(|&pid| imp::environment(pid).is_some_and(|env| env.contains(&wanted)))
        .collect()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::mem::{MaybeUninit, size_of};

    pub fn start_time(pid: i32) -> Option<String> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // SAFETY: the call filled the whole struct.
        let info = unsafe { info.assume_init() };
        Some(format!(
            "{}.{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }

    pub fn all_pids() -> Vec<i32> {
        // SAFETY: a null buffer asks for the number of processes.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; n as usize + 64];
        let bytes = (pids.len() * size_of::<i32>()) as i32;
        // SAFETY: the buffer holds `bytes` bytes.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(n.max(0) as usize);
        pids
    }

    /// The environment of a process owned by this user, from KERN_PROCARGS2:
    /// argc, the executable path, padding, argv, then the environment.
    pub fn environment(pid: i32) -> Option<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks for the size.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer holds `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
        let mut rest = &buf[4..];
        // Skip the executable path and the padding after it.
        let path_end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[path_end..];
        let first = rest.iter().position(|&b| b != 0)?;
        rest = &rest[first..];
        let mut strings = rest.split(|&b| b == 0);
        for _ in 0..argc {
            strings.next()?;
        }
        Some(
            strings
                .take_while(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn start_time(pid: i32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesized command name; start time is field 22.
        let after = &stat[stat.rfind(')')? + 2..];
        after.split(' ').nth(19).map(str::to_string)
    }

    pub fn all_pids() -> Vec<i32> {
        std::fs::read_dir("/proc")
            .map(|d| {
                d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn environment(pid: i32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        Some(
            raw.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_this_process_by_start_time_and_environment() {
        let me = std::process::id() as i32;
        let start = start_time(me).unwrap();
        assert!(alive(me, &start));
        assert!(!alive(me, "0.000000"));
        // cargo runs tests with the package name in the environment. macOS
        // hides the environment of its own system programs, so the test
        // looks at this process.
        assert!(with_env("CARGO_PKG_NAME", env!("CARGO_PKG_NAME")).contains(&me));

        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        assert!(start_time(pid).is_some());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(start_time(pid).is_none());
    }
}
