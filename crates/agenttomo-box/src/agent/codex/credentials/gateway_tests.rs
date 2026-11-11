use super::super::prepare;
use super::*;
use futures_util::{SinkExt, StreamExt};
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use std::collections::BTreeMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

struct Harness {
    gateway: Arc<Gateway>,
    proxy: Proxy,
    destination: SocketAddr,
    header: String,
    worker: tokio::task::JoinHandle<io::Result<()>>,
}
impl Harness {
    async fn start(destination: SocketAddr, upstream_ca: Option<&Authority>) -> Self {
        let scheme = if upstream_ca.is_some() {
            "https"
        } else {
            "http"
        };
        let config = format!(
            "[model_providers.test]\nbase_url='{scheme}://{destination}/v1'\nexperimental_bearer_token='host-only-test-secret'\n"
        );
        let prepared = prepare(
            Path::new("/unused"),
            BTreeMap::from([("config.toml".into(), config)]),
            BTreeMap::new(),
        )
        .unwrap();
        let config: toml::Value = toml::from_str(&prepared.files["config.toml"]).unwrap();
        let header = format!(
            "Bearer {}",
            config["model_providers"]["test"]["experimental_bearer_token"]
                .as_str()
                .unwrap()
        );
        let mut gateway = Gateway::new().unwrap();
        if let Some(authority) = upstream_ca {
            gateway.authority.client = client_trust(authority);
        }
        gateway.register("test-owner".into(), prepared.policy);
        let gateway = Arc::new(gateway);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy: Proxy = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let handler = gateway.clone();
        let worker = tokio::spawn(async move {
            let (client, _) = listener.accept().await?;
            handler
                .serve(client, "test-owner".into(), None, CancellationToken::new())
                .await
        });
        Self {
            gateway,
            proxy,
            destination,
            header,
            worker,
        }
    }
    async fn tunnel(&self, tls: bool) -> BoxIo {
        let tunnel = outbound::connect(self.destination, Some(&self.proxy), None)
            .await
            .unwrap();
        if !tls {
            return tunnel;
        }
        let connector = tokio_rustls::TlsConnector::from(client_trust(&self.gateway.authority));
        let name = rustls::pki_types::ServerName::from(self.destination.ip());
        Box::new(connector.connect(name, tunnel).await.unwrap())
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.gateway.remove("test-owner");
        self.worker.abort();
    }
}
fn client_trust(authority: &Authority) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(authority.pem.as_bytes()).unwrap())
        .unwrap();
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}
async fn headers(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
        assert!(bytes.len() < 16384);
    }
    String::from_utf8(bytes).unwrap()
}
use std::path::Path;

#[tokio::test]
async fn gateway_injects_http_and_https_headers_and_streams_before_completion() {
    for tls in [false, true] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let authority = tls.then(|| Authority::new().unwrap());
        let harness = Harness::start(address, authority.as_ref()).await;
        let (sent, received) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let mut stream: BoxIo = if let Some(authority) = authority {
                Box::new(
                    tokio_rustls::TlsAcceptor::from(authority.server("127.0.0.1").unwrap())
                        .accept(stream)
                        .await
                        .unwrap(),
                )
            } else {
                Box::new(stream)
            };
            sent.send(headers(&mut stream).await).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 11\r\n\r\nfirst").await.unwrap();
            stream.flush().await.unwrap();
            let _ = released.await;
            stream.write_all(b"second").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let mut client = harness.tunnel(tls).await;
        client.write_all(format!("GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n", harness.header).as_bytes()).await.unwrap();
        let sent = timeout(Duration::from_secs(3), received)
            .await
            .unwrap()
            .unwrap()
            .to_ascii_lowercase();
        assert!(sent.contains("authorization: bearer host-only-test-secret\r\n"));
        assert!(!sent.contains("agenttomo-credential-"));
        assert!(headers(&mut client).await.starts_with("HTTP/1.1 200"));
        let mut first = [0; 5];
        timeout(Duration::from_secs(3), client.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first, b"first");
        release.send(()).unwrap();
        let mut second = [0; 6];
        client.read_exact(&mut second).await.unwrap();
        assert_eq!(&second, b"second");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn revoking_a_session_closes_its_active_response_stream() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let harness = Harness::start(address, None).await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        headers(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\nfirst")
            .await
            .unwrap();
        let mut byte = [0];
        let _ = stream.read(&mut byte).await;
    });
    let mut client = harness.tunnel(false).await;
    client
        .write_all(
            format!(
                "GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\n\r\n",
                harness.header
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    headers(&mut client).await;
    let mut first = [0; 5];
    client.read_exact(&mut first).await.unwrap();
    harness.gateway.remove("test-owner");
    let result = timeout(Duration::from_secs(3), client.read_u8())
        .await
        .unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_upgrade_keeps_bidirectional_messages_and_upstream_authentication() {
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let harness = Harness::start(address, None).await;
    let server = tokio::spawn(async move {
        let (stream, _) = upstream.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tungstenite::handshake::server::Request, response| {
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer host-only-test-secret"
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::Text("client-message".into())
        );
        websocket
            .send(Message::Text("server-message".into()))
            .await
            .unwrap();
    });
    let tunnel = harness.tunnel(false).await;
    let mut request = format!("ws://{address}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", harness.header.parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::client_async(request, tunnel)
        .await
        .unwrap();
    websocket
        .send(Message::Text("client-message".into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Text("server-message".into())
    );
    server.await.unwrap();
}

#[tokio::test]
async fn unrelated_tcp_keeps_server_first_bytes_and_half_close() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let mut harness = Harness::start("192.0.2.1:443".parse().unwrap(), None).await;
    harness.destination = address;
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        stream.write_all(b"server-first").await.unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"client-data");
        stream.write_all(b"final-reply").await.unwrap();
    });
    let mut client = harness.tunnel(false).await;
    let mut greeting = [0; 12];
    timeout(Duration::from_secs(3), client.read_exact(&mut greeting))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&greeting, b"server-first");
    client.write_all(b"client-data").await.unwrap();
    client.shutdown().await.unwrap();
    let mut body = Vec::new();
    timeout(Duration::from_secs(3), client.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"final-reply");
    server.await.unwrap();
}

#[tokio::test]
async fn credential_requests_cannot_use_header_echo_methods_or_unrelated_endpoints() {
    for (method, path) in [("TRACE", "/v1/responses"), ("POST", "/v1/debug/headers")] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let harness = Harness::start(address, None).await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut bytes = Vec::new();
            while let Ok(byte) = stream.read_u8().await {
                bytes.push(byte);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            if !bytes.is_empty() {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            }
        });
        let mut client = harness.tunnel(false).await;
        client.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n", harness.header).as_bytes()).await.unwrap();
        let mut response = String::new();
        timeout(Duration::from_secs(3), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(!response.contains("host-only-test-secret"));
        assert!(response.starts_with("HTTP/1.1 502"));
        timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
