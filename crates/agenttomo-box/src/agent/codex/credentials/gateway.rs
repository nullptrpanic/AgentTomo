//! Host-only forwarding. Credential policy is applied after container capture.
#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;

use super::{Origin, Policy, invalid};
use crate::proxy::{
    BoxIo,
    http::transport,
    outbound::{self, Proxy},
    tls::Authority,
};
use axum::body::Bytes;
use axum::http::{Request, Response, StatusCode, Uri, uri::Authority as HttpAuthority};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

pub struct Gateway {
    authority: Authority,
    sessions: Mutex<HashMap<String, Arc<Policy>>>,
}
impl Gateway {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            authority: Authority::new()?,
            sessions: Mutex::new(HashMap::new()),
        })
    }
    pub fn certificate(&self) -> String {
        self.authority.pem.clone()
    }
    pub fn register(&self, name: String, policy: Policy) {
        if let Some(previous) = self.sessions.lock().unwrap().insert(name, Arc::new(policy)) {
            previous.lifetime.cancel();
        }
    }
    pub fn remove(&self, name: &str) {
        if let Some(policy) = self.sessions.lock().unwrap().remove(name) {
            policy.lifetime.cancel();
        }
    }

    pub async fn serve(
        self: Arc<Self>,
        client: TcpStream,
        owner: String,
        proxy: Option<Proxy>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let policy = self
            .sessions
            .lock()
            .unwrap()
            .get(&owner)
            .cloned()
            .ok_or_else(|| invalid("credential session is unavailable"))?;
        let jobs = TaskTracker::new();
        let stop = shutdown.child_token();
        let _cancel = stop.clone().drop_guard();
        let handler_stop = stop.clone();
        let handler_jobs = jobs.clone();
        let handler_policy = policy.clone();
        let handler = self.clone();
        let service =
            service_fn(move |mut request: Request<Incoming>| {
                let jobs = handler_jobs.clone();
                let policy = handler_policy.clone();
                let gateway = handler.clone();
                let proxy = proxy.clone();
                let stop = handler_stop.clone();
                async move {
                    if request.method() != "CONNECT" {
                        return Ok::<_, Infallible>(failure(
                            StatusCode::METHOD_NOT_ALLOWED,
                            "expected a TCP CONNECT request",
                        ));
                    }
                    let Some(target) = request.uri().authority().cloned().filter(|target| {
                        !target.as_str().contains('@')
                            && target.port_u16().is_some_and(|port| port != 0)
                    }) else {
                        return Ok(failure(
                            StatusCode::BAD_REQUEST,
                            "invalid CONNECT destination",
                        ));
                    };
                    let upstream =
                        match timeout(Duration::from_secs(15), connect(&target, proxy.as_ref()))
                            .await
                        {
                            Ok(Ok(upstream)) => upstream,
                            _ => {
                                return Ok(failure(
                                    StatusCode::BAD_GATEWAY,
                                    "cannot establish the host upstream connection",
                                ));
                            }
                        };
                    let upgraded = hyper::upgrade::on(&mut request);
                    jobs.spawn(async move {
                        let forward = async {
                            let client: BoxIo =
                                Box::new(TokioIo::new(upgraded.await.map_err(io::Error::other)?));
                            match policy.origin(&target) {
                                Some(origin) => {
                                    let (client, upstream) =
                                        gateway.transport(client, upstream, &origin).await?;
                                    forward_http(client, upstream, policy.clone(), origin).await
                                }
                                None => transport::opaque(client, upstream, &stop).await,
                            }
                        };
                        tokio::select! {
                            result = forward => { let _ = result; },
                            _ = policy.lifetime.cancelled() => {},
                            _ = stop.cancelled() => {},
                        }
                    });
                    Ok(Response::new(empty()))
                }
            });
        let connection = hyper::server::conn::http1::Builder::new()
            .max_buf_size(16384)
            .serve_connection(TokioIo::new(client), service)
            .with_upgrades();
        let running = async {
            connection.await.map_err(io::Error::other)?;
            jobs.close();
            jobs.wait().await;
            Ok(())
        };
        tokio::select! {
            result = running => result,
            _ = shutdown.cancelled() => Err(invalid("host forwarding has stopped")),
            _ = policy.lifetime.cancelled() => Err(invalid("credential session has ended")),
        }
    }

    async fn transport(
        &self,
        client: BoxIo,
        upstream: BoxIo,
        origin: &Origin,
    ) -> io::Result<(BoxIo, BoxIo)> {
        if !origin.tls {
            return Ok((client, upstream));
        }
        let name = rustls::pki_types::ServerName::try_from(origin.host.clone())
            .map_err(io::Error::other)?;
        let server = tokio_rustls::TlsAcceptor::from(self.authority.server(&origin.host)?);
        let connector = tokio_rustls::TlsConnector::from(self.authority.client.clone());
        let handshake = async {
            let (client, upstream) =
                tokio::try_join!(server.accept(client), connector.connect(name, upstream))?;
            let sni = client.get_ref().1.server_name();
            if !sni.is_some_and(|name| {
                name.trim_end_matches('.')
                    .eq_ignore_ascii_case(origin.host.trim_end_matches('.'))
            }) && origin.host.parse::<IpAddr>().is_err()
            {
                return Err(invalid(
                    "credential TLS name does not match the CONNECT destination",
                ));
            }
            Ok((Box::new(client) as BoxIo, Box::new(upstream) as BoxIo))
        };
        timeout(Duration::from_secs(15), handshake)
            .await
            .map_err(io::Error::other)?
    }
}

