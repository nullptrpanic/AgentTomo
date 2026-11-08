//! HTTP/1.1 and task-CA TLS inspection, independent of ptrace and the UI.
mod body;
mod io;
mod streams;

use crate::{
    proxy::{
        tcp::{Context, transfer},
        tls::Authority,
    },
    recording::{
        Observation,
        http::{Limits, Store},
    },
};
use axum::{
    body::Bytes,
    http::{HeaderMap, Method, Request, Response, StatusCode},
};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use io::{BoxIo, Counters, Metered};
use serde_json::{Value, json};
use std::{
    io as stdio,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, mpsc},
    time::{timeout, timeout_at},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub enabled: bool,
    pub limits: Limits,
    pub streams: crate::recording::streams::Limits,
}
impl Options {
    pub fn arguments(self) -> Vec<String> {
        let mut args = vec![
            "--http-body-max-bytes".into(),
            self.limits.max_bytes.to_string(),
            "--http-body-total-bytes".into(),
            self.limits.total_bytes.to_string(),
            "--stream-message-max-bytes".into(),
            self.streams.message_bytes.to_string(),
            "--stream-window-bytes".into(),
            self.streams.window_bytes.to_string(),
            "--stream-window-messages".into(),
            self.streams.window_messages.to_string(),
        ];
        if self.enabled {
            args.push("--capture-http".into());
        }
        args
    }
}

pub struct Inspection {
    pub authority: Authority,
    store: Arc<Store>,
    streams: Arc<crate::recording::streams::Store>,
    ids: AtomicU64,
}
impl Inspection {
    pub async fn create(directory: &Path, options: Options) -> stdio::Result<Option<Arc<Self>>> {
        if !options.enabled {
            return Ok(None);
        }
        let authority = Authority::new()?;
        Ok(Some(Arc::new(Self {
            authority,
            store: Store::create(directory, options.limits).await?,
            streams: crate::recording::streams::Store::create(directory, options.streams).await?,
            ids: AtomicU64::new(1),
        })))
    }
}

impl Inspection {
    pub async fn flush(&self) -> stdio::Result<()> {
        self.streams.flush().await
    }
}

fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
fn headers(headers: &HeaderMap) -> Value {
    // Header values are bytes. Preserve every byte with an explicit Latin-1
    // representation, duplicate fields included; never combine Set-Cookie.
    json!(
        headers
            .iter()
            .map(|(name, value)| (
                name.as_str(),
                value
                    .as_bytes()
                    .iter()
                    .map(|b| char::from(*b))
                    .collect::<String>()
            ))
            .collect::<Vec<_>>()
    )
}

fn header_token(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(expected))
}

#[derive(Clone)]
struct Session {
    inspection: Arc<Inspection>,
    context: Context,
    events: mpsc::Sender<Observation>,
    writes: TaskTracker,
    finished: CancellationToken,
    scheme: &'static str,
}
#[derive(Clone)]
struct MessageContext {
    session: Session,
    request_id: u64,
    start: Instant,
    unix_ns: u128,
    response: bool,
}
impl MessageContext {
    async fn emit(&self, kind: &'static str, mut data: Value) -> stdio::Result<()> {
        data["request_id"] = self.request_id.into();
        data["started_unix_ns"] = json!(self.unix_ns);
        data["start_source"] = "proxy_request_headers".into();
        data["scheme"] = self.session.scheme.into();
        self.session
            .events
            .send(self.session.context.event(kind, data))
            .await
            .map_err(stdio::Error::other)
    }
    async fn emit_body(
        self,
        body: Value,
        trailers: Value,
        completed: u128,
        duration: u128,
    ) -> stdio::Result<()> {
        self.emit(if self.response { "network.http.response.body" } else { "network.http.request.body" }, json!({"body":body,"trailers":trailers,"completed_unix_ns":completed,"duration_ns":duration})).await
    }
}

