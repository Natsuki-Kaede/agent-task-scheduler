//! The daemon: one instance per user, serving the API on a unix socket and
//! through the request-file channel.

mod lock;

use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::api::{self, AppState};
use crate::paths::Paths;
use crate::store::{self, Store};

/// Runs the daemon until `shutdown` completes.
pub async fn run(paths: Paths, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    create_private_dir(&paths.home)?;
    create_private_dir(&paths.logs)?;
    let _lock = lock::acquire(&paths.lock)?;

    let store = Store::open(&paths.db)?;
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        started_at: store::now(),
        instance: crate::ids::new("d"),
    };
    let router = api::router(state);

    // Holding the lock means no other daemon owns the socket file, so a
    // leftover one is stale.
    let _ = std::fs::remove_file(&paths.socket);
    let listener = tokio::net::UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    set_mode(&paths.socket, 0o600)?;

    let channel = tokio::spawn(crate::channel::serve(
        paths.requests.clone(),
        router.clone(),
    ));
    tracing::info!(socket = %paths.socket.display(), requests = %paths.requests.display(), "ats daemon started");

    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await;
    channel.abort();
    let _ = std::fs::remove_file(&paths.socket);
    tracing::info!("ats daemon stopped");
    served.context("serving the API")
}

/// Resolves when the process receives SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    set_mode(dir, 0o700)
}

fn set_mode(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{self, Transport};
    use std::time::Duration;

    fn test_paths(dir: &std::path::Path) -> Paths {
        Paths::with(dir.join("home"), dir.join("requests"))
    }

    async fn wait_for_socket(paths: &Paths) {
        for _ in 0..100 {
            if paths.socket.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon did not start");
    }

    #[tokio::test]
    async fn serves_socket_and_request_files_and_refuses_a_second_instance() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(run(paths.clone(), async {
            let _ = stop_rx.await;
        }));
        wait_for_socket(&paths).await;

        let (health, transport) = client::call(&paths, "GET", "/v1/health", None)
            .await
            .unwrap();
        assert_eq!((health.status, transport), (200, Transport::Socket));

        let body = serde_json::json!({"entry": "argv", "argv": ["cargo", "build"], "cwd": "/w"});
        let (created, _) = client::call(&paths, "POST", "/v1/jobs", Some(body))
            .await
            .unwrap();
        assert_eq!(created.status, 201);

        // A second daemon on the same home must refuse to start.
        let second = run(paths.clone(), async {}).await.unwrap_err();
        assert!(
            second.to_string().contains("another ats daemon is running"),
            "{second:#}"
        );

        // With the socket gone, the client falls back to request files.
        std::fs::remove_file(&paths.socket).unwrap();
        let (jobs, transport) = client::call(&paths, "GET", "/v1/jobs", None).await.unwrap();
        assert_eq!(transport, Transport::RequestFile);
        assert_eq!(jobs.body.as_array().unwrap().len(), 1);

        stop_tx.send(()).unwrap();
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn persisted_jobs_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        for round in 0..2 {
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let daemon = tokio::spawn(run(paths.clone(), async {
                let _ = stop_rx.await;
            }));
            wait_for_socket(&paths).await;
            if round == 0 {
                let body = serde_json::json!({"entry": "sh", "source": "make", "cwd": "/w"});
                client::call(&paths, "POST", "/v1/jobs", Some(body))
                    .await
                    .unwrap();
            } else {
                let (jobs, _) = client::call(&paths, "GET", "/v1/jobs", None).await.unwrap();
                assert_eq!(jobs.body[0]["command"], "make");
            }
            stop_tx.send(()).unwrap();
            daemon.await.unwrap().unwrap();
        }
    }
}
