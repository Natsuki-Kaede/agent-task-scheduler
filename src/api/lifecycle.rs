//! Job lifecycle endpoints used by executors, and the bounded wait used by
//! the CLI (design 5.4, 5.5, 7.3).

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ApiResult, AppState};
use crate::store::{PRE_LEASE, SubmissionRow, is_terminal};
use crate::timing::Deadline;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/executor", post(executor))
        .route("/v1/jobs/{id}/lease", post(lease))
        .route("/v1/jobs/{id}/started", post(started))
        .route("/v1/jobs/{id}/main-result", post(main_result))
        .route("/v1/jobs/{id}/finished", post(finished))
        .route("/v1/jobs/{id}/control", get(control))
        .route("/v1/submissions/{id}/wait", post(wait))
        .route("/v1/submissions/{id}/cancel", post(cancel))
        .route("/v1/submissions/{id}/detach", post(detach))
        .route("/v1/submissions/{id}/retrieved", post(retrieved))
        .route("/v1/requests/{id}", get(by_request))
        .route("/v1/jobs/{id}/why", get(why))
        .route("/v1/jobs/{id}/unblock", post(unblock))
        .route("/v1/jobs/{id}/pin", post(pin))
}

fn ok() -> ApiResult<Response> {
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn job(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    match s.store().job(&id)? {
        Some(row) => Ok(Json(row).into_response()),
        None => Err(ApiError(StatusCode::NOT_FOUND, format!("no job {id}"))),
    }
}

#[derive(Deserialize)]
struct ExecutorBody {
    pid: i64,
    start: String,
}

async fn executor(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ExecutorBody>,
) -> ApiResult<Response> {
    s.store().set_executor(&id, b.pid, &b.start)?;
    s.notify();
    ok()
}

#[derive(Deserialize)]
struct LeaseBody {
    pid: i64,
}

/// How long a lease request waits for the scheduler before answering
/// `granted: false`; it stays under the request-file channel's timeout.
const LEASE_POLL: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(3)
};

/// Answers whether the scheduler has admitted the job (design 10). The
/// lease is already committed when it is granted; the request waits a
/// little for a decision so commands do not lose a polling interval.
async fn lease(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<LeaseBody>,
) -> ApiResult<Response> {
    s.contact(&id);
    let deadline = Deadline::after(Some(LEASE_POLL));
    let mut changes = s.changes.subscribe();
    loop {
        let row = s
            .store()
            .job(&id)?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no job {id}")))?;
        if row.executor_pid != Some(b.pid) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("process {} is not the executor of job {id}", b.pid),
            ));
        }
        if row.state == "admitted" {
            return Ok(Json(json!({ "granted": true })).into_response());
        }
        if !PRE_LEASE.contains(&row.state.as_str()) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("job {id} is {}", row.state),
            ));
        }
        if deadline.expired() || s.stopping() {
            return Ok(Json(json!({ "granted": false })).into_response());
        }
        let pause = deadline
            .remaining()
            .unwrap_or_default()
            .min(Duration::from_secs(1));
        let _ = tokio::time::timeout(pause, changes.changed()).await;
    }
}

#[derive(Deserialize)]
struct StartedBody {
    log_path: String,
    pgid: Option<i64>,
}

async fn started(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<StartedBody>,
) -> ApiResult<Response> {
    s.store().set_started(&id, &b.log_path, b.pgid)?;
    s.notify();
    ok()
}

#[derive(Deserialize)]
struct ExitBody {
    exit_code: i64,
}

async fn main_result(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ExitBody>,
) -> ApiResult<Response> {
    s.store().set_main_result(&id, b.exit_code)?;
    s.notify();
    ok()
}

#[derive(Deserialize)]
struct FinishedBody {
    state: String,
    exit_code: Option<i64>,
    /// CPU time of every process the command waited for.
    cpu_seconds: Option<f64>,
    /// The largest resident size of any one of them.
    max_rss: Option<i64>,
}

async fn finished(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<FinishedBody>,
) -> ApiResult<Response> {
    if !is_terminal(&b.state) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{} is not a terminal state", b.state),
        ));
    }
    if let Some(cpu) = b.cpu_seconds {
        s.store().record_exit_usage(&id, cpu, b.max_rss)?;
    }
    s.store().finish(&id, &b.state, b.exit_code)?;
    s.notify();
    ok()
}