pub async fn forward(
    client: TcpStream,
    upstream: TcpStream,
    inspection: Arc<Inspection>,
    context: &Context,
    events: &mpsc::Sender<Observation>,
    finished: CancellationToken,
) -> stdio::Result<transfer::Outcome> {
    let mut first = [0];
    let mut server_first = [0];
    let tls = tokio::select! {
        biased;
        count = client.peek(&mut first) => count? > 0 && first[0] == 0x16,
        count = upstream.peek(&mut server_first) => { count?; false },
        _ = finished.cancelled() => false,
    };
    let counts = Arc::new(Counters::default());
    let client: BoxIo = Box::new(Metered {
        io: client,
        counts: counts.clone(),
    });
    let (client, upstream, scheme): (BoxIo, BoxIo, _) = if tls {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let handshake = async {
            let start =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), client)
                    .await?;
            let (name, name_source) = start
                .client_hello()
                .server_name()
                .map(|name| (name.to_owned(), "tls_sni"))
                .unwrap_or_else(|| (context.key.destination.ip().to_string(), "destination_ip"));
            let server_name = rustls::pki_types::ServerName::try_from(name.clone())
                .map_err(stdio::Error::other)?;
            let remote = tokio_rustls::TlsConnector::from(inspection.authority.client.clone())
                .connect(server_name, upstream)
                .await?;
            let local = start
                .into_stream(inspection.authority.server(&name)?)
                .await?;
            let alpn = local
                .get_ref()
                .1
                .alpn_protocol()
                .map(String::from_utf8_lossy);
            let upstream_alpn = remote
                .get_ref()
                .1
                .alpn_protocol()
                .map(String::from_utf8_lossy);
            events.send(context.event("network.protocol", json!({"protocol":"tls","domain":name,"domain_source":name_source,"scope":"connection_preface","tls_intercepted":true,"alpn":alpn,"upstream_alpn":upstream_alpn}))).await.map_err(stdio::Error::other)?;
            Ok::<_, stdio::Error>((Box::new(local) as BoxIo, Box::new(remote) as BoxIo))
        };
        let result = tokio::select! {
            result = timeout_at(deadline, handshake) => result.map_err(stdio::Error::other).and_then(|result| result),
            _ = finished.cancelled() => Err(stdio::Error::other("task ended during TLS handshake")),
        };
        match result {
            Ok((client, upstream)) => (client, upstream, "https"),
            Err(error) => {
                events.send(context.event("network.tls.error", json!({"error":error.to_string(),"stage":"handshake","tls_intercepted":false}))).await.map_err(stdio::Error::other)?;
                return Err(error);
            }
        }
    } else {
        (client, Box::new(upstream), "http")
    };
    let (is_http, client, upstream) = io::detect(client, upstream, &finished).await?;
    if is_http {
        let session = Session {
            inspection,
            context: context.clone(),
            events: events.clone(),
            writes: TaskTracker::new(),
            finished: finished.clone(),
            scheme,
        };
        bridge(client, upstream, session).await?;
    } else {
        // No HTTP parser sees opaque protocols. Existing passive SNI/Host
        // observation and half-close behavior stay available in this branch.
        transfer::copy(client, upstream, finished, context, events).await?;
    }
    Ok(transfer::Outcome {
        up: counts.up.load(Ordering::Relaxed),
        down: Some(counts.down.load(Ordering::Relaxed)),
        reason: "closed",
    })
}

type HttpBody = UnsyncBoxBody<Bytes, hyper::Error>;
async fn bridge(client: BoxIo, upstream: BoxIo, session: Session) -> stdio::Result<()> {
    let (sender, connection) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .handshake::<_, body::Observed>(TokioIo::new(upstream))
        .await
        .map_err(stdio::Error::other)?;
    let driver = tokio::spawn(connection.with_upgrades());
    let sender = Arc::new(Mutex::new(sender));
    let handler = session.clone();
    let service = service_fn(move |request| handle(request, sender.clone(), handler.clone()));
    let connection = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .half_close(true)
        .serve_connection(TokioIo::new(client), service)
        .with_upgrades();
    let mut connection = Box::pin(connection);
    let result = tokio::select! {
        biased;
        result = &mut connection => result.map_err(stdio::Error::other),
        _ = session.finished.cancelled() => {
            connection.as_mut().graceful_shutdown();
            timeout(Duration::from_secs(5), &mut connection).await.map_err(stdio::Error::other).and_then(|result| result.map_err(stdio::Error::other))
        },
    };
    driver.abort();
    let _ = driver.await;
    // All body drops and upgrade jobs finish before the proxy relinquishes its
    // observation sender, so the recorder can flush complete metadata/payloads.
    drop(connection);
    session.writes.close();
    session.writes.wait().await;
    result
}

