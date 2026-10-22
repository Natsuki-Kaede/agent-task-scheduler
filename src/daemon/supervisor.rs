//! Watches executors and settles jobs whose executor is gone (design 7.3
//! ⑦–⑨). A command runs only after its lease is committed, so a job that
//! never got a lease is known not to have run; after the lease, a job keeps
//! it while any of its processes live, and its exit code is unknown unless
//! the executor reported the main result.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

use crate::api::AppState;
use crate::executor::JOB_ENV;
use crate::procinfo;
use crate::store::{ActiveJob, Conflict};
use crate::timing;

/// Process checks, replaceable in tests.
pub trait Processes {
    fn alive(&self, pid: i64, start: &str) -> bool;
    fn group_alive(&self, pgid: i64) -> bool;
    /// Whether any process carries the job's environment marker.
    fn marked(&self, job: &str) -> bool;
    /// Asks the job's processes to end, or kills them when `force`.
    fn terminate(&self, job: &ActiveJob, force: bool);
}

pub struct System;

impl Processes for System {
    fn alive(&self, pid: i64, start: &str) -> bool {
        procinfo::alive(pid as i32, start)
    }

    fn group_alive(&self, pgid: i64) -> bool {
        procinfo::group_alive(pgid as i32)
    }

    fn marked(&self, job: &str) -> bool {
        !procinfo::with_env(JOB_ENV, job).is_empty()
    }

