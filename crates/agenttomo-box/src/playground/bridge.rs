//! Persistent read channels over Docker exec's private stdin/stdout.
//! No published port, writable host mount, per-request process or host cache.
use std::{error::Error as _, io, process::Stdio, sync::Arc, time::Duration};

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
    sender: SendRequest<Body>,
    worker: tokio::task::AbortHandle,
}

fn disconnected(error: hyper::Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error)
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Clone)]
pub struct Client(Arc<Endpoint>);

struct Endpoint {
    container: String,
    id: String,
    token: String,
    viewer: String,
    connection: Arc<Mutex<Option<Connection>>>,
}

impl Client {
    pub(super) fn new(container: &str, id: &str) -> Self {
        Self(Arc::new(Endpoint {
            container: container.into(),
            id: id.into(),
            token: uuid::Uuid::new_v4().simple().to_string(),
            viewer: uuid::Uuid::new_v4().simple().to_string(),
            connection: Arc::new(Mutex::new(None)),
        }))
    }

    pub async fn connect(container: &str, id: &str) -> io::Result<Self> {
        let client = Self::new(container, id);
        let connection = Self::open(container, id, &client.0.token).await?;
        *client.0.connection.lock().await = Some(connection);
        Ok(client)
    }

    fn target(&self, uri: &axum::http::Uri) -> String {
        let mut target = format!("{}?token={}", uri.path(), self.0.token);
        for param in uri.query().unwrap_or("").split('&') {
            if param.split_once('=').is_some_and(|(key, _)| {
                matches!(
                    key,
                    "offset" | "length" | "decode" | "request_id" | "message_id"
                )
            }) {
                target.push('&');
                target.push_str(param);
            }
        }
        target
    }

    pub(super) fn viewer(&self) -> &str {
        &self.0.viewer
    }

    async fn open(container: &str, id: &str, token: &str) -> io::Result<Connection> {
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
            .map_err(disconnected)?;
        let worker = tokio::spawn(async move {
            let _ = connection.await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        });
        Ok(Connection {
            sender,
            worker: worker.abort_handle(),
        })
    }

    pub async fn request(&self, request: Request) -> io::Result<Response> {
        let client = self.clone();
        let uri = request.uri().clone();
        // Keep draining a dispatched GET if its browser disconnects. The permit
        // covers both the HTTP response body and any replacement connection.
        let mut slot = tokio::time::timeout(
            Duration::from_secs(15),
            client.0.connection.clone().lock_owned(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sandbox read channel busy"))?;
        tokio::spawn(async move {
            for attempt in 0..2 {
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    if slot.is_none() {
                        *slot = Some(
                            Self::open(&client.0.container, &client.0.id, &client.0.token).await?,
                        );
                    }
                    let sender = &mut slot.as_mut().unwrap().sender;
                    sender.ready().await.map_err(disconnected)?;
                    let request = Request::builder()
                        .uri(&uri)
                        .header(header::HOST, "sandbox")
                        .body(Body::empty())
                        .map_err(io::Error::other)?;
                    let response = sender.send_request(request).await.map_err(disconnected)?;
                    let (parts, body) = response.into_parts();
                    let body = to_bytes(Body::new(body), MAX_RESPONSE)
                        .await
                        .map_err(|error| {
                            let kind = if error
                                .source()
                                .is_some_and(|cause| cause.is::<http_body_util::LengthLimitError>())
                            {
                                io::ErrorKind::InvalidData
                            } else {
                                io::ErrorKind::ConnectionAborted
                            };
                            io::Error::new(kind, error)
                        })?;
                    Ok::<_, io::Error>(Response::from_parts(parts, Body::from(body)))
                })
                .await
                .unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "sandbox read channel timed out",
                    ))
                });
                if let Err(error) = &result {
                    *slot = None;
                    if attempt == 0 && error.kind() != io::ErrorKind::InvalidData {
                        continue;
                    }
                }
                return result;
            }
            unreachable!()
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn events(&self, id: &str, offset: u64) -> io::Result<serde_json::Value> {
        let response = self
            .request(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{id}/events?token={}&offset={offset}",
                        self.0.token
                    ))
                    .header(header::HOST, "sandbox")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await?;
        if response.status() != StatusCode::OK {
            let status = response.status();
            let kind = match status {
                StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::BAD_GATEWAY
                | StatusCode::GATEWAY_TIMEOUT
                | StatusCode::TOO_MANY_REQUESTS
                | StatusCode::REQUEST_TIMEOUT => io::ErrorKind::WouldBlock,
                _ => io::ErrorKind::InvalidData,
            };
            return Err(io::Error::new(
                kind,
                format!("sandbox events returned {status}"),
            ));
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
    let id = request
        .uri()
        .path()
        .split('/')
        .nth(3)
        .unwrap_or("")
        .to_owned();
    if !access.authorized(&app) && !access.can_monitor(&app, &id) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let head = request.method() == axum::http::Method::HEAD;
    if !head && request.method() != axum::http::Method::GET {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }
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
        .uri(client.target(request.uri()))
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
        value["monitor_token"] = client.viewer().into();
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
        Ok(mut response) => {
            if head {
                *response.body_mut() = Body::empty();
            }
            response
        }
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
            agents: Vec::new(),
            proxy: None,
            credential_gateway: None,
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
    async fn event_cursor_survives_temporary_read_service_failures() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let service =
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                assert!(request.uri().query().unwrap().ends_with("offset=0"));
                let attempt = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move {
                    let response = if attempt < 2 {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    } else {
                        axum::Json(serde_json::json!({
                            "events":[], "next_offset":0, "active":false, "has_more":false,
                            "result":{"status":{"state":"complete"}}
                        }))
                        .into_response()
                    };
                    Ok::<_, std::convert::Infallible>(response)
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
        let client = Client::new("test", "test");
        *client.0.connection.lock().await = Some(Connection {
            sender,
            worker: worker.abort_handle(),
        });
        let (frames, mut received) = tokio::sync::mpsc::channel(2);
        tokio::time::timeout(
            Duration::from_secs(4),
            super::super::trace_feed::follow(client, "test", frames),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(super::super::session::Frame::Complete(_))
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 3);
        server.abort();
    }

    #[test]
    fn forwarding_uses_only_the_private_channel_token_and_reader_parameters() {
        let client = Client::new("sandbox", "session");
        let uri = "/api/sessions/session/events?offset=51&token=host-secret&access=another-secret"
            .parse()
            .unwrap();
        assert_eq!(
            client.target(&uri),
            format!(
                "/api/sessions/session/events?token={}&offset=51",
                client.0.token
            )
        );
        assert_ne!(client.0.token, client.viewer());
        assert_ne!(client.viewer(), Client::new("other", "other").viewer());
    }

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
        let client = Client(Arc::new(Endpoint {
            container: String::new(),
            id: String::new(),
            token: String::new(),
            viewer: String::new(),
            connection: Arc::new(Mutex::new(Some(Connection {
                sender,
                worker: worker.abort_handle(),
            }))),
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
