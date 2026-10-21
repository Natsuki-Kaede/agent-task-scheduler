//! Request-file channel (design 6.8): for clients that cannot reach the unix
//! socket, such as agents in a sandbox without network access. A client
//! writes `<id>.req`, the daemon answers with `<id>.resp`. Both are written
//! to a temporary name first and renamed, so a reader never sees a partial
//! file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize, Deserialize)]
pub struct FileRequest {
    pub method: String,
    pub path: String,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// Creates the channel directory, readable only by the current user.
pub fn prepare(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Answers requests found in `dir` until the task is dropped.
pub async fn serve(dir: PathBuf, router: Router) -> Result<()> {
    prepare(&dir)?;
    loop {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "req") {
                continue;
            }
            // Claim the request before answering it in its own task, so a
            // long wait does not hold up the requests behind it.
            let raw = match std::fs::read(&path)
                .and_then(|raw| std::fs::remove_file(&path).map(|()| raw))
            {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::warn!("request file {}: {e}", path.display());
                    continue;
                }
            };
            let router = router.clone();
            tokio::spawn(async move {
                if let Err(e) = answer(&path, raw, &router).await {
                    tracing::warn!("request file {}: {e:#}", path.display());
                }
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn answer(path: &Path, raw: Vec<u8>, router: &Router) -> Result<()> {
    let resp = match serde_json::from_slice::<FileRequest>(&raw) {
        Ok(req) => dispatch(router, req).await?,
        Err(e) => FileResponse {
            status: 400,
            body: serde_json::json!({ "error": e.to_string() }),
        },
    };
    write_atomically(&path.with_extension("resp"), &serde_json::to_vec(&resp)?)
}

async fn dispatch(router: &Router, req: FileRequest) -> Result<FileResponse> {
    let body = req
        .body
        .map(|b| Body::from(b.to_string()))
        .unwrap_or_else(Body::empty);
    let http = axum::http::Request::builder()
        .method(req.method.as_str())
        .uri(req.path)
        .header("content-type", "application/json")
        .body(body)?;
    let resp = router.clone().oneshot(http).await?;
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Ok(FileResponse { status, body })
}

/// Sends one request through the channel and waits up to `timeout`.
pub async fn request(dir: &Path, req: &FileRequest, timeout: Duration) -> Result<FileResponse> {
    let id = crate::ids::new("r");
    let req_path = dir.join(format!("{id}.req"));
    let resp_path = dir.join(format!("{id}.resp"));
    write_atomically(&req_path, &serde_json::to_vec(req)?)
        .with_context(|| format!("writing to {}", dir.display()))?;
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(raw) = std::fs::read(&resp_path) {
            std::fs::remove_file(&resp_path)?;
            return Ok(serde_json::from_slice(&raw)?);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Withdraw the request so a daemon that starts later does not act on it.
    let _ = std::fs::remove_file(&req_path);
    bail!("no answer through {} within {timeout:?}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new().route(
            "/v1/ping",
            get(|| async { axum::Json(serde_json::json!({"pong": true})) }),
        );
        let server = tokio::spawn(serve(dir.path().to_path_buf(), router));
        let req = FileRequest {
            method: "GET".into(),
            path: "/v1/ping".into(),
            body: None,
        };
        let resp = request(dir.path(), &req, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["pong"], true);
        server.abort();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "channel files are cleaned up"
        );
    }

    #[tokio::test]
    async fn a_slow_request_does_not_hold_up_others() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    "slow"
                }),
            )
            .route("/fast", get(|| async { "fast" }));
        let server = tokio::spawn(serve(dir.path().to_path_buf(), router));
        let get = |path: &str| FileRequest {
            method: "GET".into(),
            path: path.into(),
            body: None,
        };
        let slow_dir = dir.path().to_path_buf();
        let slow = tokio::spawn(async move {
            request(&slow_dir, &get("/slow"), Duration::from_secs(10)).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let fast = request(dir.path(), &get("/fast"), Duration::from_secs(1)).await;
        assert_eq!(fast.unwrap().status, 200);
        assert_eq!(slow.await.unwrap().unwrap().status, 200);
        server.abort();
    }

    #[tokio::test]
    async fn times_out_and_withdraws_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let req = FileRequest {
            method: "GET".into(),
            path: "/v1/ping".into(),
            body: None,
        };
        assert!(
            request(dir.path(), &req, Duration::from_millis(200))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
