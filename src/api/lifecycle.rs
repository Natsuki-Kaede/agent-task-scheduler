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
use crate::store::{SubmissionRow, is_terminal};
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
}

fn ok() -> ApiResult<Response> {
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn not_found(e: anyhow::Error) -> ApiError {
    if e.to_string().starts_with("no ") {
        ApiError(StatusCode::NOT_FOUND, e.to_string())
    } else {
        e.into()
    }
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
    s.store()
        .set_executor(&id, b.pid, &b.start)
        .map_err(not_found)?;
    s.notify();
    ok()
}

/// Grants the lease. Admission control (design 10) decides when; until it
/// lands, every job is admitted at once.
async fn lease(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store()
        .grant_lease(&id)
        .map_err(|e| match e.to_string() {
            m if m.starts_with("no ") => ApiError(StatusCode::NOT_FOUND, m),
            m => ApiError(StatusCode::CONFLICT, m),
        })?;
    s.notify();
    Ok(Json(json!({ "granted": true })).into_response())
}

#[derive(Deserialize)]
struct StartedBody {
    log_path: String,
}

async fn started(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<StartedBody>,
) -> ApiResult<Response> {
    s.store().set_started(&id, &b.log_path).map_err(not_found)?;
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
    s.store()
        .set_main_result(&id, b.exit_code)
        .map_err(not_found)?;
    s.notify();
    ok()
}

#[derive(Deserialize)]
struct FinishedBody {
    state: String,
    exit_code: Option<i64>,
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
    s.store()
        .finish(&id, &b.state, b.exit_code)
        .map_err(not_found)?;
    s.notify();
    ok()
}

async fn control(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
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

fn reached(row: &SubmissionRow, until: &str) -> bool {
    is_terminal(&row.job_state) || (until == "main" && row.main_exit_code.is_some())
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
    loop {
        let row = s
            .store()
            .submission(&id)?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no submission {id}")))?;
        let done = reached(&row, &b.until);
        if done || deadline.expired() {
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
    s.store().cancel_submission(&id).map_err(not_found)?;
    s.notify();
    ok()
}

async fn detach(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store().detach_submission(&id).map_err(not_found)?;
    s.notify();
    ok()
}

async fn retrieved(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    s.store()
        .mark_retrieved(&id)
        .map_err(|e| match e.to_string() {
            m if m.starts_with("no ") => ApiError(StatusCode::NOT_FOUND, m),
            m => ApiError(StatusCode::CONFLICT, m),
        })?;
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
        let app = app();
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

        let executor = json!({"pid": 1, "start": "t"});
        assert_eq!(
            post(format!("/v1/jobs/{job}/executor"), executor).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            post(format!("/v1/jobs/{job}/lease"), json!({})).await,
            StatusCode::OK
        );
        // A lease is granted once.
        assert_eq!(
            post(format!("/v1/jobs/{job}/lease"), json!({})).await,
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
            Some(json!({})),
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
