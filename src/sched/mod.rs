//! Admission and ordering (design 10.1–10.3, 10.7), as a pure function of
//! one scheduling round's inputs, so every rule can be tested with
//! simulated resources.

pub mod locks;
pub mod predict;
pub mod pressure;

use serde::Serialize;

use locks::Lock;

/// Memory and CPU the machine offers this round.
#[derive(Debug, Clone, PartialEq)]
pub struct Capacity {
    /// C: the calibrated memory budget.
    pub memory: u64,
    /// U: memory in use now, as the system counts it.
    pub used: u64,
    pub cores: f64,
    /// Target CPU use as a multiple of `cores`; above 1 allows mild
    /// oversubscription (CPU is a soft constraint).
    pub cpu_target: f64,
}

/// A job holding a lease: admitted, running, throttled, frozen or awaiting
/// takeover.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub id: String,
    /// Rᵢ: the reservation for its private memory.
    pub reserved: u64,
    /// Mᵢ: its private memory resident now.
    pub resident: u64,
    /// Bₕ: its budget for memory outside the private range.
    pub extra: u64,
    pub cores: f64,
    pub locks: Vec<Lock>,
    /// Predicted seconds until it ends, when known.
    pub ends_in: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub id: String,
    /// Seconds since submission.
    pub age: f64,
    pub reserved: u64,
    pub extra: u64,
    pub cores: f64,
    /// Predicted run time, when known.
    pub seconds: Option<f64>,
    pub locks: Vec<Lock>,
    /// (predecessor job, its state) from `--after`.
    pub deps: Vec<(String, Dep)>,
    /// `--now`: skips ordering and admission checks, not conflicts.
    pub now: bool,
    /// Its executor is registered and can take a lease.
    pub ready: bool,
    /// Someone is waiting for it (a bounded wait or `ats wait`).
    pub awaited: bool,
    /// How recently the user interacted with its session, 1 = just now.
    pub session_recency: f64,
    /// Declared urgency: -1 low, 0 normal, 1 high.
    pub urgency: i8,
    /// Manual position from the dashboard; lower runs first.
    pub pinned: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Seconds between admission rounds (design 10.2: 3 s initially).
    pub interval: f64,
    /// Headroom below this fraction of C counts as tight: one admission per
    /// round.
    pub tight: f64,
    pub weights: Weights,
}

/// Ordering adjustments, in seconds of queue age (design 10.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    pub awaited: f64,
    pub session: f64,
    /// Per urgency step, capped at one step either way.
    pub urgency: f64,
    /// After this many seconds, waiting time counts extra.
    pub starvation_after: f64,
    pub starvation_rate: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval: 3.0,
            tight: 0.2,
            weights: Weights {
                awaited: 120.0,
                session: 60.0,
                urgency: 60.0,
                starvation_after: 600.0,
                starvation_rate: 1.0,
            },
        }
    }
}

pub struct Round<'a> {
    pub capacity: &'a Capacity,
    pub held: &'a [Held],
    pub waiting: &'a [Waiting],
    /// The ats pressure level (design 11); admission stops at 1 and above.
    pub pressure: u8,
    /// Seconds since the last admission, if any.
    pub since_admission: Option<f64>,
    /// False while executors reconnect after a restart (design 7.3).
    pub admitting: bool,
    pub settings: &'a Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    Recovering,
    Pressure {
        level: u8,
    },
    Interval,
    NoExecutor,
    Dependency {
        job: String,
    },
    Lock {
        resource: String,
        holder: String,
    },
    Memory {
        need: u64,
        free: i64,
    },
    Cpu {
        need: f64,
        free: f64,
    },
    /// Would delay the job at the head of the queue.
    Behind {
        head: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Admit,
    Wait {
        reason: Reason,
    },
    /// A predecessor failed; `ats unblock` or cancel decides (design 5.3).
    Blocked {
        job: String,
    },
    /// Its predicted peak exceeds the whole budget (design 5.1).
    CapacityInsufficient {
        need: u64,
        budget: u64,
    },
}

/// The ordering score: queue age plus adjustments (design 10.3).
fn score(w: &Waiting, s: &Weights) -> f64 {
    let starvation = (w.age - s.starvation_after).max(0.0) * s.starvation_rate;
    w.age
        + starvation
        + if w.awaited { s.awaited } else { 0.0 }
        + w.session_recency.clamp(0.0, 1.0) * s.session
        + f64::from(w.urgency.clamp(-1, 1)) * s.urgency
}

/// Waiting jobs in the order they are considered: pinned first, then by
/// score, then oldest first.
pub fn order<'a>(waiting: &'a [Waiting], s: &Settings) -> Vec<&'a Waiting> {
    let mut v: Vec<&Waiting> = waiting.iter().collect();
    v.sort_by(|a, b| {
        let pin = |w: &Waiting| w.pinned.unwrap_or(i64::MAX);
        pin(a)
            .cmp(&pin(b))
            .then(score(b, &s.weights).total_cmp(&score(a, &s.weights)))
            .then(b.age.total_cmp(&a.age))
    });
    v
}

