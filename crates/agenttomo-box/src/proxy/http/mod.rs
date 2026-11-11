//! HTTP/1.1 and task-CA TLS inspection, independent of ptrace and the UI.
mod body;
mod io;
mod streams;
pub(crate) mod transport;

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
use hyper::body::Incoming;
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
    sync::{Mutex, mpsc},
    time::timeout_at,
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
    agents: crate::agent::Hub,
    semantic: crate::agent::store::Store,
    semantic_serial: Mutex<()>,
}
impl Inspection {
    pub async fn create(
        directory: &Path,
        options: Options,
        agents: crate::agent::Hub,
    ) -> stdio::Result<Option<Arc<Self>>> {
        if !options.enabled {
            return Ok(None);
        }
        let authority = Authority::new()?;
        Ok(Some(Arc::new(Self {
            authority,
            store: Store::create(directory, options.limits).await?,
            streams: crate::recording::streams::Store::create(directory, options.streams).await?,
            ids: AtomicU64::new(1),
            agents,
            semantic: crate::agent::store::Store::create(directory).await?,
            semantic_serial: Mutex::new(()),
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
    connect: bool,
    agent: Option<crate::agent::Scope>,
    encoding: String,
}
impl MessageContext {
    fn model_observer(&self) -> Option<crate::agent::Observer> {
        self.agent
            .clone()
            .map(|scope| crate::agent::Observer::new(scope, self.session.inspection.agents.clone()))
    }
    fn semantic(&self, events: Vec<crate::agent::Event>) {
        if events.is_empty() {
            return;
        }
        let message = self.clone();
        self.session.writes.spawn(async move {
            let _ordered = message.session.inspection.semantic_serial.lock().await;
            for mut event in events {
                if let Some(content) = event.content {
                    event.data["preview"] = content
                        .as_str()
                        .map(|text| text.chars().take(4096).collect::<String>())
                        .unwrap_or_else(|| "Structured content".to_owned())
                        .into();
                    event.data["payload"] =
                        match message.session.inspection.semantic.save(&content).await {
                            Ok(reference) => reference,
                            Err(error) => json!({"status":"unavailable","error":error.to_string()}),
                        };
                }
                event.data["request_id"] = message.request_id.into();
                event.data["started_unix_ns"] =
                    if let Some(started) = event.data.get("message_started_unix_ns") {
                        started.clone()
                    } else if event.kind == "agent.model.request" {
                        json!(message.unix_ns)
                    } else {
                        event.data["captured_unix_ns"].clone()
                    };
                event.data["start_source"] = if event.data.get("message_started_unix_ns").is_some()
                {
                    "proxy_frame_header"
                } else {
                    "model_protocol"
                }
                .into();
                let _ = message
                    .session
                    .events
                    .send(message.session.context.event(event.kind, event.data))
                    .await;
            }
        });
    }
    async fn emit(&self, kind: &'static str, mut data: Value) -> stdio::Result<()> {
        data["request_id"] = self.request_id.into();
        data["started_unix_ns"] = json!(self.unix_ns);
        data["start_source"] = "proxy_request_headers".into();
        if !self.connect {
            data["scheme"] = self.session.scheme.into();
        }
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
    client: BoxIo,
    upstream: BoxIo,
    inspection: Arc<Inspection>,
    context: &Context,
    events: &mpsc::Sender<Observation>,
    finished: CancellationToken,
) -> stdio::Result<transfer::Outcome> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut client = BufReader::new(client);
    let mut upstream = BufReader::new(upstream);
    let tls = tokio::select! {
        biased;
        bytes = client.fill_buf() => bytes?.first() == Some(&0x16),
        bytes = upstream.fill_buf() => { bytes?; false },
        _ = finished.cancelled() => false,
    };
    let counts = Arc::new(Counters::default());
    let client: BoxIo = Box::new(Metered {
        io: client,
        counts: counts.clone(),
    });
    let (client, upstream, scheme): (BoxIo, BoxIo, _) = if tls {
        let (client, upstream) = decrypt(
            client,
            Box::new(upstream),
            &inspection,
            context,
            events,
            &finished,
            None,
        )
        .await?;
        (client, upstream, "https")
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

// Both direct TLS and HTTP CONNECT tunnels use the same task CA and verifier.
async fn decrypt(
    client: BoxIo,
    upstream: BoxIo,
    inspection: &Arc<Inspection>,
    context: &Context,
    events: &mpsc::Sender<Observation>,
    finished: &CancellationToken,
    connect_host: Option<&str>,
) -> stdio::Result<(BoxIo, BoxIo)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let handshake = async {
        let start =
            tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), client)
                .await?;
        let (name, name_source) = start
            .client_hello()
            .server_name()
            .map(|name| (name.to_owned(), "tls_sni"))
            .or_else(|| {
                connect_host.map(|host| {
                    (
                        host.trim_matches(['[', ']']).to_owned(),
                        "connect_authority",
                    )
                })
            })
            .unwrap_or_else(|| (context.key.destination.ip().to_string(), "destination_ip"));
        let server_name =
            rustls::pki_types::ServerName::try_from(name.clone()).map_err(stdio::Error::other)?;
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
        Ok(pair) => Ok(pair),
        Err(error) => {
            events
                .send(context.event(
                    "network.tls.error",
                    json!({"error":error.to_string(),"stage":"handshake","tls_intercepted":false}),
                ))
                .await
                .map_err(stdio::Error::other)?;
            Err(error)
        }
    }
}

type HttpBody = UnsyncBoxBody<Bytes, hyper::Error>;
fn bridge(
    client: BoxIo,
    upstream: BoxIo,
    session: Session,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = stdio::Result<()>> + Send>> {
    Box::pin(async move {
        transport::bridge(
            client,
            upstream,
            session.finished.clone(),
            session.writes.clone(),
            move |request, sender| handle(request, sender, session.clone()),
        )
        .await
    })
}

async fn tunnel(
    client: BoxIo,
    upstream: BoxIo,
    mut session: Session,
    connect_host: Option<String>,
) -> stdio::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut client = BufReader::new(client);
    let mut upstream = BufReader::new(upstream);
    let tls = tokio::select! {
        bytes = client.fill_buf() => bytes?.first() == Some(&0x16),
        bytes = upstream.fill_buf() => { bytes?; false },
        _ = session.finished.cancelled() => return Ok(()),
    };
    session.scheme = if tls { "https" } else { "http" };
    let (client, upstream): (BoxIo, BoxIo) = if tls {
        decrypt(
            Box::new(client),
            Box::new(upstream),
            &session.inspection,
            &session.context,
            &session.events,
            &session.finished,
            connect_host.as_deref(),
        )
        .await?
    } else {
        (Box::new(client), Box::new(upstream))
    };
    let (http, client, upstream) = io::detect(client, upstream, &session.finished).await?;
    if http {
        bridge(client, upstream, session).await
    } else {
        io::opaque(client, upstream, &session.finished).await
    }
}

async fn handle(
    mut request: Request<Incoming>,
    sender: Arc<Mutex<hyper::client::conn::http1::SendRequest<body::Observed>>>,
    session: Session,
) -> Result<Response<HttpBody>, stdio::Error> {
    let agent = (matches!(*request.method(), Method::POST | Method::GET)
        && request
            .uri()
            .path()
            .trim_end_matches('/')
            .ends_with("/responses"))
    .then(|| {
        session
            .context
            .metadata
            .as_ref()
            .filter(|metadata| metadata.actors.len() == 1)
            .and_then(|metadata| {
                session
                    .inspection
                    .agents
                    .binding(metadata.actors[0].process_instance_id)
            })
            .map(|binding| binding.scope)
    })
    .flatten();
    let encoding = request
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let connect = request.method() == Method::CONNECT;
    let message = MessageContext {
        request_id: session.inspection.ids.fetch_add(1, Ordering::Relaxed),
        session,
        start: Instant::now(),
        unix_ns: now(),
        response: false,
        connect,
        agent,
        encoding,
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
    let url = if connect || request.uri().scheme().is_some() {
        request.uri().to_string()
    } else {
        format!(
            "{}://{}{}",
            message.session.scheme,
            authority,
            request.uri()
        )
    };
    message.emit("network.http.request", json!({"method":request.method().as_str(),"url":url,"domain":authority,"version":format!("{:?}",request.version()),"headers":headers(request.headers()),"header_encoding":"latin1"})).await?;
    let connect_host = request.uri().host().map(str::to_owned);
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
    let result = transport::send(sender, request).await;
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
            if let Ok((mut client, mut upstream)) =
                transport::upgraded(client_upgrade, upstream_upgrade).await
            {
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
                complete = if connect {
                    let session = Session {
                        scheme: "https",
                        writes: TaskTracker::new(),
                        ..upgraded_message.session.clone()
                    };
                    tunnel(client, upstream, session, connect_host)
                        .await
                        .is_ok()
                } else {
                    io::opaque(client, upstream, &finished).await.is_ok()
                } && !finished.is_cancelled();
            }
            if websocket {
                streams::closed(upgraded_message, "websocket", complete).await;
            }
        });
    }
    let response_message = MessageContext {
        response: true,
        encoding: response
            .headers()
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned(),
        ..message
    };
    let sse = response_message.agent.is_some() && !upgrade_requested
        || response
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
