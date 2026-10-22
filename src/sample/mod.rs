//! Resource measurement (design 9): system pressure, and per-job CPU, disk
//! and private memory, sampled every second. Region walks, which cost a few
//! milliseconds per process, run every few seconds and for new processes.
//! Raw samples stay in memory; the daemon stores 5-second summaries.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(target_os = "macos"))]
mod os {
    //! Other platforms report no measurements until their samplers land.
    use super::{Memory, System, Usage};
    pub fn system() -> Option<System> {
        None
    }
    pub fn usage(_: i32) -> Option<Usage> {
        None
    }
    pub fn memory(_: i32) -> Option<Memory> {
        None
    }
    pub fn boot_id() -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string())
    }
}

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;

use crate::procinfo::{self, CommandLine, Proc};
use crate::profile::Profiles;
use crate::timing;

pub use os::boot_id;

/// Whole-machine readings (design 9.3). Counters are totals since boot.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct System {
    pub mem_total: u64,
    /// Memory in use, as the system counts it (U in design 10.1).
    pub mem_used: u64,
    pub compressed: u64,
    pub swap_used: u64,
    /// 1 normal, 2 warning, 4 critical.
    pub pressure: u8,
    pub page_ins: u64,
    pub page_outs: u64,
    pub swap_ins: u64,
    pub swap_outs: u64,
    pub compressions: u64,
    pub decompressions: u64,
    pub perf_cores: u32,
    pub efficiency_cores: u32,
}

/// Per-second rates of the system counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rates {
    pub page_ins: f64,
    pub page_outs: f64,
    pub swap_ins: f64,
    pub swap_outs: f64,
    pub compressions: f64,
    pub decompressions: f64,
}

/// One process's cumulative usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu_ns: u64,
    /// For reference only: it counts shared memory in every process that
    /// maps it (P2).
    pub footprint: u64,
    pub disk_read: u64,
    pub disk_write: u64,
}

/// One process's private anonymous memory (design 9.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Memory {
    pub private_resident: u64,
    /// Compressed or swapped out, at uncompressed size.
    pub private_swapped: u64,
}

/// Usage of a job's processes, or of a shared daemon's.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GroupUsage {
    pub processes: Vec<i32>,
    /// Cores kept busy over the last interval.
    pub cpu_cores: f64,
    /// CPU time used since measuring began.
    pub cpu_seconds: f64,
    /// Private memory resident now (M in design 10.1).
    pub private_resident: u64,
    /// Private memory including compressed and swapped-out parts.
    pub private_total: u64,
    /// The largest `private_total` seen (what R learns from).
    pub peak_private_total: u64,
    /// Bytes read and written by its current processes.
    pub disk_read: u64,
    pub disk_write: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sleep {
    pub at: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    pub at: String,
    pub boot_id: Option<String>,
    pub system: Option<System>,
    pub rates: Rates,
    pub jobs: BTreeMap<String, GroupUsage>,
    /// Shared daemons, metered apart from any job (design 9.1).
    pub shared: BTreeMap<String, GroupUsage>,
    pub last_sleep: Option<Sleep>,
}

