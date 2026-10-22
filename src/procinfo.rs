//! Process facts the daemon uses to recognize executors and job processes
//! (design 7.3, 9.1): a process is identified by its id and start time, job
//! processes descend from their executor or share its process group, and
//! they carry the job id in their environment.

/// Start time of a process, or `None` when it does not exist. Together with
/// the process id it identifies one process, since ids are reused.
pub fn start_time(pid: i32) -> Option<String> {
    imp::proc(pid).map(|p| p.start)
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
        .filter(|&pid| command_line(pid).is_some_and(|c| c.env.contains(&wanted)))
        .collect()
}

/// One process as the system lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub uid: u32,
    pub start: String,
}

/// Every process the system lists.
pub fn list() -> Vec<Proc> {
    imp::all_pids().into_iter().filter_map(imp::proc).collect()
}

/// Arguments and environment of a process. The system hides the
/// environment of some programs (on macOS, its own system programs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandLine {
    pub args: Vec<String>,
    pub env: Vec<String>,
}

pub fn command_line(pid: i32) -> Option<CommandLine> {
    imp::command_line(pid)
}

/// The current user's id.
pub fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::mem::{MaybeUninit, size_of};

    use super::{CommandLine, Proc};

    pub fn proc(pid: i32) -> Option<Proc> {
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
        let i = unsafe { info.assume_init() };
        Some(Proc {
            pid,
            ppid: i.pbi_ppid as i32,
            pgid: i.pbi_pgid as i32,
            uid: i.pbi_uid,
            start: format!("{}.{:06}", i.pbi_start_tvsec, i.pbi_start_tvusec),
        })
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

    /// From KERN_PROCARGS2: argc, the executable path, padding, the
    /// arguments, then the environment.
    pub fn command_line(pid: i32) -> Option<CommandLine> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks for the size.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer holds `size` bytes.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
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
        let text = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
        let mut strings = rest.split(|&b| b == 0);
        let args = strings.by_ref().take(argc).map(text).collect();
        let env = strings.take_while(|s| !s.is_empty()).map(text).collect();
        Some(CommandLine { args, env })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::unix::fs::MetadataExt;

    use super::{CommandLine, Proc};

    pub fn proc(pid: i32) -> Option<Proc> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesized command name, starting at field 3.
        let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split(' ').collect();
        Some(Proc {
            pid,
            ppid: f.get(1)?.parse().ok()?,
            pgid: f.get(2)?.parse().ok()?,
            uid: std::fs::metadata(format!("/proc/{pid}")).ok()?.uid(),
            start: f.get(19)?.to_string(),
        })
    }

    pub fn all_pids() -> Vec<i32> {
        std::fs::read_dir("/proc")
            .map(|d| {
                d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn strings(file: &str) -> Vec<String> {
        std::fs::read(file)
            .unwrap_or_default()
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }

    pub fn command_line(pid: i32) -> Option<CommandLine> {
        std::fs::metadata(format!("/proc/{pid}")).ok()?;
        Some(CommandLine {
            args: strings(&format!("/proc/{pid}/cmdline")),
            env: strings(&format!("/proc/{pid}/environ")),
        })
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
        let listed = list().into_iter().find(|p| p.pid == pid).unwrap();
        assert_eq!((listed.ppid, listed.uid), (me, uid()));
        assert_eq!(command_line(pid).unwrap().args, ["sleep", "5"]);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(start_time(pid).is_none());
    }
}
