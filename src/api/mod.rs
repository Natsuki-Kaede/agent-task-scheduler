//! The daemon's HTTP/JSON API (api/openapi.yaml). The same router serves the
//! unix socket and the request-file channel.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

mod lifecycle;
mod plan;

use crate::profile::Profiles;
use crate::redact;
use crate::sample::Snapshot;
use crate::sched::Verdict;
use crate::sched::pressure::PressureStatus;
use crate::store::{Conflict, Entry, NewSubmission, NotFound, Store};
use crate::timing::Deadline;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub started_at: String,
    pub instance: String,
    /// Bumped after every change to jobs or submissions, so that waiting
    /// requests re-check their condition.
    pub changes: Arc<tokio::sync::watch::Sender<u64>>,
    /// Executors expected to reconnect after a restart, and until when
    /// leases wait for them (design 7.1).
    reconnect: Arc<Mutex<(HashSet<String>, Deadline)>>,
    stopping: Arc<AtomicBool>,
    /// The latest resource measurements (design 9).
    pub measurements: Arc<Mutex<Snapshot>>,
    pub profiles: Arc<Profiles>,
    /// Wrapper programs from the configuration (design 8.1).
    pub wrappers: Arc<Vec<String>>,
    /// Jobs someone is waiting for, with how many waits (design 10.3).
    awaited: Arc<Mutex<HashMap<String, usize>>>,
    /// The scheduler's latest verdict on each waiting job, for `ats why`.
    pub verdicts: Arc<Mutex<HashMap<String, Verdict>>>,
    /// Memory pressure and what it is doing (design 10.2).
    pub pressure: Arc<Mutex<PressureStatus>>,
}

/// Counts a wait on a job while it lasts.
pub struct Awaiting {
    awaited: Arc<Mutex<HashMap<String, usize>>>,
    job: String,
}

impl Drop for Awaiting {
    fn drop(&mut self) {
        let mut map = self.awaited.lock().expect("awaited lock");
        if let Some(n) = map.get_mut(&self.job) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.job);
            }
        }
    }
}

impl AppState {
    pub fn new(store: Store, started_at: String, instance: String) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            started_at,
            instance,
            changes: Arc::new(tokio::sync::watch::channel(0).0),
            reconnect: Arc::new(Mutex::new((HashSet::new(), Deadline::after(None)))),
            stopping: Arc::new(AtomicBool::new(false)),
            measurements: Arc::new(Mutex::new(Snapshot::default())),
            profiles: Arc::new(Profiles::load(None).expect("built-in profiles parse")),
            wrappers: Arc::new(Vec::new()),
            awaited: Arc::new(Mutex::new(HashMap::new())),
            verdicts: Arc::new(Mutex::new(HashMap::new())),
            pressure: Arc::new(Mutex::new(PressureStatus::default())),
        }
    }

    /// Uses the user's profiles and wrapper list instead of the built-in
    /// profiles alone.
    pub fn with_recognition(mut self, profiles: Profiles, wrappers: Vec<String>) -> Self {
        self.profiles = Arc::new(profiles);
        self.wrappers = Arc::new(wrappers);
        self
    }

    pub fn awaiting(&self, job: &str) -> Awaiting {
        *self
            .awaited
            .lock()
            .expect("awaited lock")
            .entry(job.to_string())
            .or_default() += 1;
        Awaiting {
            awaited: self.awaited.clone(),
            job: job.to_string(),
        }
    }

    pub fn awaited_jobs(&self) -> HashSet<String> {
        self.awaited
            .lock()
            .expect("awaited lock")
            .keys()
            .cloned()
            .collect()
    }

    /// Holds leases until each of these jobs' executors has contacted the
    /// daemon, or until `grace` has passed.
    pub fn expect_reconnects(&self, jobs: HashSet<String>, grace: Duration) {
        *self.reconnect.lock().expect("reconnect lock") = (jobs, Deadline::after(Some(grace)));
    }

    /// Records that a job's executor reached the daemon.
    pub fn contact(&self, job: &str) {
        self.reconnect.lock().expect("reconnect lock").0.remove(job);
    }

    /// False while executors are still reconnecting after a restart.
    pub fn admitting(&self) -> bool {
        let r = self.reconnect.lock().expect("reconnect lock");
        r.0.is_empty() || r.1.expired()
    }

    /// Ends held requests so the daemon can stop.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.notify();
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Wakes requests waiting for a change.
    pub fn notify(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("store lock")
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/jobs", get(list_jobs).post(submit))
        .route("/v1/submissions/{id}", get(submission))
        .route("/v1/measurements", get(measurements))
        .route("/v1/pressure", get(pressure))
        .merge(lifecycle::routes())
        .with_state(state)
}