async fn handle(
    mut request: Request<Incoming>,
    sender: Arc<Mutex<hyper::client::conn::http1::SendRequest<body::Observed>>>,
    session: Session,
) -> Result<Response<HttpBody>, stdio::Error> {
    let message = MessageContext {
        request_id: session.inspection.ids.fetch_add(1, Ordering::Relaxed),
        session,
        start: Instant::now(),
        unix_ns: now(),
        response: false,
    };
    let authority = request
        .uri()
        .authority()
        .map(|value| value.to_string())
        .or_else(|| {
            request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| message.session.context.key.destination.to_string());
    let url = if request.uri().scheme().is_some() {
        request.uri().to_string()
    } else if request.method() == Method::CONNECT {
        format!("{}://{}", message.session.scheme, request.uri())
    } else {
        format!(
            "{}://{}{}",
            message.session.scheme,
            authority,
            request.uri()
        )
    };
    message.emit("network.http.request", json!({"method":request.method().as_str(),"url":url,"domain":authority,"version":format!("{:?}",request.version()),"headers":headers(request.headers()),"header_encoding":"latin1"})).await?;
    let connect = request.method() == Method::CONNECT;
    let upgrade_requested = connect || request.headers().contains_key("upgrade");
    let websocket_accept = (request.method() == Method::GET
        && header_token(request.headers(), "upgrade", "websocket")
        && header_token(request.headers(), "connection", "upgrade")
        && request
            .headers()
            .get("sec-websocket-version")
            .is_some_and(|value| value == "13"))
    .then(|| {
        request
            .headers()
            .get("sec-websocket-key")
            .map(|key| tungstenite::handshake::derive_accept_key(key.as_bytes()))
    })
    .flatten();
    let client_upgrade = upgrade_requested.then(|| hyper::upgrade::on(&mut request));
    let request = request.map(|inner| body::Observed::new(inner, message.clone(), None));
    let result = async {
        let mut sender = sender.lock().await;
        sender.ready().await?;
        sender.send_request(request).await
    }
    .await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            message.emit("network.http.error", json!({"error":error.to_string(),"completed_unix_ns":now(),"duration_ns":message.start.elapsed().as_nanos()})).await?;
            return Ok(Response::builder()
                .status(502)
                .header("connection", "close")
                .body(
                    Full::new(Bytes::from_static(
                        b"AgentTomo: upstream HTTP exchange failed\n",
                    ))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
                )
                .unwrap());
        }
    };
    message.emit("network.http.response", json!({"status_code":response.status().as_u16(),"version":format!("{:?}",response.version()),"headers":headers(response.headers()),"header_encoding":"latin1","response_started_unix_ns":now()})).await?;
    if (response.status() == StatusCode::SWITCHING_PROTOCOLS
        || connect && response.status().is_success())
        && let Some(client_upgrade) = client_upgrade
    {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let finished = message.session.finished.clone();
        let websocket = response.status() == StatusCode::SWITCHING_PROTOCOLS
            && header_token(response.headers(), "upgrade", "websocket")
            && header_token(response.headers(), "connection", "upgrade")
            && websocket_accept.as_ref().is_some_and(|expected| {
                response
                    .headers()
                    .get("sec-websocket-accept")
                    .is_some_and(|actual| actual.as_bytes() == expected.as_bytes())
            });
        let extensions = response
            .headers()
            .get_all("sec-websocket-extensions")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        if websocket {
            message.emit("network.stream.open", json!({"protocol":"websocket","extensions":extensions,"retention":"sliding_window"})).await?;
        }
        let upgraded_message = message.clone();
        message.session.writes.spawn(async move {
            let mut complete = false;
            if let Ok(Ok((client, upstream))) = timeout(Duration::from_secs(5), async {
                tokio::try_join!(client_upgrade, upstream_upgrade)
            })
            .await
            {
                let mut client: BoxIo = Box::new(TokioIo::new(client));
                let mut upstream: BoxIo = Box::new(TokioIo::new(upstream));
                if websocket {
                    client =
                        streams::websocket::observe(client, &upgraded_message, &extensions, true);
                    upstream = streams::websocket::observe(
                        upstream,
                        &upgraded_message,
                        &extensions,
                        false,
                    );
                }
                complete = io::opaque(client, upstream, &finished).await.is_ok()
                    && !finished.is_cancelled();
            }
            if websocket {
                streams::closed(upgraded_message, "websocket", complete).await;
            }
        });
    }
    let response_message = MessageContext {
        response: true,
        ..message
    };
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
    let encoding = sse.then(|| {
        response
            .headers()
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned()
    });
    if sse {
        response_message
            .emit(
                "network.stream.open",
                json!({"protocol":"sse","content_encoding":encoding,"retention":"sliding_window"}),
            )
            .await?;
    }
    Ok(response.map(|inner| body::Observed::new(inner, response_message, encoding).boxed_unsync()))
}
