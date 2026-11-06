#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

#[allow(dead_code)]
mod common;
use common::Fixture;
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};

fn fixture() -> Fixture {
    let directory = std::env::temp_dir().join(format!("agenttomo-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    Fixture(directory)
}

fn read_headers(stream: &mut impl Read) -> Vec<u8> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
        assert!(headers.len() < 65536);
    }
    headers
}

fn respond(mut stream: impl Read + Write) {
    let mut request = Vec::new();
    let mut byte = [0];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&request).starts_with("POST /echo HTTP/1.1"));
    let mut body = [0; 12];
    stream.read_exact(&mut body).unwrap();
    assert_eq!(&body, b"hello-upload");
    stream.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 14\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nhello-response").unwrap();
    stream.flush().unwrap();
}

fn check_capture(fixture: &Fixture, scheme: &str) {
    let events = fixture.events();
    let event = |kind| -> &Value {
        events
            .iter()
            .find(|e| e["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind}: {events:?}"))
    };
    let request = &event("network.http.request")["data"];
    assert_eq!(request["method"], "POST");
    assert_eq!(request["scheme"], scheme);
    assert!(request["pid"].is_number());
    assert_eq!(event("network.http.response")["data"]["status_code"], 201);
    let payload = std::fs::read(fixture.0.join("run/http-bodies.bin")).unwrap();
    assert_eq!(
        payload.len(),
        8,
        "budget is shared between request and response"
    );
    for (kind, expected, length) in [
        ("network.http.request.body", b"hell", 12),
        ("network.http.response.body", b"hell", 14),
    ] {
        let data = &event(kind)["data"];
        assert_eq!(data["request_id"], request["request_id"]);
        assert_eq!(data["started_unix_ns"], request["started_unix_ns"]);
        assert!(data["completed_unix_ns"].as_u64() >= data["started_unix_ns"].as_u64());
        let body = &data["body"];
        assert_eq!(body["status"], "truncated");
        assert_eq!(body["observed_bytes"], length);
        assert_eq!(body["captured_bytes"], 4);
        let offset = body["offset"].as_u64().unwrap() as usize;
        assert_eq!(&payload[offset..offset + 4], expected);
    }
}

#[test]
fn http_prefix_limits_do_not_truncate_forwarded_requests_or_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        respond(stream);
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 10 -fsS -d hello-upload http://{address}/echo"
    ));
    command.args([
        "--capture-http",
        "--http-body-max-bytes",
        "4",
        "--http-body-total-bytes",
        "8",
    ]);
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"hello-response");
    server.join().unwrap();
    check_capture(&fixture, "http");
}

#[test]
fn https_uses_overlay_trust_without_changing_the_original_bundle() {
    let before = std::fs::read("/etc/ssl/certs/ca-certificates.crt").unwrap();
    let fixture = fixture();
    let mut ca = rcgen::CertificateParams::default();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let trust = fixture.0.join("upstream-ca.pem");
    std::fs::write(&trust, ca_cert.pem()).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &rcgen::Issuer::new(ca, ca_key))
        .unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let connection = rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap();
        respond(rustls::StreamOwned::new(connection, stream));
    });
    let mut command = fixture.command(&format!("exec env -u SSL_CERT_FILE curl --noproxy '*' --resolve localhost:{port}:127.0.0.1 --max-time 10 -fsS -d hello-upload https://localhost:{port}/echo"));
    command.env("SSL_CERT_FILE", trust).args([
        "--capture-http",
        "--http-body-max-bytes",
        "4",
        "--http-body-total-bytes",
        "8",
    ]);
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"hello-response");
    server.join().unwrap();
    check_capture(&fixture, "https");
    assert_eq!(
        std::fs::read("/etc/ssl/certs/ca-certificates.crt").unwrap(),
        before
    );
    let public_ca = std::fs::read_to_string(fixture.0.join("run/task-ca.pem")).unwrap();
    assert!(public_ca.contains("BEGIN CERTIFICATE"));
    assert!(!public_ca.contains("PRIVATE KEY"));
}

#[test]
fn untrusted_upstream_certificates_are_rejected_even_when_the_target_uses_insecure_tls() {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut connection = rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap();
        assert!(connection.complete_io(&mut stream).is_err());
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!("exec curl --noproxy '*' --resolve localhost:{port}:127.0.0.1 -k --max-time 10 -sS https://localhost:{port}/"));
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    server.join().unwrap();
    let events = fixture.events();
    let error = events
        .iter()
        .find(|e| e["kind"] == "network.tls.error")
        .expect("TLS failure must be recorded");
    assert!(
        error["data"]["error"]
            .as_str()
            .unwrap()
            .contains("UnknownIssuer"),
        "{error}"
    );
    assert!(events.iter().all(|e| e["kind"] != "network.http.request"));
}

