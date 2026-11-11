//! Bounded, passive connection-preface inspection. No target memory, I/O,
//! certificates, TLS termination or dependency on the syscall collector.

use axum::http::uri::{Authority, Uri};
use rustls::server::Acceptor;

const LIMIT: usize = 64 * 1024;

#[derive(Debug)]
pub struct Name {
    pub protocol: &'static str,
    pub domain: Option<String>,
    pub source: &'static str,
}

enum State {
    Start,
    Http(Vec<u8>),
    Tls(Box<Acceptor>),
    Done,
}

pub struct Inspector {
    state: State,
    remaining: usize,
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            state: State::Start,
            remaining: LIMIT,
        }
    }
}

impl Inspector {
    pub fn observe(&mut self, bytes: &[u8]) -> Option<Name> {
        if bytes.is_empty() || matches!(self.state, State::Done) {
            return None;
        }
        let mut bytes = &bytes[..bytes.len().min(self.remaining)];
        self.remaining -= bytes.len();
        if matches!(self.state, State::Start) {
            self.state = match bytes[0] {
                0x16 => State::Tls(Box::default()),
                b'A'..=b'Z' => State::Http(Vec::new()),
                _ => State::Done,
            };
        }
        let result = match &mut self.state {
            State::Http(buffer) => {
                buffer.extend_from_slice(bytes);
                http_name(buffer).map_err(|_| ())
            }
            State::Tls(acceptor) => tls_name(acceptor, &mut bytes),
            _ => return None,
        };
        match result {
            Ok(None) if self.remaining > 0 => None,
            result => {
                // Release buffered headers/ClientHello immediately, including
                // on malformed/oversized input. Parsing never rejects traffic.
                self.state = State::Done;
                result.ok().flatten()
            }
        }
    }
}

/// Reuse passive parsing for upstream routing. Unknown/server-first protocols
/// retain IP routing after a bounded wait; consumed bytes are always replayed.
pub(super) async fn preface(
    mut client: crate::proxy::BoxIo,
    finished: &tokio_util::sync::CancellationToken,
) -> std::io::Result<(Option<Name>, crate::proxy::BoxIo)> {
    use tokio::io::AsyncReadExt;
    let mut inspector = Inspector::default();
    let mut prefix = Vec::new();
    let mut buffer = [0; 8192];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    let name = loop {
        let available = buffer.len().min(LIMIT - prefix.len());
        let count = tokio::select! {
            biased;
            count = client.read(&mut buffer[..available]) => count?,
            _ = finished.cancelled() => break None,
            _ = tokio::time::sleep_until(deadline) => break None,
        };
        if count == 0 {
            break None;
        }
        prefix.extend_from_slice(&buffer[..count]);
        let name = inspector.observe(&buffer[..count]);
        if matches!(inspector.state, State::Done) {
            break name;
        }
    };
    Ok((name, crate::proxy::replay::replay(prefix, client)))
}

fn tls_name(acceptor: &mut Acceptor, bytes: &mut &[u8]) -> Result<Option<Name>, ()> {
    while !bytes.is_empty() {
        if acceptor.read_tls(bytes).map_err(|_| ())? == 0 {
            return Err(());
        }
        if let Some(accepted) = acceptor.accept().map_err(|_| ())? {
            return Ok(Some(Name {
                protocol: "tls",
                domain: accepted.client_hello().server_name().map(str::to_owned),
                source: "tls_sni",
            }));
        }
    }
    Ok(None)
}

fn http_name(bytes: &[u8]) -> Result<Option<Name>, httparse::Error> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    if request.parse(bytes)?.is_partial() {
        return Ok(None);
    }
    // A proxy's absolute request target (or CONNECT authority) determines its
    // destination even if a conflicting Host header is present.
    let target = request.path.unwrap_or_default();
    let authority = if request.method == Some("CONNECT") {
        target.parse::<Authority>().ok()
    } else {
        target
            .parse::<Uri>()
            .ok()
            .and_then(|uri| uri.authority().cloned())
    };
    let (domain, source) = if let Some(authority) = authority {
        (host(authority), "http_authority")
    } else {
        let mut hosts = request
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("host"));
        let authority = hosts
            .next()
            .and_then(|header| std::str::from_utf8(header.value).ok())
            .and_then(|value| value.trim().parse::<Authority>().ok());
        let domain = if hosts.next().is_none() {
            authority.and_then(host)
        } else {
            None
        };
        (domain, "http_host")
    };
    Ok(Some(Name {
        protocol: "http",
        domain,
        source,
    }))
}

