#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[allow(dead_code)]
mod common;
use common::Fixture;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!("agenttomo-streams-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    Fixture(dir)
}

fn headers(stream: &mut impl Read) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536);
    }
}

#[test]
fn sse_records_messages_without_changing_the_forwarded_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = b"id: 1\nevent: token\ndata: hello\n\ndata: world\n\n";
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        headers(&mut socket);
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        for chunk in body.chunks(3) {
            socket.write_all(chunk).unwrap();
        }
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 10 -fsSN http://{address}/events"
    ));
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, body);
    server.join().unwrap();
    let events = fixture.events();
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "network.stream.open" && e["data"]["protocol"] == "sse"),
        "missing SSE lifecycle"
    );
    assert!(fixture.0.join("run/stream-messages.sqlite3").exists());
    let db = rusqlite::Connection::open(fixture.0.join("run/stream-messages.sqlite3")).unwrap();
    let messages: Vec<Vec<u8>> = db
        .prepare("SELECT payload FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(messages, [b"hello".to_vec(), b"world".to_vec()]);
    assert_eq!(
        std::fs::metadata(fixture.0.join("run/http-bodies.bin"))
            .unwrap()
            .len(),
        0,
        "SSE must not retain a second non-expiring body prefix"
    );
}

#[test]
fn sse_is_available_before_eof_and_old_messages_are_removed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        headers(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        socket
            .write_all(b"data: first\n\ndata: second\n\ndata: third\n\n")
            .unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(10));
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 12 -fsSN http://{address}/events"
    ));
    command.args(["--capture-http", "--stream-window-messages", "2"]);
    let child = std::thread::spawn(move || Fixture::wait(command));
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut live = false;
    while std::time::Instant::now() < deadline {
        if let Ok(db) = rusqlite::Connection::open_with_flags(
            fixture.0.join("run/stream-messages.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) && let Ok(received) =
            db.query_row("SELECT received FROM state", [], |row| row.get::<_, u64>(0))
            && received == 3
        {
            live = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    release.send(()).unwrap();
    let output = child.join().unwrap();
    server.join().unwrap();
    assert!(
        live,
        "messages were not stored while the response remained open"
    );
    assert!(output.status.success());
    fixture.events();
    let db = rusqlite::Connection::open(fixture.0.join("run/stream-messages.sqlite3")).unwrap();
    let messages: Vec<Vec<u8>> = db
        .prepare("SELECT payload FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(messages, [b"second".to_vec(), b"third".to_vec()]);
}

#[test]
fn sse_message_start_time_does_not_include_idle_time_after_the_previous_event() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        headers(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        socket.write_all(b"data: second\r\n\r\n").unwrap();
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 10 -fsSN http://{address}/events"
    ));
    command.arg("--capture-http");
    assert!(Fixture::wait(command).status.success());
    server.join().unwrap();
    fixture.events();
    let messages = messages(&fixture);
    assert_eq!(messages.len(), 2);
    assert!(
        messages[1].0["started_unix_ns"].as_u64().unwrap()
            - messages[0].0["started_unix_ns"].as_u64().unwrap()
            >= 150_000_000
    );
}

#[test]
fn oversized_sse_blocks_are_reported_and_later_events_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut body = b"data: ".to_vec();
    body.extend(std::iter::repeat_n(b'x', 1024 * 1024 + 1));
    body.extend_from_slice(b"\n\ndata: recovered\n\n");
    let length = body.len();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        headers(&mut socket);
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
        socket.write_all(&body).unwrap();
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --max-time 10 -fsSN -o received http://{address}/events"
    ));
    command.arg("--capture-http");
    assert!(Fixture::wait(command).status.success());
    server.join().unwrap();
    fixture.events();
    assert_eq!(
        std::fs::metadata(fixture.0.join("received")).unwrap().len(),
        length as u64
    );
    let messages = messages(&fixture);
    assert!(
        messages
            .iter()
            .any(|(data, _)| data["kind"] == "inspection_error")
    );
    assert!(
        messages
            .iter()
            .any(|(data, bytes)| data["kind"] == "event" && bytes == b"recovered")
    );
}