#[test]
fn keep_alive_chunked_and_head_responses_are_separate_exchanges() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert!(read_headers(&mut stream).starts_with(b"GET /chunked HTTP/1.1"));
        for part in [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Final\r\n\r\n".as_slice(),
            b"3\r\nabc\r\n",
            b"4\r\ndefg\r\n0\r\nX-Final: yes\r\n\r\n",
        ] {
            stream.write_all(part).unwrap();
        }
        assert!(read_headers(&mut stream).starts_with(b"HEAD /empty HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let fixture = fixture();
    let script = format!(
        "import http.client\nc=http.client.HTTPConnection(\"127.0.0.1\",{port},timeout=8)\nc.request(\"GET\",\"/chunked\")\nr=c.getresponse(); assert r.read()==b\"abcdefg\"\nc.request(\"HEAD\",\"/empty\")\nr=c.getresponse(); assert r.read()==b\"\"\nc.close()\nprint(\"OK\")\n"
    );
    std::fs::write(fixture.0.join("client.py"), script).unwrap();
    let mut command = fixture.command("exec python3 client.py");
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let events = fixture.events();
    let requests: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.http.request")
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["data"]["connection_id"],
        requests[1]["data"]["connection_id"]
    );
    assert_ne!(
        requests[0]["data"]["request_id"],
        requests[1]["data"]["request_id"]
    );
    let bodies: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.http.response.body")
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["data"]["body"]["observed_bytes"], 7);
    assert_eq!(
        bodies[0]["data"]["trailers"],
        serde_json::json!([["x-final", "yes"]])
    );
    assert_eq!(bodies[1]["data"]["body"]["observed_bytes"], 0);
    assert_eq!(bodies[1]["data"]["body"]["status"], "complete");
    assert_eq!(
        std::fs::read(fixture.0.join("run/http-bodies.bin")).unwrap(),
        b"abcdefg"
    );
}

#[test]
fn upgrade_preserves_bidirectional_bytes_after_http_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert!(read_headers(&mut stream).starts_with(b"GET /upgrade HTTP/1.1"));
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: echo\r\n\r\nready").unwrap();
        let mut ping = [0; 4];
        stream.read_exact(&mut ping).unwrap();
        assert_eq!(&ping, b"ping");
        stream.write_all(b"pong").unwrap();
    });
    let fixture = fixture();
    std::fs::write(fixture.0.join("client.py"), format!(r#"import socket
s=socket.create_connection(("127.0.0.1",{port}),timeout=8)
s.sendall(b"GET /upgrade HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\nUpgrade: echo\r\n\r\n")
f=s.makefile("rb")
assert f.readline().startswith(b"HTTP/1.1 101")
while f.readline()!=b"\r\n": pass
assert f.read(5)==b"ready"
s.sendall(b"ping")
assert f.read(4)==b"pong"
s.close()
print("UPGRADE_OK")
"#)).unwrap();
    let mut command = fixture.command("exec python3 client.py");
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"UPGRADE_OK\n");
    server.join().unwrap();
    assert!(
        fixture
            .events()
            .iter()
            .any(|e| e["kind"] == "network.http.response" && e["data"]["status_code"] == 101)
    );
}

#[test]
fn ambiguous_non_http_prefaces_are_replayed_without_deadlock() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"G");
        stream.write_all(b"K").unwrap();
    });
    let fixture = fixture();
    std::fs::write(fixture.0.join("client.py"), format!("import socket\ns=socket.create_connection(('127.0.0.1',{port}),timeout=3)\ns.sendall(b'G')\nassert s.recv(1)==b'K'\ns.close()\n")).unwrap();
    let mut command = fixture.command("exec python3 client.py");
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let events = fixture.events();
    assert!(events.iter().all(|e| e["kind"] != "network.http.request"));
    assert!(events.iter().any(|e| e["kind"] == "network.transfer"
        && e["data"]["bytes_up"] == 1
        && e["data"]["bytes_down"] == 1));
}

#[test]
fn truncated_upstream_response_retains_an_explicit_incomplete_capture() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_headers(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort")
            .unwrap();
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 8 -sS http://{address}/"
    ));
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(!output.status.success());
    server.join().unwrap();
    let events = fixture.events();
    let body = &events
        .iter()
        .find(|e| e["kind"] == "network.http.response.body")
        .unwrap()["data"]["body"];
    assert_eq!(body["status"], "incomplete");
    assert_eq!(body["complete"], false);
    assert_eq!(body["observed_bytes"], 5);
    assert_eq!(
        std::fs::read(fixture.0.join("run/http-bodies.bin")).unwrap(),
        b"short"
    );
}
