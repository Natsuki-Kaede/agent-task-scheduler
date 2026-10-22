//! Planning a submission (design 5.3, 8, 10): what the command is, what it
//! is expected to use, which locks it needs, what it waits for, and whether
//! `--now` can run it at once.

use std::path::Path;

use axum::http::StatusCode;

use super::{ApiError, ApiResult, AppState, SubmitRequest, bad_request};
use crate::profile::Profiles;
use crate::recognize::{Analysis, Recognizer};
use crate::sched::locks;
use crate::sched::predict::{Defaults, Hint, Lookup, Stats, line_key, predict};
use crate::store::{Entry, Plan, Store};

/// Memory and cores of this machine, for default predictions.
fn machine(s: &AppState) -> (u64, f64) {
    let measured = s
        .measurements
        .lock()
        .expect("measurements lock")
        .system
        .clone();
    match measured.or_else(crate::sample::system) {
        Some(sys) => (
            sys.mem_total,
            f64::from(sys.perf_cores + sys.efficiency_cores).max(1.0),
        ),
        None => (
            8 << 30,
            std::thread::available_parallelism().map_or(4.0, |n| n.get() as f64),
        ),
    }
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
        Some(h) => Hint::parse(h).map_err(|e| bad_request(format!("--hint: {e}")))?,
        None => Hint::default(),
    };
    let (mem, cores) = machine(s);
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
        &Defaults::for_machine(mem, cores),
        &hint,
        &history,
    );
    let locks = locks::for_line(&analysis, cwd, &s.profiles, &req.lock);

    let mut after = Vec::new();
    for id in &req.after {
        match store.resolve_job(id)? {
            None => {
                return Err(bad_request(format!(
                    "--after {id}: no such job or submission"
                )));
            }
            Some((_, true)) => {
                return Err(bad_request(format!(
                    "--after {id}: it is a service, which never succeeds; submit a readiness probe such as curl -sf and depend on that"
                )));
            }
            Some((job, false)) => after.push(job),
        }
    }
    let urgency = match req.urgency.as_deref() {
        None | Some("normal") => 0,
        Some("high") => 1,
        Some("low") => -1,
        Some(other) => {
            return Err(bad_request(format!(
                "--urgency {other}: use low, normal or high"
            )));
        }
    };
    let (parallel_env, jobserver, explicit) = parallelism(&analysis, &s.profiles);
    let mut prediction = prediction;
    if let Some(n) = explicit {
        prediction.cores = prediction.cores.max(n.min(cores));
    }
    // Some jobs must never be stopped (design 11).
    prediction.freezable &= !req.no_freeze && !req.now;
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
        urgency,
        history_key: line_key(&analysis),
        parallel_env,
        jobserver,
    };
    if req.now {
        refuse_now_conflicts(store, &plan)?;
    }
    Ok(plan)
}

/// The value given to a parallelism flag such as `-j8`, `-j 8` or
/// `--max-workers=4`; a bare `-j` means no limit.
fn explicit_value(args: &[String], flag: &str) -> Option<f64> {
    for (i, a) in args.iter().enumerate() {
        let rest = if a == flag {
            match args.get(i + 1).and_then(|n| n.parse::<f64>().ok()) {
                Some(n) => return Some(n),
                None => return Some(f64::INFINITY),
            }
        } else if let Some(r) = a.strip_prefix(flag) {
            r.strip_prefix('=').unwrap_or(r)
        } else {
            continue;
        };
        if let Ok(n) = rest.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

/// Parallelism controls for a line (design 10.5): the variables to set to
/// the leased parallelism and whether to offer a jobserver pool. A command
/// that sets its own parallelism keeps it, and that becomes its CPU request.
fn parallelism(analysis: &Analysis, profiles: &Profiles) -> (Vec<String>, bool, Option<f64>) {
    let (mut env, mut jobserver, mut explicit) = (Vec::new(), false, None::<f64>);
    for seg in &analysis.segments {
        let Some(p) = profiles
            .find(&seg.argv)
            .filter(|p| Some(&p.name) == seg.profile.as_ref())
        else {
            continue;
        };
        if let Some(n) = p
            .parallelism
            .flag
            .as_deref()
            .and_then(|f| explicit_value(&seg.argv[1..], f))
        {
            explicit = Some(explicit.map_or(n, |e| e.max(n)));
        }
        env.extend(p.parallelism.env.iter().cloned());
        jobserver |= p.parallelism.jobserver;
    }
    if explicit.is_some() {
        return (Vec::new(), false, explicit);
    }
    env.sort();
    env.dedup();
    (env, jobserver, None)
}

/// `--now` runs at once unless something real stands in the way: a
/// predecessor that has not succeeded, or a lock another job holds (user
/// decision 3). The answer names each conflict and what to do instead.
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
            (vec!["CARGO_BUILD_JOBS".to_string()], true, None)
        );
        assert_eq!(
            par("make all && ninja"),
            (vec![], true, None),
            "make takes its jobs from the pool"
        );
        assert_eq!(
            par("ninja -j8"),
            (vec![], false, Some(8.0)),
            "explicit parallelism is kept"
        );
        assert_eq!(par("make -j"), (vec![], false, Some(f64::INFINITY)));
        assert_eq!(par("jest --max-workers=4"), (vec![], false, Some(4.0)));
        assert_eq!(par("ls"), (vec![], false, None));
    }
}