/// H in design 10.1.
pub fn headroom(capacity: &Capacity, held: &[Held]) -> i64 {
    let pending: u64 = held
        .iter()
        .map(|h| h.reserved.saturating_sub(h.resident) + h.extra)
        .sum();
    capacity.memory as i64 - capacity.used as i64 - pending as i64
}

/// When the head of the queue could start: the time until enough held
/// jobs end to free `deficit` bytes, if their end times are known.
fn head_eta(held: &[Held], admitted: &[(f64, u64)], deficit: i64) -> Option<f64> {
    let mut ends: Vec<(f64, u64)> = held
        .iter()
        .map(|h| (h.ends_in, h.reserved.max(h.resident) + h.extra))
        .chain(admitted.iter().map(|&(s, m)| (Some(s), m)))
        .map(|(t, m)| (t.unwrap_or(f64::INFINITY), m))
        .collect();
    ends.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut freed = 0i64;
    for (t, m) in ends {
        freed += m as i64;
        if freed >= deficit {
            return t.is_finite().then_some(t);
        }
    }
    None
}

/// Decides every waiting job, in queue order.
pub fn decide(r: &Round) -> Vec<(String, Verdict)> {
    let c = r.capacity;
    let mut free = headroom(c, r.held);
    let mut cpu_free = c.cores * c.cpu_target - r.held.iter().map(|h| h.cores).sum::<f64>();
    let mut locks: Vec<(Lock, String)> = r
        .held
        .iter()
        .flat_map(|h| h.locks.iter().map(|l| (l.clone(), h.id.clone())))
        .collect();
    let interval_ok = r.since_admission.is_none_or(|s| s >= r.settings.interval);
    let mut admitted = 0;
    let mut admitted_ends: Vec<(f64, u64)> = Vec::new();
    let mut round_closed = false;
    // The first job held back only by memory or CPU, and when it could
    // start; later jobs must not delay it (design 10.3, backfilling).
    let mut head: Option<(String, Option<f64>, Vec<Lock>)> = None;
    let nothing_held = r.held.is_empty();

    let mut out = Vec::new();
    for w in order(r.waiting, r.settings) {
        let wait = |reason| Verdict::Wait { reason };
        let verdict = (|| {
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Failed) {
                return Verdict::Blocked { job: job.clone() };
            }
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Pending) {
                return wait(Reason::Dependency { job: job.clone() });
            }
            let need = w.reserved + w.extra;
            if need > c.memory && !w.now {
                return Verdict::CapacityInsufficient {
                    need,
                    budget: c.memory,
                };
            }
            if !r.admitting {
                return wait(Reason::Recovering);
            }
            if !w.ready {
                return wait(Reason::NoExecutor);
            }
            if let Some((lock, holder)) = locks
                .iter()
                .find(|(l, _)| w.locks.iter().any(|m| m.conflicts(l)))
            {
                return wait(Reason::Lock {
                    resource: lock.to_string(),
                    holder: holder.clone(),
                });
            }
            if w.now {
                return Verdict::Admit;
            }
            if r.pressure >= 1 {
                return wait(Reason::Pressure { level: r.pressure });
            }
            if !interval_ok || round_closed {
                return wait(Reason::Interval);
            }
            let mem_ok = need as i64 <= free;
            // CPU is soft: an idle machine takes any one job.
            let cpu_ok = w.cores <= cpu_free || (nothing_held && admitted == 0);
            if let Some((head_id, eta, head_locks)) = &head {
                let ends_first = w.seconds.zip(*eta).is_some_and(|(s, e)| s <= e);
                let takes_its_lock = w
                    .locks
                    .iter()
                    .any(|l| head_locks.iter().any(|h| h.conflicts(l)));
                if takes_its_lock || (!ends_first && (need > 0 || w.cores > 0.0)) {
                    return wait(Reason::Behind {
                        head: head_id.clone(),
                    });
                }
            }
            if mem_ok && cpu_ok {
                return Verdict::Admit;
            }
            if head.is_none() {
                let eta = head_eta(r.held, &admitted_ends, need as i64 - free);
                head = Some((w.id.clone(), eta, w.locks.clone()));
            }
            if !mem_ok {
                wait(Reason::Memory { need, free })
            } else {
                wait(Reason::Cpu {
                    need: w.cores,
                    free: cpu_free,
                })
            }
        })();
        if verdict == Verdict::Admit {
            free -= (w.reserved + w.extra) as i64;
            cpu_free -= w.cores;
            locks.extend(w.locks.iter().map(|l| (l.clone(), w.id.clone())));
            admitted_ends.push((w.seconds.unwrap_or(f64::INFINITY), w.reserved + w.extra));
            if !w.now {
                admitted += 1;
                if free < (c.memory as f64 * r.settings.tight) as i64 {
                    round_closed = true;
                }
            }
        }
        out.push((w.id.clone(), verdict));
    }
    out
}

#[cfg(test)]
mod tests;