async fn connect(target: &HttpAuthority, proxy: Option<&Proxy>) -> io::Result<BoxIo> {
    let host = target.host().trim_matches(['[', ']']);
    let port = target
        .port_u16()
        .ok_or_else(|| invalid("CONNECT port is required"))?;
    if proxy.is_some() {
        let ip = host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let domain = host.parse::<IpAddr>().is_err().then_some(host);
        outbound::connect(SocketAddr::new(ip, port), proxy, domain).await
    } else {
        Ok(Box::new(TcpStream::connect((host, port)).await?))
    }
}

async fn forward_http(
    client: BoxIo,
    upstream: BoxIo,
    policy: Arc<Policy>,
    origin: Origin,
) -> io::Result<()> {
    let jobs = TaskTracker::new();
    let stop = CancellationToken::new();
    let _cancel = stop.clone().drop_guard();
    let handler_stop = stop.clone();
    let handler_jobs = jobs.clone();
    let handler = move |mut request: Request<Incoming>,
                        sender: Arc<
        tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<Incoming>>,
    >| {
        let policy = policy.clone();
        let origin = origin.clone();
        let jobs = handler_jobs.clone();
        let stop = handler_stop.clone();
        async move {
            let uri = request.uri().clone();
            let method = request.method().clone();
            if let Err(error) = policy.inject(&origin, &method, &uri, request.headers_mut()) {
                return Ok::<_, io::Error>(failure(StatusCode::BAD_GATEWAY, &error.to_string()));
            }
            let client_upgrade = hyper::upgrade::on(&mut request);
            *request.uri_mut() = uri
                .path_and_query()
                .cloned()
                .map(Uri::from)
                .unwrap_or_else(|| Uri::from_static("/"));
            let response = transport::send(sender, request).await;
            let mut response = match response {
                Ok(response) => response,
                Err(_) => {
                    return Ok(failure(
                        StatusCode::BAD_GATEWAY,
                        "host model forwarding failed",
                    ));
                }
            };
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                let upstream_upgrade = hyper::upgrade::on(&mut response);
                jobs.spawn(async move {
                    let copy = async {
                        if let Ok((client, upstream)) =
                            transport::upgraded(client_upgrade, upstream_upgrade).await
                        {
                            let _ = transport::opaque(client, upstream, &stop).await;
                        }
                    };
                    tokio::select! { _ = copy => {}, _ = stop.cancelled() => {} }
                });
            }
            Ok(response.map(BodyExt::boxed_unsync))
        }
    };
    transport::bridge(client, upstream, stop, jobs, handler).await
}

fn empty() -> Body {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed_unsync()
}
fn failure(status: StatusCode, message: &str) -> Response<Body> {
    let body =
        serde_json::json!({"error":{"message":message,"type":"agenttomo_credential_gateway"}})
            .to_string();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}