/// Where a job's processes start: the executor's children, and the
/// command's process group. `since` is the executor's start time; a
/// process that left the tree is only matched by its environment marker
/// if it started later.
#[derive(Debug, Clone)]
pub struct JobRoot {
    pub id: String,
    pub executor: Option<i32>,
    pub pgid: Option<i32>,
    pub since: f64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Attribution {
    pub jobs: BTreeMap<String, Vec<i32>>,
    pub shared: BTreeMap<String, Vec<i32>>,
}

/// Assigns processes to jobs (design 9.1): the process tree and process
/// group first, then the environment marker for processes that left the
/// tree. Shared daemons (design 8.2: helpers that leave the process tree
/// and serve several jobs) are found among processes reparented to launchd,
/// taken out first and metered on their own.
pub fn attribute(
    procs: &[Proc],
    jobs: &[JobRoot],
    me: u32,
    mut daemon_of: impl FnMut(&Proc) -> Option<String>,
    mut marker_of: impl FnMut(&Proc) -> Option<String>,
) -> Attribution {
    let mut children: HashMap<i32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut taken: HashSet<i32> = HashSet::new();
    let tree = |root: &Proc, taken: &mut HashSet<i32>| -> Vec<i32> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            if taken.insert(p.pid) {
                out.push(p.pid);
                stack.extend(children.get(&p.pid).into_iter().flatten().copied());
            }
        }
        out
    };
    let mut result = Attribution::default();

    for p in procs.iter().filter(|p| p.ppid == 1 && p.uid == me) {
        if !taken.contains(&p.pid)
            && let Some(name) = daemon_of(p)
        {
            let pids = tree(p, &mut taken);
            result.shared.entry(name).or_default().extend(pids);
        }
    }
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    for job in jobs {
        let mut pids = Vec::new();
        let executor_children = job
            .executor
            .and_then(|e| children.get(&e))
            .into_iter()
            .flatten()
            .copied();
        let leader = job.pgid.and_then(|g| by_pid.get(&g)).copied();
        let group = procs
            .iter()
            .filter(|p| job.pgid.is_some_and(|g| p.pgid == g));
        for root in executor_children.chain(leader).chain(group) {
            pids.extend(tree(root, &mut taken));
        }
        result.jobs.insert(job.id.clone(), pids);
    }
    let earliest = jobs.iter().map(|j| j.since).fold(f64::INFINITY, f64::min);
    for p in procs {
        let candidate = p.ppid == 1 && p.uid == me && !taken.contains(&p.pid);
        if !candidate || p.start.parse::<f64>().map_or(true, |s| s < earliest) {
            continue;
        }
        if let Some(job) = marker_of(p).filter(|j| result.jobs.contains_key(j)) {
            let pids = tree(p, &mut taken);
            result.jobs.get_mut(&job).expect("known job").extend(pids);
        }
    }
    result
}