fn host(authority: Authority) -> Option<String> {
    // Host headers cannot contain userinfo. Do not expose credentials as a name.
    (!authority.as_str().contains('@')).then(|| authority.host().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &[u8] = include_bytes!("../../../tests/fixtures/client-hello-sni.bin");
    const NO_SNI: &[u8] = include_bytes!("../../../tests/fixtures/client-hello-no-sni.bin");

    #[tokio::test]
    async fn routing_replays_fragmented_and_unrecognized_prefaces_exactly() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (bytes, expected) in [
            (HELLO.to_vec(), Some("tls.example.test")),
            (
                b"GET / HTTP/1.1\r\nHost: origin.test\r\n\r\npayload".to_vec(),
                Some("origin.test"),
            ),
            (NO_SNI.to_vec(), None),
            ((0..=255).collect::<Vec<_>>(), None),
            (
                [
                    b"GET / HTTP/1.1\r\nX-Large: ".as_slice(),
                    &vec![b'x'; LIMIT + 1],
                ]
                .concat(),
                None,
            ),
        ] {
            let (mut app, stream) = tokio::io::duplex(64);
            let original = bytes.clone();
            let writer = tokio::spawn(async move {
                for chunk in bytes.chunks(7) {
                    app.write_all(chunk).await.unwrap();
                }
                app.shutdown().await.unwrap();
                let mut reply = Vec::new();
                app.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, b"reply");
            });
            let (name, mut replay) = preface(
                Box::new(stream),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(name.and_then(|name| name.domain).as_deref(), expected);
            let mut received = Vec::new();
            replay.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, original);
            replay.write_all(b"reply").await.unwrap();
            replay.shutdown().await.unwrap();
            writer.await.unwrap();
        }
    }

    fn fragmented(bytes: &[u8], size: usize) -> Name {
        let mut inspector = Inspector::default();
        let mut names = Vec::new();
        for chunk in bytes.chunks(size) {
            names.extend(inspector.observe(chunk));
        }
        assert_eq!(names.len(), 1);
        names.pop().unwrap()
    }

    #[test]
    fn http_headers_survive_fragmentation_and_do_not_parse_a_body_as_another_request() {
        let bytes = b"POST / HTTP/1.1\r\nhOsT: HTTP.Example.Test:8080\r\nContent-Length: 37\r\n\r\nGET / HTTP/1.1\r\nHost: body.test\r\n\r\n";
        for size in [1, 7, bytes.len()] {
            let name = fragmented(bytes, size);
            assert_eq!(name.domain.as_deref(), Some("http.example.test"));
            assert_eq!(name.source, "http_host");
        }
    }

    #[test]
    fn http_authorities_and_missing_or_ambiguous_hosts_are_handled_explicitly() {
        for (request, expected, source) in [
            (
                "GET http://absolute.test:80/ HTTP/1.1\r\nHost: conflicting.test\r\n\r\n",
                Some("absolute.test"),
                "http_authority",
            ),
            (
                "CONNECT tunnel.test:443 HTTP/1.1\r\n\r\n",
                Some("tunnel.test"),
                "http_authority",
            ),
            ("GET / HTTP/1.0\r\n\r\n", None, "http_host"),
            (
                "GET / HTTP/1.1\r\nHost: a.test\r\nHost: b.test\r\n\r\n",
                None,
                "http_host",
            ),
            (
                "GET / HTTP/1.1\r\nHost: secret@host.test\r\n\r\n",
                None,
                "http_host",
            ),
        ] {
            let name = fragmented(request.as_bytes(), 3);
            assert_eq!(name.domain.as_deref(), expected);
            assert_eq!(name.source, source);
        }
    }

    #[test]
    fn tls_client_hello_survives_tcp_and_tls_record_fragmentation_without_a_crypto_provider() {
        for size in [1, 7, HELLO.len()] {
            let name = fragmented(HELLO, size);
            assert_eq!(name.protocol, "tls");
            assert_eq!(name.domain.as_deref(), Some("tls.example.test"));
        }
        // Split the actual OpenSSL handshake across two TLS records as well.
        let mut records = Vec::new();
        for part in [&HELLO[5..32], &HELLO[32..]] {
            records.extend_from_slice(&HELLO[..3]);
            records.extend_from_slice(&(part.len() as u16).to_be_bytes());
            records.extend_from_slice(part);
        }
        assert_eq!(
            fragmented(&records, 11).domain.as_deref(),
            Some("tls.example.test")
        );
        assert!(fragmented(NO_SNI, 7).domain.is_none());
    }

    #[test]
    fn invalid_unknown_and_oversized_prefaces_stop_inspection_without_guessing() {
        for bytes in [
            b"\x00not-http".as_slice(),
            b"\x16\x03\x03\xff\xffinvalid",
            b"GET / HTTP/9.9\r\n\r\n",
        ] {
            let mut inspector = Inspector::default();
            assert!(inspector.observe(bytes).is_none());
            assert!(inspector.observe(HELLO).is_none());
            assert!(matches!(inspector.state, State::Done));
        }
        let mut inspector = Inspector::default();
        assert!(inspector.observe(b"GET / HTTP/1.1\r\nX-Large: ").is_none());
        assert!(inspector.observe(&vec![b'x'; LIMIT + 1]).is_none());
        assert!(matches!(inspector.state, State::Done));
        assert!(
            inspector
                .observe(b"\r\nHost: after-limit.test\r\n\r\n")
                .is_none()
        );
    }
}