/// Polled by executors; also tells the daemon that the executor is
/// connected.
async fn control(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.contact(&id);
    let cancel = s.store().cancel_requested(&id)?;
    Ok(Json(json!({ "cancel": cancel })).into_response())
}

#[derive(Deserialize)]
struct WaitBody {
    /// Seconds to wait; absent waits without limit.
    wait_seconds: Option<f64>,
    /// "main": until the main execution process has a result or the job
    /// ends. "terminal": until the job ends.
    until: String,
}

/// A blocked job waits for `ats unblock` or a cancel, and one too big for
/// the machine for a decision; both are reported instead of waited out.
fn reached(row: &SubmissionRow, until: &str) -> bool {
    is_terminal(&row.job_state)
        || row.job_state == "blocked"
        || (until == "main"
            && (row.main_exit_code.is_some() || row.job_state == "capacity_insufficient"))
}

/// Waits for a submission's job to reach a condition, up to a time limit,
/// and returns its state either way with `reached` set accordingly.
async fn wait(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<WaitBody>,
) -> ApiResult<Response> {
    if b.until != "main" && b.until != "terminal" {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "until must be main or terminal".into(),
        ));
    }
    let deadline = Deadline::after(b.wait_seconds.map(Duration::from_secs_f64));
    let mut changes = s.changes.subscribe();
    let job = s
        .store()
        .submission(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no submission {id}")))?
        .job;
    let _awaiting = s.awaiting(&job);
    loop {
        let row = s
            .store()
            .submission(&id)?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no submission {id}")))?;
        let done = reached(&row, &b.until);
        if done || deadline.expired() || s.stopping() {
            return Ok(Json(json!({ "reached": done, "submission": row })).into_response());
        }
        // Re-check on every change, and at least once a second: tokio's
        // timers do not count system sleep, while the deadline does.
        let pause = deadline
            .remaining()
            .unwrap_or(Duration::MAX)
            .min(Duration::from_secs(1));
        let _ = tokio::time::timeout(pause, changes.changed()).await;
    }
}

async fn cancel(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store().cancel_submission(&id)?;
    s.notify();
    ok()
}

async fn detach(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store().detach_submission(&id)?;
    s.notify();
    ok()
}

async fn retrieved(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store().mark_retrieved(&id)?;
    ok()
}