/// A shared daemon matches when every word of its profile name appears in
/// the command line, ignoring case.
fn daemon_names(profiles: &Profiles) -> Vec<(String, Vec<String>)> {
    let mut names: Vec<(String, Vec<String>)> = profiles
        .all()
        .iter()
        .flat_map(|p| &p.daemons)
        .filter(|d| d.shared)
        .map(|d| {
            (
                d.name.clone(),
                d.name
                    .to_lowercase()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
            )
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Region walks run this often; new processes are walked at once.
const MEMORY_EVERY: u64 = 5;
/// A gap between the two clocks larger than this is reported as sleep.
const SLEEP_THRESHOLD: Duration = Duration::from_secs(2);

type Key = (i32, String);

struct Last {
    elapsed: Duration,
    awake: Duration,
    system: Option<System>,
}

pub struct Sampler {
    me: u32,
    daemons: Vec<(String, Vec<String>)>,
    command_lines: HashMap<Key, Option<CommandLine>>,
    cpu: HashMap<Key, u64>,
    memory: HashMap<Key, Memory>,
    /// CPU seconds and peak memory of each job and daemon so far.
    totals: HashMap<String, (f64, u64)>,
    last: Option<Last>,
    last_sleep: Option<Sleep>,
    ticks: u64,
    boot_id: Option<String>,
}

impl Sampler {
    pub fn new(profiles: &Profiles) -> Self {
        Sampler {
            me: procinfo::uid(),
            daemons: daemon_names(profiles),
            command_lines: HashMap::new(),
            cpu: HashMap::new(),
            memory: HashMap::new(),
            totals: HashMap::new(),
            last: None,
            last_sleep: None,
            ticks: 0,
            boot_id: os::boot_id(),
        }
    }

    pub fn sample(&mut self, jobs: &[JobRoot]) -> Snapshot {
        let (elapsed, awake) = (timing::elapsed(), timing::awake());
        let system = os::system();
        let interval = self
            .last
            .as_ref()
            .map(|l| elapsed.saturating_sub(l.elapsed));
        let rates = match (&self.last, &system) {
            (
                Some(Last {
                    system: Some(before),
                    awake: a0,
                    ..
                }),
                Some(now),
            ) => rates(before, now, awake.saturating_sub(*a0)),
            _ => Rates::default(),
        };
        if let Some(last) = &self.last {
            let slept = elapsed
                .saturating_sub(last.elapsed)
                .saturating_sub(awake.saturating_sub(last.awake));
            if slept > SLEEP_THRESHOLD {
                tracing::info!(seconds = slept.as_secs_f64(), "system slept");
                self.last_sleep = Some(Sleep {
                    at: crate::store::now(),
                    seconds: slept.as_secs_f64(),
                });
            }
        }
        self.last = Some(Last {
            elapsed,
            awake,
            system: system.clone(),
        });

        let procs = procinfo::list();
        let live: HashSet<Key> = procs.iter().map(|p| (p.pid, p.start.clone())).collect();
        self.command_lines.retain(|k, _| live.contains(k));
        self.cpu.retain(|k, _| live.contains(k));
        self.memory.retain(|k, _| live.contains(k));

        let attribution = {
            let daemons = &self.daemons;
            let lines = RefCell::new(&mut self.command_lines);
            let line = |p: &Proc| -> Option<CommandLine> {
                lines
                    .borrow_mut()
                    .entry((p.pid, p.start.clone()))
                    .or_insert_with(|| procinfo::command_line(p.pid))
                    .clone()
            };
            let marker = format!("{}=", crate::executor::JOB_ENV);
            attribute(
                &procs,
                jobs,
                self.me,
                |p| {
                    let text = line(p)?.args.join(" ").to_lowercase();
                    daemons
                        .iter()
                        .find(|(_, words)| words.iter().all(|w| text.contains(w.as_str())))
                        .map(|(n, _)| n.clone())
                },
                |p| {
                    line(p)?
                        .env
                        .iter()
                        .find_map(|e| e.strip_prefix(&marker).map(str::to_string))
                },
            )
        };

        let starts: HashMap<i32, String> = procs.iter().map(|p| (p.pid, p.start.clone())).collect();
        let walk_all = self.ticks.is_multiple_of(MEMORY_EVERY);
        self.ticks += 1;
        // A job's own processes count all their CPU time; a shared daemon
        // counts what it uses from the first time it is seen.
        let group = |name: &str, pids: &[i32], own: bool, this: &mut Self| -> GroupUsage {
            let mut g = GroupUsage {
                processes: pids.to_vec(),
                ..GroupUsage::default()
            };
            let mut cpu_delta = 0u64;
            for &pid in pids {
                let key = (pid, starts.get(&pid).cloned().unwrap_or_default());
                if let Some(u) = os::usage(pid) {
                    let before = this.cpu.insert(key.clone(), u.cpu_ns);
                    let base = before.unwrap_or(if own { 0 } else { u.cpu_ns });
                    cpu_delta += u.cpu_ns.saturating_sub(base);
                    g.disk_read += u.disk_read;
                    g.disk_write += u.disk_write;
                }
                if (walk_all || !this.memory.contains_key(&key))
                    && let Some(m) = os::memory(pid)
                {
                    this.memory.insert(key.clone(), m);
                }
                if let Some(m) = this.memory.get(&key) {
                    g.private_resident += m.private_resident;
                    g.private_total += m.private_resident + m.private_swapped;
                }
            }
            let total = this.totals.entry(name.to_string()).or_default();
            total.0 += cpu_delta as f64 / 1e9;
            total.1 = total.1.max(g.private_total);
            g.cpu_seconds = total.0;
            g.peak_private_total = total.1;
            g.cpu_cores = interval
                .filter(|d| !d.is_zero())
                .map_or(0.0, |d| cpu_delta as f64 / d.as_nanos() as f64);
            g
        };
        let mut snapshot = Snapshot {
            at: crate::store::now(),
            boot_id: self.boot_id.clone(),
            system,
            rates,
            last_sleep: self.last_sleep.clone(),
            ..Snapshot::default()
        };
        for (job, pids) in &attribution.jobs {
            let usage = group(job, pids, true, self);
            snapshot.jobs.insert(job.clone(), usage);
        }
        for (name, pids) in &attribution.shared {
            let usage = group(&format!("shared:{name}"), pids, false, self);
            snapshot.shared.insert(name.clone(), usage);
        }
        let current: HashSet<String> = attribution
            .jobs
            .keys()
            .cloned()
            .chain(attribution.shared.keys().map(|n| format!("shared:{n}")))
            .collect();
        self.totals.retain(|k, _| current.contains(k));
        snapshot
    }
}

fn rates(before: &System, now: &System, over: Duration) -> Rates {
    let secs = over.as_secs_f64();
    if secs <= 0.0 {
        return Rates::default();
    }
    let r = |a: u64, b: u64| b.saturating_sub(a) as f64 / secs;
    Rates {
        page_ins: r(before.page_ins, now.page_ins),
        page_outs: r(before.page_outs, now.page_outs),
        swap_ins: r(before.swap_ins, now.swap_ins),
        swap_outs: r(before.swap_outs, now.swap_outs),
        compressions: r(before.compressions, now.compressions),
        decompressions: r(before.decompressions, now.decompressions),
    }
}

#[cfg(test)]
mod tests;
