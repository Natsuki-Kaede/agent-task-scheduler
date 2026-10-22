//! Planning a submission (design 5.2, 8, 10): what the command is, what it
//! is expected to use, which locks it needs, what it waits for, and whether
//! `--now` can run it at once.

use std::path::Path;

use axum::http::StatusCode;

use super::{ApiError, ApiResult, AppState, SubmitRequest, bad_request};
use crate::profile::Profiles;
use crate::recognize::{Analysis, Recognizer};
use crate::sched::locks;
use crate::sched::predict::{Defaults, Lookup, Stats, line_key, parse_hint, predict};
use crate::store::{Entry, Plan, Store};

/// This machine's memory, for default predictions.
fn mem_total(s: &AppState) -> u64 {
    let measured = s
        .measurements
        .lock()
        .expect("measurements lock")
        .system
        .clone();
    measured
        .or_else(crate::sample::system)
        .map_or(8 << 30, |sys| sys.mem_total)
}

/// `command` is the stored (redacted) form, which history is matched on.
pub fn plan(s: &AppState, store: &Store, req: &SubmitRequest, command: &str) -> ApiResult<Plan> {
    let cwd = Path::new(&req.cwd);
    let recognizer = Recognizer::new(&s.profiles, &s.wrappers);
    let analysis = match req.entry {
        Entry::Argv => recognizer.argv(&req.argv, cwd),
        Entry::Sh => recognizer.shell(req.source.as_deref().unwrap_or_default(), cwd),
    };
    let hint = match &req.hint {
        Some(h) => Some(parse_hint(h).map_err(|e| bad_request(format!("--hint: {e}")))?),
        None => None,
    };
    let project = locks::project_root(cwd).to_string_lossy().into_owned();
    let history = |lookup: Lookup| {
        let found = match lookup {
            Lookup::Line => store.history(Some(&project), Some(command), None),
            Lookup::ProjectKey(key) => store.history(Some(&project), None, Some(key)),
            Lookup::GlobalKey(key) => store.history(None, None, Some(key)),
        };
        found.unwrap_or_else(|e| {
            tracing::warn!("reading history: {e:#}");
            Stats::default()
        })
    };
    let prediction = predict(
        &analysis,
        &s.profiles,
        &Defaults::for_machine(mem_total(s)),
        hint,
        &history,
    );
    let locks = locks::for_line(&analysis, cwd, &s.profiles, &req.lock);

    let mut after = Vec::new();
    for id in &req.after {
        match store.resolve_job(id)? {
            Some(job) => after.push(job),
            None => {
                return Err(bad_request(format!(
                    "--after {id}: no such job or submission"
                )));
            }
        }
    }
    let (parallel_env, jobserver) = parallelism(&analysis, &s.profiles);
    let mut profiles: Vec<String> = analysis
        .segments
        .iter()
        .filter_map(|s| s.profile.clone())
        .collect();
    profiles.sort();
    profiles.dedup();
    let plan = Plan {
        profiles,
        prediction,
        locks,
        after,
        run_now: req.now,
        history_key: line_key(&analysis),
        parallel_env,
        jobserver,
    };
    if req.now {
        refuse_now_conflicts(store, &plan)?;
    }
    Ok(plan)
}

/// Whether `args` set a parallelism flag such as `-j8`, `-j 8`, a bare `-j`
/// or `--max-workers=4`.
fn sets_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| {
        a.strip_prefix(flag)
            .is_some_and(|r| r.is_empty() || r.starts_with('=') || r.parse::<f64>().is_ok())
    })
}

/// Parallelism controls for a line (design 10.5): the variables to set to
/// the leased parallelism and whether to offer a jobserver pool. A line
/// that sets its own parallelism gets neither.
fn parallelism(analysis: &Analysis, profiles: &Profiles) -> (Vec<String>, bool) {
    let (mut env, mut jobserver) = (Vec::new(), false);
    for seg in &analysis.segments {
        let Some(p) = profiles
            .find(&seg.argv)
            .filter(|p| Some(&p.name) == seg.profile.as_ref())
        else {
            continue;
        };
        if p.parallelism
            .flag
            .as_deref()
            .is_some_and(|f| sets_flag(&seg.argv[1..], f))
        {
            return (Vec::new(), false);
        }
        env.extend(p.parallelism.env.iter().cloned());
        jobserver |= p.parallelism.jobserver;
    }
    env.sort();
    env.dedup();
    (env, jobserver)
}

/// `--now` runs at once unless something real stands in the way: a
/// predecessor that has not succeeded, or a lock another job holds. The
/// answer names each conflict and what to do instead.
fn refuse_now_conflicts(store: &Store, plan: &Plan) -> ApiResult<()> {
    let mut conflicts = Vec::new();
    for job in &plan.after {
        let row = store.job(job)?.expect("resolved above");
        if !(row.state == "completed" && row.exit_code == Some(0)) {
            conflicts.push(format!("predecessor {job} is {}", row.state));
        }
    }
    for held in store.held_jobs()? {
        for lock in &plan.locks {
            if let Some(h) = held.locks.iter().find(|h| h.conflicts(lock)) {
                conflicts.push(format!("lock {h} is held by {}", held.id));
            }
        }
    }
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(ApiError(
        StatusCode::CONFLICT,
        format!(
            "--now cannot run it: {}. Submit without --now to queue it, or retry later.",
            conflicts.join("; ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallelism_from_profiles() {
        let profiles = Profiles::builtin();
        let dir = tempfile::tempdir().unwrap();
        let r = Recognizer::new(&profiles, &[]);
        let par = |src: &str| parallelism(&r.shell(src, dir.path()), &profiles);
        assert_eq!(
            par("cargo build"),
            (vec!["CARGO_BUILD_JOBS".to_string()], true)
        );
        assert_eq!(
            par("make all && ninja"),
            (vec![], true),
            "make takes its jobs from the pool"
        );
        for explicit in ["ninja -j8", "make -j", "make -j 4", "jest --max-workers=4"] {
            assert_eq!(
                par(explicit),
                (vec![], false),
                "{explicit}: its own parallelism is kept"
            );
        }
        assert_eq!(par("ls"), (vec![], false));
    }
}
