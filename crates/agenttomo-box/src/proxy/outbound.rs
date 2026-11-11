//! Establish upstream transport before protocol inspection. CONNECT belongs to
//! the box, so its headers never enter the application's traffic/evidence.
use std::{fmt, io, net::SocketAddr, str::FromStr};

use axum::body::Bytes;
use http_body_util::Empty;
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_util::task::AbortOnDropHandle;

use super::BoxIo;

pub const ENVIRONMENT_NAMES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Proxy(Uri);

impl Default for Proxy {
    fn default() -> Self {
        Self(Uri::from_static("http://127.0.0.1:8118"))
    }
}

impl FromStr for Proxy {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uri: Uri = value.parse().map_err(|_| "invalid upstream proxy URL")?;
        if uri.scheme_str() != Some("http") || uri.host().is_none() {
            return Err("--proxy requires an HTTP CONNECT proxy: http://host:port".into());
        }
        if uri
            .authority()
            .is_some_and(|value| value.as_str().contains('@'))
        {
            return Err("proxy authentication is not supported; use an unauthenticated HTTP CONNECT endpoint".into());
        }
        if !matches!(uri.path(), "" | "/") || uri.query().is_some() || uri.port_u16() == Some(0) {
            return Err("proxy URL must contain only http://host[:port]".into());
        }
        Ok(Self(uri))
    }
}
impl TryFrom<String> for Proxy {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<Proxy> for String {
    fn from(value: Proxy) -> Self {
        value.to_string()
    }
}
impl fmt::Display for Proxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl Proxy {
    pub fn host(&self) -> &str {
        self.0.host().unwrap().trim_matches(['[', ']'])
    }
    pub fn port(&self) -> u16 {
        self.0.port_u16().unwrap_or(80)
    }
}

pub async fn connect(
    destination: SocketAddr,
    proxy: Option<&Proxy>,
    domain: Option<&str>,
) -> io::Result<BoxIo> {
    let Some(proxy) = proxy else {
        return Ok(Box::new(TcpStream::connect(destination).await?));
    };
    let stream = TcpStream::connect((proxy.host(), proxy.port()))
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot reach upstream proxy: {error}"),
            )
        })?;
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(16384)
        .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    // Dropping a timed-out connect must also drop its connection driver/socket.
    let _driver = AbortOnDropHandle::new(tokio::spawn(connection.with_upgrades()));
    let authority = domain
        .map(|host| format!("{host}:{}", destination.port()))
        .unwrap_or_else(|| destination.to_string());
    let request = Request::builder()
        .method("CONNECT")
        .uri(&authority)
        .header("host", &authority)
        .body(Empty::new())
        .map_err(io::Error::other)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| io::Error::other(format!("upstream proxy CONNECT failed: {error}")))?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "upstream proxy CONNECT rejected: {}",
            response.status()
        )));
    }
    // Hyper's upgrade preserves bytes read after the response headers, which
    // matters for server-first protocols. Never discard them by taking the FD.
    let tunnel = hyper::upgrade::on(response)
        .await
        .map_err(io::Error::other)?;
    Ok(Box::new(TokioIo::new(tunnel)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        time::{Duration, timeout},
    };

    #[test]
    fn configuration_rejects_unsupported_or_ambiguous_endpoints() {
        for url in [
            "socks5://localhost:1080",
            "https://localhost:8118",
            "localhost:8118",
            "http://user:secret@localhost",
            "http://localhost/path",
            "http://localhost?key=secret",
            "http://localhost:0",
        ] {
            assert!(url.parse::<Proxy>().is_err(), "{url}");
        }
        let proxy: Proxy = "http://[::1]:8118".parse().unwrap();
        assert_eq!(proxy.host(), "::1");
        assert_eq!(proxy.port(), 8118);
    }

    #[tokio::test]
    async fn tunnel_preserves_server_first_bytes_ipv6_authority_and_half_close() {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy: Proxy = format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line, "CONNECT [2001:db8::1]:25 HTTP/1.1\r\n");
                loop {
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\n\r\nserver-first")
                    .await
                    .unwrap();
                let mut input = Vec::new();
                socket.read_to_end(&mut input).await.unwrap();
                assert_eq!(input, b"client-data");
                socket.write_all(b"after-half-close").await.unwrap();
                socket.shutdown().await.unwrap();
            });
            let mut tunnel = connect("[2001:db8::1]:25".parse().unwrap(), Some(&proxy), None)
                .await
                .unwrap();
            let mut first = [0; 12];
            tunnel.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"server-first");
            tunnel.write_all(b"client-data").await.unwrap();
            tunnel.shutdown().await.unwrap();
            let mut tail = Vec::new();
            tunnel.read_to_end(&mut tail).await.unwrap();
            assert_eq!(tail, b"after-half-close");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rejection_does_not_connect_directly_or_leak_the_proxy_response() {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy: Proxy = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                loop {
                    let mut line = String::new();
                    assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" { break; }
                }
                socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 6\r\n\r\nsecret").await.unwrap();
            });
            let error = connect(origin.local_addr().unwrap(), Some(&proxy), None).await.err().unwrap();
            assert!(error.to_string().contains("407"));
            assert!(!error.to_string().contains("secret"));
            assert!(timeout(Duration::from_millis(100), origin.accept()).await.is_err());
            server.await.unwrap();
        }).await.unwrap();
    }
}