#[test]
fn websocket_records_both_directions_without_changing_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        headers(&mut socket);
        socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n\x81\x05hello").unwrap();
        let mut frame = [0; 11];
        socket.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"\x81\x85\x01\x02\x03\x04vmqhe");
        socket
            .write_all(b"\x82\x02\x00\xff\x89\x01?\x88\x02\x03\xe8")
            .unwrap();
    });
    let fixture = fixture();
    std::fs::write(fixture.0.join("client.py"), format!(r#"import socket
s=socket.create_connection(("127.0.0.1",{port}),timeout=8)
s.sendall(b"GET /socket HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
f=s.makefile("rb")
assert f.readline().startswith(b"HTTP/1.1 101")
while f.readline()!=b"\r\n": pass
assert f.read(7)==b"\x81\x05hello"
s.sendall(b"\x81\x85\x01\x02\x03\x04vmqhe")
assert f.read()==b"\x82\x02\x00\xff\x89\x01?\x88\x02\x03\xe8"
s.close()
print("WS_OK")
"#)).unwrap();
    let mut command = fixture.command("exec python3 client.py");
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"WS_OK\n");
    assert!(
        fixture
            .events()
            .iter()
            .any(|e| e["kind"] == "network.stream.open" && e["data"]["protocol"] == "websocket"),
        "missing WebSocket lifecycle"
    );
    let messages = messages(&fixture);
    for (direction, kind, payload) in [
        ("downstream", "text", &b"hello"[..]),
        ("upstream", "text", &b"world"[..]),
        ("downstream", "binary", &b"\0\xff"[..]),
        ("downstream", "ping", &b"?"[..]),
        ("downstream", "close", &b"\x03\xe8"[..]),
    ] {
        assert!(
            messages
                .iter()
                .any(|(data, bytes)| data["direction"] == direction
                    && data["kind"] == kind
                    && bytes == payload),
            "missing {direction} {kind}: {messages:?}"
        );
    }
}

fn messages(fixture: &Fixture) -> Vec<(serde_json::Value, Vec<u8>)> {
    let db = rusqlite::Connection::open(fixture.0.join("run/stream-messages.sqlite3")).unwrap();
    db.prepare("SELECT data,payload FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            let text: String = row.get(0)?;
            Ok((serde_json::from_str(&text).unwrap(), row.get(1)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn ws_server_bytes(bytes: Vec<u8>, extensions: &str, limit: u32) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expected = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let extensions = extensions.to_owned();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        headers(&mut socket);
        write!(socket,"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Extensions: {extensions}\r\n\r\n").unwrap();
        for chunk in bytes.chunks(7) {
            socket.write_all(chunk).unwrap();
        }
    });
    let fixture = fixture();
    std::fs::write(fixture.0.join("client.py"), format!(r#"import socket
s=socket.create_connection(("127.0.0.1",{port}),timeout=8)
s.sendall(b"GET /socket HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n")
f=s.makefile("rb")
assert f.readline().startswith(b"HTTP/1.1 101")
while f.readline()!=b"\r\n": pass
assert f.read()==bytes.fromhex("{expected}")
s.close()
"#)).unwrap();
    let mut command = fixture.command("exec python3 client.py");
    command.args([
        "--capture-http",
        "--stream-message-max-bytes",
        &limit.to_string(),
    ]);
    let output = Fixture::wait(command);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.events();
    fixture
}

#[test]
fn websocket_fragments_control_and_compression_context_are_observed() {
    use flate2::{Compress, Compression, FlushCompress};
    let mut compressor = Compress::new(Compression::fast(), false);
    let texts = [
        "compressed-message-中文".repeat(10),
        "compressed-message-中文".repeat(20),
    ];
    let mut wire = Vec::new();
    for (index, text) in texts.iter().enumerate() {
        let mut compressed = [0; 4096];
        let before = compressor.total_out();
        compressor
            .compress(text.as_bytes(), &mut compressed, FlushCompress::Sync)
            .unwrap();
        let count = (compressor.total_out() - before) as usize - 4;
        assert!(count < 125);
        if index == 0 {
            wire.extend_from_slice(&[0x41, (count / 2) as u8]);
            wire.extend_from_slice(&compressed[..count / 2]);
            wire.extend_from_slice(b"\x89\x01?");
            wire.extend_from_slice(&[0x80, (count - count / 2) as u8]);
            wire.extend_from_slice(&compressed[count / 2..count]);
        } else {
            wire.extend_from_slice(&[0xc1, count as u8]);
            wire.extend_from_slice(&compressed[..count]);
        }
    }
    let fixture = ws_server_bytes(wire, "permessage-deflate", 65536);
    let messages = messages(&fixture);
    let data: Vec<_> = messages
        .iter()
        .filter(|(data, _)| data["kind"] == "text")
        .collect();
    assert_eq!(data.len(), 2, "{messages:?}");
    for (index, (metadata, payload)) in data.into_iter().enumerate() {
        assert_eq!(payload, texts[index].as_bytes());
        assert_eq!(metadata["decoded"], true);
        assert_eq!(metadata["frames"], if index == 0 { 2 } else { 1 });
    }
    assert!(
        messages
            .iter()
            .any(|(data, bytes)| data["kind"] == "ping" && bytes == b"?")
    );
}

#[test]
fn large_websocket_frames_truncate_only_retention_and_resume_at_the_next_frame() {
    let mut bytes = vec![0x82, 127];
    bytes.extend_from_slice(&100_000_u64.to_be_bytes());
    bytes.extend(std::iter::repeat_n(b'x', 100_000));
    bytes.extend_from_slice(b"\x81\x04next");
    let fixture = ws_server_bytes(bytes, "", 16);
    let messages = messages(&fixture);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].1, b"xxxxxxxxxxxxxxxx");
    assert_eq!(messages[0].0["truncated"], true);
    assert_eq!(messages[0].0["observed_bytes"], 100_000);
    assert_eq!(messages[1].1, b"next");
}

#[test]
fn unknown_websocket_extensions_are_not_misrepresented_as_decoded_text() {
    let fixture = ws_server_bytes(b"\xa1\x03abc".to_vec(), "x-custom", 16);
    let messages = messages(&fixture);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0["decoded"], false);
    assert_eq!(messages[0].1, b"abc");
}

#[test]
fn websocket_eof_retains_an_incomplete_message_without_inventing_a_close_frame() {
    let fixture = ws_server_bytes(b"\x01\x03abc\x80\x06par".to_vec(), "", 16);
    let messages = messages(&fixture);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0["complete"], false);
    assert_eq!(messages[0].1, b"abcpar");
}

