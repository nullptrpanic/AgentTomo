//! One persistent HTTP connection over Docker exec's private stdin/stdout.
//! No published port, writable host mount, per-request process or host cache.
use std::{io, process::Stdio, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::{Query, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hyper::client::conn::http1::SendRequest;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::{io::BufReader, sync::Mutex};

use super::{Access, App, Settings, sandbox};
use crate::execution::protocol;

const MAX_RESPONSE: usize = 32 * 1024 * 1024;

struct Connection {
    sender: Arc<Mutex<SendRequest<Body>>>,
    worker: tokio::task::AbortHandle,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Clone)]
pub struct Client(Arc<Connection>);

impl Client {
    pub async fn connect(container: &str, id: &str, token: &str) -> io::Result<Self> {
        let mut child = tokio::process::Command::new("docker")
            .args([
                "exec",
                "--interactive",
                container,
                "/agenttomo-box",
                "__records",
                "--session",
                id,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        protocol::send(&mut input, &token).await?;
        let io = TokioIo::new(tokio::io::join(child.stdout.take().unwrap(), input));
        let (sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(io::Error::other)?;
        let worker = tokio::spawn(async move {
            let _ = connection.await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        });
        Ok(Self(Arc::new(Connection {
            sender: Arc::new(Mutex::new(sender)),
            worker: worker.abort_handle(),
        })))
    }

    pub async fn request(&self, request: Request) -> io::Result<Response> {
        let mut sender =
            tokio::time::timeout(Duration::from_secs(15), self.0.sender.clone().lock_owned())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "sandbox read channel busy")
                })?;
        let connection = self.clone();
        // Once dispatched, finish draining even if the caller closes its page.
        // The owned lock permits only one such in-flight task per session.
        tokio::spawn(async move {
            let _connection = connection;
            tokio::time::timeout(Duration::from_secs(15), async {
                sender.ready().await.map_err(io::Error::other)?;
                let response = sender
                    .send_request(request)
                    .await
                    .map_err(io::Error::other)?;
                let (parts, body) = response.into_parts();
                let body = to_bytes(Body::new(body), MAX_RESPONSE)
                    .await
                    .map_err(io::Error::other)?;
                Ok(Response::from_parts(parts, Body::from(body)))
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "sandbox read channel timed out")
            })?
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn events(
        &self,
        id: &str,
        token: &str,
        offset: u64,
    ) -> io::Result<serde_json::Value> {
        let response = self
            .request(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{id}/events?token={token}&offset={offset}"
                    ))
                    .header(header::HOST, "sandbox")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await?;
        if response.status() != StatusCode::OK {
            return Err(io::Error::other(format!(
                "sandbox events returned {}",
                response.status()
            )));
        }
        serde_json::from_slice(
            &to_bytes(response.into_body(), MAX_RESPONSE)
                .await
                .map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)
    }
}

pub(super) async fn forward(State(app): State<App>, request: Request, next: Next) -> Response {
    let Some(bridges) = &app.bridges else {
        return next.run(request).await;
    };
    let Ok(Query(access)) = Query::<Access>::try_from_uri(request.uri()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id = request
        .uri()
        .path()
        .split('/')
        .nth(3)
        .unwrap_or("")
        .to_owned();
    if !super::valid_session(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let client = bridges
        .lock()
        .expect("read channel registry")
        .get(&id)
        .cloned();
    let Some(client) = client else {
        return if app
            .running
            .lock()
            .expect("session registry")
            .contains_key(&id)
        {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response();
    };
    let events = request.uri().path().ends_with("/events");
    let request = Request::builder()
        .uri(request.uri().clone())
        .header(header::HOST, "sandbox")
        .body(Body::empty())
        .unwrap();
    let result = async {
        let response = client.request(request).await?;
        if !events || response.status() != StatusCode::OK {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        let mut value: serde_json::Value = serde_json::from_slice(
            &to_bytes(body, MAX_RESPONSE)
                .await
                .map_err(io::Error::other)?,
        )?;
        value["root_pid"] = app
            .running
            .lock()
            .expect("session registry")
            .get(&id)
            .copied()
            .flatten()
            .into();
        let mut response = Response::from_parts(parts, Body::from(serde_json::to_vec(&value)?));
        response.headers_mut().remove(header::CONTENT_LENGTH);
        Ok::<_, io::Error>(response)
    }
    .await;
    match result {
        Ok(response) => response,
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Sandbox recording channel unavailable",
        )
            .into_response(),
    }
}

pub async fn serve(id: &str) -> io::Result<()> {
    if !super::valid_session(id) {
        return Err(io::Error::other("invalid session id"));
    }
    let mut input = BufReader::new(sandbox::stdin()?);
    let token: String = protocol::receive(&mut input).await?;
    let app = App {
        settings: Arc::new(Settings {
            browser: false,
            image: String::new(),
            image_id: String::new(),
            binary: Default::default(),
            output_dir: sandbox::ROOT.into(),
            session_tmpfs_size_mib: 0,
            token,
            file_preview: Default::default(),
            http: Default::default(),
            rootfs: Default::default(),
            codex: None,
            proxy: None,
        }),
        shutdown: Default::default(),
        sessions: tokio_util::task::TaskTracker::new(),
        desktops: Default::default(),
        running: Default::default(),
        proxy_peers: Default::default(),
        bridges: None,
    };
    app.running.lock().unwrap().insert(id.to_owned(), None);
    let io = TokioIo::new(tokio::io::join(input, tokio::io::stdout()));
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            io,
            TowerToHyperService::new(super::records_router().with_state(app)),
        )
        .await
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_one_reader_keeps_other_readers_and_large_payloads_working() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = entered.clone();
        let gate = release.clone();
        let service =
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                let gate = gate.clone();
                async move {
                    if request.uri().path() == "/slow" {
                        seen.notify_one();
                        gate.notified().await;
                    }
                    Ok::<_, std::convert::Infallible>(Response::new(Body::from(vec![
                        b'x';
                        128 * 1024
                    ])))
                }
            });
        let server = tokio::spawn(
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server_io), service),
        );
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client_io))
            .await
            .unwrap();
        let worker = tokio::spawn(connection);
        let client = Client(Arc::new(Connection {
            sender: Arc::new(Mutex::new(sender)),
            worker: worker.abort_handle(),
        }));
        let first = client.clone();
        let pending = tokio::spawn(async move {
            first
                .request(Request::builder().uri("/slow").body(Body::empty()).unwrap())
                .await
        });
        entered.notified().await;
        pending.abort();
        let _ = pending.await;
        release.notify_one();
        let response = client
            .request(Request::builder().uri("/next").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            to_bytes(response.into_body(), MAX_RESPONSE).await.unwrap(),
            vec![b'x'; 128 * 1024]
        );
        drop(client);
        server.abort();
    }
}