    fn terminate(&self, job: &ActiveJob, force: bool) {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        let pids = procinfo::with_env(JOB_ENV, &job.id);
        if let Some(g) = job.pgid {
            procinfo::signal_group(g as i32, sig);
            // A frozen process handles SIGTERM only once resumed.
            procinfo::signal_group(g as i32, libc::SIGCONT);
        }
        for pid in pids {
            // SAFETY: signals a process carrying this job's marker.
            unsafe {
                libc::kill(pid, sig);
                libc::kill(pid, libc::SIGCONT);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    AwaitTakeover,
    /// Canceled while its executor is gone: the daemon stops it.
    Terminate,
    Finish(&'static str, Option<i64>),
}

fn alive(p: &Option<(i64, String)>, procs: &impl Processes) -> bool {
    p.as_ref()
        .is_some_and(|(pid, start)| procs.alive(*pid, start))
}

/// Decides what to do with one job that has not ended.
pub fn check(job: &ActiveJob, procs: &impl Processes) -> Option<Action> {
    if alive(&job.executor, procs) {
        return None;
    }
    if matches!(
        job.state.as_str(),
        "queued" | "blocked" | "capacity_insufficient"
    ) {
        // Without a lease the command never ran. Wait for an executor while
        // the process that starts it is alive.
        let starter_gone =
            job.executor.is_some() || (job.launcher.is_some() && !alive(&job.launcher, procs));
        return starter_gone.then_some(Action::Finish("start_failed", None));
    }
    let running = job.pgid.is_some_and(|g| procs.group_alive(g)) || procs.marked(&job.id);
    if running {
        return Some(if job.state != "awaiting_takeover" {
            Action::AwaitTakeover
        } else if job.canceled {
            Action::Terminate
        } else {
            return None;
        });
    }
    Some(match (job.canceled, job.main_exit_code) {
        (true, code) => Action::Finish("canceled", code),
        (false, Some(code)) => Action::Finish("completed", Some(code)),
        (false, None) => Action::Finish("ended_unknown", None),
    })
}

/// Jobs whose executors are alive now; after a restart they are given time
/// to reconnect before new leases are granted.
pub fn live_executors(state: &AppState, procs: &impl Processes) -> Result<Vec<String>> {
    Ok(state
        .store()
        .active_jobs()?
        .into_iter()
        .filter(|j| alive(&j.executor, procs))
        .map(|j| j.id)
        .collect())
}

/// How long processes get to end after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Applies [`check`] to every job that has not ended. `terminating` holds
/// when each canceled job was asked to end.
pub fn sweep(
    state: &AppState,
    procs: &impl Processes,
    terminating: &mut HashMap<String, Duration>,
) -> Result<()> {
    let jobs = state.store().active_jobs()?;
    terminating.retain(|id, _| jobs.iter().any(|j| &j.id == id));
    let mut changed = false;
    for job in jobs {
        let Some(action) = check(&job, procs) else {
            continue;
        };
        let applied = match &action {
            Action::Terminate => {
                let now = timing::elapsed();
                match terminating.get(&job.id) {
                    None => {
                        procs.terminate(&job, false);
                        terminating.insert(job.id.clone(), now);
                    }
                    Some(&asked) if now.saturating_sub(asked) >= TERM_GRACE => {
                        procs.terminate(&job, true)
                    }
                    Some(_) => {}
                }
                continue;
            }
            Action::AwaitTakeover => state.store().await_takeover(&job.id),
            Action::Finish(st, code) => state.store().finish(&job.id, st, *code),
        };
        match applied {
            Ok(()) => {
                tracing::warn!(job = %job.id, ?action, "executor gone");
                changed = true;
            }
            // The executor reported the end between the read and the check.
            Err(e) if e.downcast_ref::<Conflict>().is_some() => {}
            Err(e) => return Err(e),
        }
    }
    if changed {
        state.notify();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[derive(Default)]
    struct Fake {
        alive: HashSet<i64>,
        groups: HashSet<i64>,
        marked: HashSet<String>,
    }

    impl Processes for Fake {
        fn alive(&self, pid: i64, _: &str) -> bool {
            self.alive.contains(&pid)
        }
        fn group_alive(&self, pgid: i64) -> bool {
            self.groups.contains(&pgid)
        }
        fn marked(&self, job: &str) -> bool {
            self.marked.contains(job)
        }
        fn terminate(&self, _: &ActiveJob, _: bool) {}
    }

    fn job(state: &str) -> ActiveJob {
        ActiveJob {
            id: "j".into(),
            state: state.into(),
            executor: Some((10, "s".into())),
            pgid: None,
            launcher: Some((20, "s".into())),
            main_exit_code: None,
            canceled: false,
        }
    }

    fn fake(alive: &[i64], groups: &[i64], marked: bool) -> Fake {
        Fake {
            alive: alive.iter().copied().collect(),
            groups: groups.iter().copied().collect(),
            marked: if marked {
                HashSet::from(["j".to_string()])
            } else {
                HashSet::new()
            },
        }
    }

    #[test]
    fn queued_jobs_never_ran_so_a_lost_starter_means_start_failed() {
        let start_failed = Some(Action::Finish("start_failed", None));
        assert_eq!(check(&job("queued"), &fake(&[10], &[], false)), None);
        assert_eq!(
            check(&job("queued"), &fake(&[20], &[], false)),
            start_failed
        );

        let mut waiting = job("queued");
        waiting.executor = None;
        assert_eq!(
            check(&waiting, &fake(&[20], &[], false)),
            None,
            "launcher still starting the executor"
        );
        assert_eq!(check(&waiting, &fake(&[], &[], false)), start_failed);
        waiting.launcher = None;
        assert_eq!(
            check(&waiting, &fake(&[], &[], false)),
            None,
            "no launcher known"
        );
    }

    #[test]
    fn a_lost_executor_holds_the_job_while_its_processes_run() {
        let mut j = job("running");
        j.pgid = Some(30);
        assert_eq!(check(&j, &fake(&[10], &[30], false)), None);
        assert_eq!(
            check(&j, &fake(&[], &[30], false)),
            Some(Action::AwaitTakeover)
        );
        assert_eq!(
            check(&j, &fake(&[], &[], true)),
            Some(Action::AwaitTakeover),
            "found by marker"
        );
        assert_eq!(
            check(&j, &fake(&[], &[], false)),
            Some(Action::Finish("ended_unknown", None))
        );

        j.state = "awaiting_takeover".into();
        assert_eq!(check(&j, &fake(&[], &[30], false)), None);
        j.main_exit_code = Some(4);
        assert_eq!(
            check(&j, &fake(&[], &[], false)),
            Some(Action::Finish("completed", Some(4)))
        );
    }

    #[test]
    fn a_lost_start_confirmation_is_never_rerun_or_reported_as_not_started() {
        // Leased, but the executor died before reporting the start: the
        // command may have run.
        let j = job("admitted");
        assert_eq!(
            check(&j, &fake(&[], &[], true)),
            Some(Action::AwaitTakeover)
        );
        assert_eq!(
            check(&j, &fake(&[], &[], false)),
            Some(Action::Finish("ended_unknown", None))
        );
    }

    #[test]
    fn a_canceled_takeover_job_is_stopped_then_ends_canceled() {
        let mut j = job("awaiting_takeover");
        j.pgid = Some(30);
        j.canceled = true;
        assert_eq!(check(&j, &fake(&[], &[30], false)), Some(Action::Terminate));
        assert_eq!(
            check(&j, &fake(&[], &[], false)),
            Some(Action::Finish("canceled", None))
        );
        j.state = "running".into();
        assert_eq!(
            check(&j, &fake(&[], &[30], false)),
            Some(Action::AwaitTakeover),
            "first marked as taken over"
        );
    }
}