async fn by_request(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    match s.store().submission_by_request(&id)? {
        Some(row) => Ok(Json(row).into_response()),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no submission with request id {id}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{app, call};
    use axum::http::StatusCode;
    use serde_json::json;

    async fn submit(app: &axum::Router) -> (String, String) {
        let (st, created) = call(
            app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "argv", "argv": ["make"], "cwd": "/w"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        (
            created["submission"].as_str().unwrap().into(),
            created["job"].as_str().unwrap().into(),
        )
    }

    #[tokio::test]
    async fn job_runs_to_terminal_and_result_is_retrieved() {
        let state = crate::api::AppState::new(
            crate::store::Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        );
        let app = crate::api::router(state.clone());
        let (sub, job) = submit(&app).await;
        let post = |path: String, body| {
            let app = app.clone();
            async move { call(&app, "POST", &path, Some(body)).await.0 }
        };
        let wait = |until: &'static str| {
            let (app, sub) = (app.clone(), sub.clone());
            async move {
                call(
                    &app,
                    "POST",
                    &format!("/v1/submissions/{sub}/wait"),
                    Some(json!({"wait_seconds": 0, "until": until})),
                )
                .await
                .1
            }
        };

        // The CLI and the executor both register the same process; another
        // process is refused.
        for _ in 0..2 {
            let executor = json!({"pid": 1, "start": "t"});
            assert_eq!(
                post(format!("/v1/jobs/{job}/executor"), executor).await,
                StatusCode::NO_CONTENT
            );
        }
        let other = json!({"pid": 2, "start": "t"});
        assert_eq!(
            post(format!("/v1/jobs/{job}/executor"), other).await,
            StatusCode::CONFLICT
        );
        // The lease comes from the scheduler; until it decides, the answer
        // is "not yet".
        let lease_path = format!("/v1/jobs/{job}/lease");
        let lease = |pid: i64| call(&app, "POST", &lease_path, Some(json!({"pid": pid})));
        assert_eq!(lease(1).await.1["granted"], false);
        state.store().admit(&job, 1 << 30, 0).unwrap();
        // Asking again after a lost answer gets the same lease; only the
        // executor gets it.
        for _ in 0..2 {
            assert_eq!(lease(1).await.1["granted"], true);
        }
        assert_eq!(
            post(format!("/v1/jobs/{job}/lease"), json!({"pid": 2})).await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            post(format!("/v1/jobs/{job}/started"), json!({"log_path": "/l"})).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(wait("main").await["reached"], false);
        // The retrieved marker needs a terminal state.
        assert_eq!(
            post(format!("/v1/submissions/{sub}/retrieved"), json!({})).await,
            StatusCode::CONFLICT
        );

        post(
            format!("/v1/jobs/{job}/main-result"),
            json!({"exit_code": 2}),
        )
        .await;
        let w = wait("main").await;
        assert_eq!(w["reached"], true);
        assert_eq!(w["submission"]["main_exit_code"], 2);
        assert_eq!(w["submission"]["job_state"], "running");
        assert_eq!(wait("terminal").await["reached"], false);

        assert_eq!(
            post(
                format!("/v1/jobs/{job}/finished"),
                json!({"state": "running"})
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        post(
            format!("/v1/jobs/{job}/finished"),
            json!({"state": "completed", "exit_code": 2}),
        )
        .await;
        // A repeated report is accepted; a different end is refused.
        let again = json!({"state": "completed", "exit_code": 2});
        assert_eq!(
            post(format!("/v1/jobs/{job}/finished"), again).await,
            StatusCode::NO_CONTENT
        );
        let other = json!({"state": "canceled"});
        assert_eq!(
            post(format!("/v1/jobs/{job}/finished"), other).await,
            StatusCode::CONFLICT
        );
        let w = wait("terminal").await;
        assert_eq!(w["reached"], true);
        assert_eq!(w["submission"]["exit_code"], 2);
        assert_eq!(w["submission"]["final_retrieved"], false);
        assert_eq!(
            post(format!("/v1/submissions/{sub}/retrieved"), json!({})).await,
            StatusCode::NO_CONTENT
        );
        let (_, row) = call(&app, "GET", &format!("/v1/submissions/{sub}"), None).await;
        assert_eq!(row["final_retrieved"], true);
    }

    #[tokio::test]
    async fn canceling_a_queued_job_ends_it_and_refuses_the_lease() {
        let app = app();
        let (sub, job) = submit(&app).await;
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/submissions/{sub}/cancel"),
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, row) = call(&app, "GET", &format!("/v1/jobs/{job}"), None).await;
        assert_eq!(row["state"], "canceled");
        let (_, control) = call(&app, "GET", &format!("/v1/jobs/{job}/control"), None).await;
        assert_eq!(control["cancel"], true);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/jobs/{job}/lease"),
            Some(json!({"pid": 1})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn finds_submissions_by_request_id() {
        let app = app();
        let body = json!({"entry": "argv", "argv": ["make"], "cwd": "/w", "request_id": "r1"});
        let (_, created) = call(&app, "POST", "/v1/jobs", Some(body)).await;
        let (st, row) = call(&app, "GET", "/v1/requests/r1", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(row["id"], created["submission"]);
        assert_eq!(
            call(&app, "GET", "/v1/requests/r2", None).await.0,
            StatusCode::NOT_FOUND
        );
    }
}

/// Why a job is where it is: its state, and for a waiting job the
/// scheduler's latest verdict (design 5.4).
async fn why(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let row = s
        .store()
        .job(&id)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no job {id}")))?;
    let verdict = s.verdicts.lock().expect("verdicts lock").get(&id).cloned();
    Ok(Json(json!({ "job": id, "state": row.state, "verdict": verdict })).into_response())
}

async fn unblock(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store().unblock(&id)?;
    s.notify();
    ok()
}

#[derive(Deserialize)]
struct PinBody {
    /// Queue position; lower runs first. `null` unpins.
    rank: Option<i64>,
}

async fn pin(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PinBody>,
) -> ApiResult<Response> {
    s.store().pin(&id, b.rank)?;
    s.notify();
    ok()
}
