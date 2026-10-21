//! Client side of the daemon API: the unix socket first, then the
//! request-file channel when the socket cannot be reached (design 6.8).

use std::io::ErrorKind;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;

use crate::channel::{self, FileRequest};
use crate::paths::Paths;

const CHANNEL_TIMEOUT: Duration = Duration::from_secs(5);

/// A daemon answer: HTTP status and JSON body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: serde_json::Value,
}

/// Which transport carried a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Socket,
    RequestFile,
}

/// Sends a request to the daemon.
pub async fn call(
    paths: &Paths,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(Answer, Transport)> {
    match via_socket(paths, method, path, body.as_ref()).await {
        Ok(answer) => Ok((answer, Transport::Socket)),
        Err(e) if socket_unreachable(&e) => {
            let req = FileRequest {
                method: method.into(),
                path: path.into(),
                body,
            };
            let resp = channel::request(&paths.requests, &req, CHANNEL_TIMEOUT)
                .await
                .map_err(|ce| {
                    anyhow!(
                        "daemon unreachable: socket {} ({e}); request-file channel: {ce:#}",
                        paths.socket.display()
                    )
                })?;
            Ok((
                Answer {
                    status: resp.status,
                    body: resp.body,
                },
                Transport::RequestFile,
            ))
        }
        Err(e) => Err(e),
    }
}

fn socket_unreachable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::PermissionDenied
        )
    })
}

async fn via_socket(
    paths: &Paths,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<Answer> {
    let stream = tokio::net::UnixStream::connect(&paths.socket).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("HTTP handshake")?;
    tokio::spawn(conn);
    let payload = body.map(|b| Bytes::from(b.to_string())).unwrap_or_default();
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "ats")
        .header("content-type", "application/json")
        .body(Full::new(payload))?;
    let resp = sender.send_request(req).await.context("sending request")?;
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Ok(Answer { status, body })
}
