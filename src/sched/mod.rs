//! Admission and ordering (design 10), as a pure function of one
//! scheduling round's inputs, so every rule can be tested with simulated
//! resources.

pub mod locks;
pub mod predict;
pub mod pressure;

use serde::Serialize;

use locks::Lock;

/// Memory the machine offers this round.
#[derive(Debug, Clone, PartialEq)]
pub struct Capacity {
    /// C: the memory budget.
    pub memory: u64,
    /// U: memory in use now, as the system counts it.
    pub used: u64,
}

/// A job holding a lease: admitted, running or awaiting takeover.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub id: String,
    /// Rᵢ: its reservation.
    pub reserved: u64,
    /// Mᵢ: the part of it resident now, already counted in U.
    pub resident: u64,
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
    /// Predicted run time, when known.
    pub seconds: Option<f64>,
    pub locks: Vec<Lock>,
    /// (predecessor job, its state) from `--after`.
    pub deps: Vec<(String, Dep)>,
    /// `--now`: skips the queue and memory admission, not conflicts.
    pub now: bool,
    /// Its executor is registered and can take a lease.
    pub ready: bool,
    /// Someone is waiting for it (a bounded wait or `ats wait`).
    pub awaited: bool,
}

pub struct Round<'a> {
    pub capacity: &'a Capacity,
    pub held: &'a [Held],
    pub waiting: &'a [Waiting],
    /// Memory pressure is up: nothing new is admitted (design 10.2).
    pub pressure: bool,
    /// A job admitted moments ago may not show its memory in the
    /// measurements yet; when headroom is tight, admission waits for it
    /// (design 10.1).
    pub settling: bool,
    /// False while executors reconnect after a restart (design 7.1).
    pub admitting: bool,
}

/// Headroom below this fraction of the budget counts as tight: one
/// admission per round, and none while recent admissions settle.
const TIGHT: f64 = 0.2;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    Recovering,
    Pressure,
    Settling,
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
    /// A predecessor failed; `ats unblock` or cancel decides.
    Blocked {
        job: String,
    },
    /// Its predicted peak exceeds the whole budget (design 5.1).
    CapacityInsufficient {
        need: u64,
        budget: u64,
    },
}

/// Waiting jobs in the order they are considered: those someone is
/// waiting for first, then oldest first (design 10.3).
pub fn order(waiting: &[Waiting]) -> Vec<&Waiting> {
    let mut v: Vec<&Waiting> = waiting.iter().collect();
    v.sort_by(|a, b| b.awaited.cmp(&a.awaited).then(b.age.total_cmp(&a.age)));
    v
}

/// H in design 10.1.
pub fn headroom(capacity: &Capacity, held: &[Held]) -> i64 {
    let pending: u64 = held
        .iter()
        .map(|h| h.reserved.saturating_sub(h.resident))
        .sum();
    capacity.memory as i64 - capacity.used as i64 - pending as i64
}

/// When the head of the queue could start: the time until enough held
/// jobs end to free `deficit` bytes, if their end times are known.
fn head_eta(held: &[Held], admitted: &[(f64, u64)], deficit: i64) -> Option<f64> {
    let mut ends: Vec<(f64, u64)> = held
        .iter()
        .map(|h| {
            (
                h.ends_in.unwrap_or(f64::INFINITY),
                h.reserved.max(h.resident),
            )
        })
        .chain(admitted.iter().copied())
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
    let mut locks: Vec<(Lock, String)> = r
        .held
        .iter()
        .flat_map(|h| h.locks.iter().map(|l| (l.clone(), h.id.clone())))
        .collect();
    let mut admitted_ends: Vec<(f64, u64)> = Vec::new();
    let tight = |free: i64| free < (c.memory as f64 * TIGHT) as i64;
    let mut round_closed = false;
    // The first job held back by memory, when it could start, and its
    // locks; later jobs must not delay it.
    let mut head: Option<(String, Option<f64>, Vec<Lock>)> = None;

    let mut out = Vec::new();
    for w in order(r.waiting) {
        let wait = |reason| Verdict::Wait { reason };
        let verdict = (|| {
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Failed) {
                return Verdict::Blocked { job: job.clone() };
            }
            if let Some((job, _)) = w.deps.iter().find(|(_, d)| *d == Dep::Pending) {
                return wait(Reason::Dependency { job: job.clone() });
            }
            if w.reserved > c.memory && !w.now {
                return Verdict::CapacityInsufficient {
                    need: w.reserved,
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
            if r.pressure {
                return wait(Reason::Pressure);
            }
            if (r.settling && tight(free)) || round_closed {
                return wait(Reason::Settling);
            }
            if let Some((head_id, eta, head_locks)) = &head {
                let ends_first = w.seconds.zip(*eta).is_some_and(|(s, e)| s <= e);
                let takes_its_lock = w
                    .locks
                    .iter()
                    .any(|l| head_locks.iter().any(|h| h.conflicts(l)));
                if takes_its_lock || !ends_first {
                    return wait(Reason::Behind {
                        head: head_id.clone(),
                    });
                }
            }
            // With no managed job holding memory, waiting cannot free any
            // that ats controls: the first job always starts (design 10.1).
            let idle = r.held.is_empty() && admitted_ends.is_empty();
            if w.reserved as i64 <= free || idle {
                return Verdict::Admit;
            }
            if head.is_none() {
                let eta = head_eta(r.held, &admitted_ends, w.reserved as i64 - free);
                head = Some((w.id.clone(), eta, w.locks.clone()));
            }
            wait(Reason::Memory {
                need: w.reserved,
                free,
            })
        })();
        if verdict == Verdict::Admit {
            free -= w.reserved as i64;
            locks.extend(w.locks.iter().map(|l| (l.clone(), w.id.clone())));
            admitted_ends.push((w.seconds.unwrap_or(f64::INFINITY), w.reserved));
            if !w.now && tight(free) {
                round_closed = true;
            }
        }
        out.push((w.id.clone(), verdict));
    }
    out
}

#[cfg(test)]
mod tests;