#[test]
fn gzip_sse_handles_bom_utf8_multiline_and_crlf_boundaries() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let content = "\u{feff}: heartbeat\r\nid: 你好\r\nevent: token\r\ndata: 你好\r\ndata: world\r\n\r\ndata: final\r\n\r\n";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(content.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        headers(&mut socket);
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).unwrap();
        for chunk in compressed.chunks(1) {
            socket.write_all(chunk).unwrap();
        }
    });
    let fixture = fixture();
    let mut command = fixture.command(&format!(
        "exec curl --noproxy '*' --compressed --max-time 10 -fsSN http://{address}/events"
    ));
    command.arg("--capture-http");
    let output = Fixture::wait(command);
    assert!(output.status.success());
    assert_eq!(output.stdout, content.as_bytes());
    server.join().unwrap();
    fixture.events();
    let messages = messages(&fixture);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0].1, "你好\nworld".as_bytes());
    assert_eq!(messages[0].0["event"], "token");
    assert_eq!(messages[1].0["event_id"], "你好");
    assert_eq!(messages[1].1, b"final");
}

#[test]
fn https_sse_and_wss_use_the_existing_task_ca_path() {
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
    let config = std::sync::Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(config.clone()).unwrap(),
                stream,
            );
            headers(&mut stream);
            if index == 0 {
                let body = b"data: encrypted-sse\n\n";
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(body).unwrap();
            } else {
                stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n\x81\x09secure-ws\x88\x02\x03\xe8").unwrap();
            }
            stream.conn.send_close_notify();
            stream.flush().unwrap();
        }
    });
    std::fs::write(fixture.0.join("client.py"), format!(r#"import socket,ssl
ctx=ssl.create_default_context()
for upgrade in [False,True]:
    s=ctx.wrap_socket(socket.create_connection(("127.0.0.1",{port}),timeout=8),server_hostname="localhost")
    extra=b"Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n" if upgrade else b"Connection: close\r\n"
    s.sendall(b"GET /secure HTTP/1.1\r\nHost: localhost\r\n"+extra+b"\r\n")
    f=s.makefile("rb")
    assert f.readline().startswith(b"HTTP/1.1 101" if upgrade else b"HTTP/1.1 200")
    while f.readline()!=b"\r\n": pass
    assert f.read()==(b"\x81\x09secure-ws\x88\x02\x03\xe8" if upgrade else b"data: encrypted-sse\n\n")
    f.close();s.close()
"#)).unwrap();
    let mut command = fixture.command("exec env -u SSL_CERT_FILE python3 client.py");
    command.env("SSL_CERT_FILE", trust).arg("--capture-http");
    let output = Fixture::wait(command);
    let server_result = server.join();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server_result.unwrap();
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "network.protocol"
                && event["data"]["tls_intercepted"] == true)
            .count(),
        2
    );
    let messages = messages(&fixture);
    assert!(
        messages
            .iter()
            .any(|(data, bytes)| data["protocol"] == "sse" && bytes == b"encrypted-sse")
    );
    assert!(
        messages
            .iter()
            .any(|(data, bytes)| data["protocol"] == "websocket" && bytes == b"secure-ws")
    );
}
