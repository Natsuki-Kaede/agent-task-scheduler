//! Process facts the daemon uses to recognize executors and job processes
//! (design 7.1, 9): a process is identified by its id and start time, and
//! job processes descend from their executor or share the command's process
//! group.

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

/// One process as the system lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub start: String,
}

/// Every process the system lists.
pub fn list() -> Vec<Proc> {
    imp::all_pids().into_iter().filter_map(imp::proc).collect()
}

mod imp {
    use std::mem::{MaybeUninit, size_of};

    use super::Proc;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_processes_by_start_time() {
        let me = std::process::id() as i32;
        let start = start_time(me).unwrap();
        assert!(alive(me, &start));
        assert!(!alive(me, "0.000000"));

        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let listed = list().into_iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(listed.ppid, me);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(start_time(pid).is_none());
    }
}