/// An error answered as JSON with a status code.
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<Conflict>().is_some() {
            return ApiError(StatusCode::CONFLICT, e.to_string());
        }
        if e.downcast_ref::<NotFound>().is_some() {
            return ApiError(StatusCode::NOT_FOUND, e.to_string());
        }
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

async fn health(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "started_at": s.started_at,
        "instance": s.instance,
    }))
}

/// Body of `POST /v1/jobs`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub entry: Entry,
    #[serde(default)]
    pub argv: Vec<String>,
    pub source: Option<String>,
    pub cwd: String,
    pub session: Option<String>,
    /// The submitting CLI, which starts the executor.
    pub launcher_pid: Option<i64>,
    pub launcher_start: Option<String>,
    /// `--after`: job or submission ids that must succeed first.
    #[serde(default)]
    pub after: Vec<String>,
    /// `--now`: run at once unless a dependency or lock conflicts.
    #[serde(default)]
    pub now: bool,
    /// `--lock`: extra paths or named resources.
    #[serde(default)]
    pub lock: Vec<String>,
    /// `--hint mem=6G`.
    pub hint: Option<String>,
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match String::deserialize(d)?.as_str() {
            "argv" => Ok(Entry::Argv),
            "sh" => Ok(Entry::Sh),
            other => Err(serde::de::Error::custom(format!("unknown entry {other:?}"))),
        }
    }
}

async fn submit(State(s): State<AppState>, Json(req): Json<SubmitRequest>) -> ApiResult<Response> {
    let command = match req.entry {
        Entry::Argv if req.argv.is_empty() => return Err(bad_request("argv entry needs argv")),
        Entry::Argv => redact::command_words(&req.argv).join(" "),
        Entry::Sh => redact::shell_source(
            req.source
                .as_deref()
                .ok_or_else(|| bad_request("sh entry needs source"))?,
        ),
    };
    let store = s.store();
    let plan = plan::plan(&s, &store, &req, &command)?;
    let new = NewSubmission {
        entry: req.entry,
        command,
        project_root: crate::sched::locks::project_root(std::path::Path::new(&req.cwd))
            .to_string_lossy()
            .into_owned(),
        cwd: req.cwd,
        session_id: req.session,
        launcher: req.launcher_pid.zip(req.launcher_start),
        plan,
    };
    let mut store = store;
    let created = store.create_submission(&new)?;
    drop(store);
    s.notify();
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<u32>,
}

async fn list_jobs(State(s): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Response> {
    let states: Vec<String> = q
        .state
        .map(|v| {
            v.split(',')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let jobs = s
        .store()
        .list_jobs(&states, q.limit.unwrap_or(100).min(1000))?;
    Ok(Json(jobs).into_response())
}

async fn measurements(State(s): State<AppState>) -> Json<Snapshot> {
    Json(s.measurements.lock().expect("measurements lock").clone())
}

async fn pressure(State(s): State<AppState>) -> Json<PressureStatus> {
    Json(s.pressure.lock().expect("pressure lock").clone())
}

async fn submission(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    match s.store().submission(&id)? {
        Some(row) => Ok(Json(row).into_response()),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no submission {id}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub(super) fn app() -> Router {
        router(AppState::new(
            Store::open_in_memory().unwrap(),
            "t".into(),
            "i".into(),
        ))
    }

    pub(super) async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn submit_list_and_get() {
        let app = app();
        let (st, created) = call(&app, "POST", "/v1/jobs", Some(json!({
            "entry": "sh", "source": "API_TOKEN=x make build", "cwd": "/w", "session": "s1", "now": false
        }))).await;
        assert_eq!(st, StatusCode::CREATED);
        let sid = created["submission"].as_str().unwrap().to_string();

        let (st, jobs) = call(&app, "GET", "/v1/jobs?state=queued", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(jobs[0]["command"], "API_TOKEN=[redacted] make build");

        let (st, sub) = call(&app, "GET", &format!("/v1/submissions/{sid}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(sub["job_state"], "queued");
        assert_eq!(sub["session_id"], "s1");
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let app = app();
        let (st, _) = call(
            &app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "argv", "cwd": "/w"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/jobs",
            Some(json!({"entry": "sh", "source": "make", "cwd": "/w", "urgency": "high"})),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown fields are rejected"
        );
        assert_eq!(
            call(&app, "GET", "/v1/submissions/s_nope", None).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
