//! Measurement (design 9): system memory and swap-out rate every second,
//! and each job's private anonymous memory. Per-process readings cost up to
//! a few milliseconds (a region walk on macOS) and run every five seconds
//! and for new processes.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;

use crate::procinfo::{self, Proc};
use crate::timing;

pub use os::system;

/// Whole-machine readings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct System {
    pub mem_total: u64,
    /// Memory in use, as the system counts it (U in design 10.1).
    pub mem_used: u64,
    /// The system's own pressure level: 1 normal, 4 critical (macOS also
    /// reports 2, warning).
    pub pressure: u8,
    /// Bytes swapped out since boot.
    pub swap_out_bytes: u64,
    pub cores: u32,
}

/// One process's private anonymous memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Memory {
    pub private_resident: u64,
    /// Compressed or swapped out, at uncompressed size.
    pub private_swapped: u64,
}

/// Memory of a job's processes.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JobUsage {
    pub processes: Vec<i32>,
    /// Private memory resident now (M in design 10.1).
    pub private_resident: u64,
    /// Private memory including compressed and swapped-out parts.
    pub private_total: u64,
    /// The largest `private_total` seen, which a reservation is raised to.
    pub peak_private_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    pub at: String,
    pub system: Option<System>,
    /// Bytes swapped out per second since the previous sample.
    pub swap_out_rate: f64,
    pub jobs: BTreeMap<String, JobUsage>,
}

/// Where a job's processes start: the executor's children and the
/// command's process group.
#[derive(Debug, Clone)]
pub struct JobRoot {
    pub id: String,
    pub executor: Option<i32>,
    pub pgid: Option<i32>,
}

/// Assigns processes to jobs: the executor's descendants and every process
/// in the command's group, with their descendants.
pub fn attribute(procs: &[Proc], jobs: &[JobRoot]) -> BTreeMap<String, Vec<i32>> {
    let mut children: HashMap<i32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut taken: HashSet<i32> = HashSet::new();
    let mut out = BTreeMap::new();
    for job in jobs {
        let executor_children = job
            .executor
            .and_then(|e| children.get(&e))
            .into_iter()
            .flatten()
            .copied();
        let group = procs.iter().filter(|p| job.pgid == Some(p.pgid));
        let mut pids = Vec::new();
        let mut stack: Vec<&Proc> = executor_children.chain(group).collect();
        while let Some(p) = stack.pop() {
            if taken.insert(p.pid) {
                pids.push(p.pid);
                stack.extend(children.get(&p.pid).into_iter().flatten().copied());
            }
        }
        out.insert(job.id.clone(), pids);
    }
    out
}

/// Cores this process may use: the CPU affinity mask and, on Linux, the
/// cgroup CPU quota count.
fn cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// Per-process memory is read this often; new processes are read at once.
const MEMORY_EVERY: u64 = 5;

type Key = (i32, String);

#[derive(Default)]
pub struct Sampler {
    memory: HashMap<Key, Memory>,
    /// The peak of each job so far.
    peaks: HashMap<String, u64>,
    /// The previous swap-out count and when it was read.
    last: Option<(u64, Duration)>,
    ticks: u64,
}

impl Sampler {
    pub fn sample(&mut self, jobs: &[JobRoot]) -> Snapshot {
        let system = system();
        let now = timing::elapsed();
        let mut swap_out_rate = 0.0;
        if let Some(s) = &system {
            if let Some((before, at)) = self.last {
                let secs = now.saturating_sub(at).as_secs_f64();
                if secs > 0.0 {
                    swap_out_rate = s.swap_out_bytes.saturating_sub(before) as f64 / secs;
                }
            }
            self.last = Some((s.swap_out_bytes, now));
        }

        let procs = procinfo::list();
        let starts: HashMap<i32, &str> = procs.iter().map(|p| (p.pid, p.start.as_str())).collect();
        self.memory
            .retain(|(pid, start), _| starts.get(pid) == Some(&start.as_str()));
        let walk_all = self.ticks.is_multiple_of(MEMORY_EVERY);
        self.ticks += 1;

        let mut snapshot = Snapshot {
            at: crate::store::now(),
            system,
            swap_out_rate,
            jobs: BTreeMap::new(),
        };
        for (job, pids) in attribute(&procs, jobs) {
            let mut u = JobUsage {
                processes: pids.clone(),
                ..JobUsage::default()
            };
            for pid in pids {
                let key = (
                    pid,
                    starts.get(&pid).copied().unwrap_or_default().to_string(),
                );
                if (walk_all || !self.memory.contains_key(&key))
                    && let Some(m) = os::memory(pid)
                {
                    self.memory.insert(key.clone(), m);
                }
                if let Some(m) = self.memory.get(&key) {
                    u.private_resident += m.private_resident;
                    u.private_total += m.private_resident + m.private_swapped;
                }
            }
            let peak = self.peaks.entry(job.clone()).or_default();
            *peak = (*peak).max(u.private_total);
            u.peak_private_total = *peak;
            snapshot.jobs.insert(job, u);
        }
        self.peaks.retain(|job, _| snapshot.jobs.contains_key(job));
        snapshot
    }
}

#[cfg(test)]
mod tests;
