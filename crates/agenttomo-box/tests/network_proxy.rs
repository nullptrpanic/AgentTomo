#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;
use common::Fixture;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

#[test]
fn box_proxy_forwards_opaque_tcp_and_preserves_the_application_peer() {
    let directory = std::env::temp_dir().join(format!("agenttomo-tunnel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let fixture = Fixture(directory);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() < 4096);
        }
        assert!(headers.starts_with(b"CONNECT 192.0.2.42:2345 HTTP/1.1\r\n"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\n\r\n\x00READY\xff")
            .unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, (0..=255).collect::<Vec<u8>>());
        socket.write_all(&bytes).unwrap();
    });
    std::fs::write(
        fixture.0.join("probe.py"),
        r#"
import socket
s = socket.create_connection(('192.0.2.42', 2345), timeout=5)
assert s.getpeername() == ('192.0.2.42', 2345)
f = s.makefile('rb')
assert f.read(7) == b'\x00READY\xff'
s.sendall(bytes(range(256)))
s.shutdown(socket.SHUT_WR)
assert f.read() == bytes(range(256))
print('OPAQUE_TCP_OK')
"#,
    )
    .unwrap();
    let mut command = fixture.command("exec python3 probe.py");
    command.args(["--proxy", &format!("http://{address}")]);
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"OPAQUE_TCP_OK\n");
    server.join().unwrap();
    let events = fixture.events();
    let flows: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "network.transfer")
        .collect();
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0]["data"]["destination"], "192.0.2.42:2345");
    assert_eq!(flows[0]["data"]["bytes_up"], 256);
    assert_eq!(flows[0]["data"]["bytes_down"], 263);
    assert!(flows[0]["data"]["pid"].is_number());
    assert!(
        !events
            .iter()
            .any(|event| event["kind"].as_str().unwrap().starts_with("network.http"))
    );
}

fn check_ipv6_route_availability(with_route: bool) {
    let fixture = Fixture::compile("transparent");
    let setup = if with_route {
        "ip link set lo up; ip -6 route add 2001:db8::/32 dev lo;"
    } else {
        "ip link set lo up;"
    };
    let program = format!(
        "python3 -c 'import socket,errno\ns=socket.socket(socket.AF_INET6,socket.SOCK_DGRAM)\ntry:\n s.connect((\"2001:db8::1\",443)); result=0\nexcept OSError as e:\n result=e.errno\nassert result=={}, result\ns.close()\ns=socket.socket(socket.AF_INET6,socket.SOCK_DGRAM); s.connect((\"::1\",9)); print(\"IPV6_ROUTE_OK\")'",
        if with_route {
            0
        } else {
            nix::libc::ENETUNREACH
        }
    );
    let mut command = std::process::Command::new("unshare");
    command.args(["--user", "--map-root-user", "--net", "--", "/bin/sh", "-c"])
        .arg(format!("{setup} exec \"$AGENTTOMO_TEST_BIN\" run --output-dir \"$AGENTTOMO_TEST_DIR/run\" -c \"$AGENTTOMO_TEST_COMMAND\""))
        .env("AGENTTOMO_TEST_BIN", env!("CARGO_BIN_EXE_agenttomo-box"))
        .env("AGENTTOMO_TEST_DIR", &fixture.0)
        .env("AGENTTOMO_TEST_COMMAND", program)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = Fixture::wait(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"IPV6_ROUTE_OK\n");
    fixture.events();
}

#[test]
fn missing_outer_ipv6_routes_do_not_become_reachable_inside_the_scope() {
    check_ipv6_route_availability(false);
}

#[test]
fn outer_ipv6_routes_keep_ipv6_enabled_inside_the_scope() {
    check_ipv6_route_availability(true);
}

fn echo(listener: TcpListener, count: usize) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            socket.write_all(&byte).unwrap();
        }
    })
}

#[test]
fn services_started_inside_the_scope_keep_local_tcp_and_udp_communication() {
    let fixture = Fixture::compile("transparent");
    let output = fixture.run("exec ./fixture local 0");
    assert!(
        output.status.success(),
        "{:?} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"LOCAL_OK\n");
    assert!(
        fixture
            .events()
            .iter()
            .all(|e| !e["kind"].as_str().unwrap().starts_with("network.")),
        "local traffic must not be reported as an external proxy transfer"
    );
}

#[test]
fn identical_source_ports_to_different_destinations_do_not_collide() {
    let first = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = first.local_addr().unwrap().port();
    let second = TcpListener::bind(("127.0.0.3", port)).unwrap();
    let servers = [echo(first, 1), echo(second, 1)];
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture reuse {port}"));
    assert!(
        output.status.success(),
        "{:?} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"REUSE_OK\n");
    for server in servers {
        server.join().unwrap();
    }
    let events = fixture.events();
    let flows: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.transfer")
        .collect();
    assert_eq!(flows.len(), 2);
    assert_eq!(
        flows[0]["data"]["source_address"],
        flows[1]["data"]["source_address"]
    );
    assert_ne!(
        flows[0]["data"]["destination"],
        flows[1]["data"]["destination"]
    );
    assert_ne!(flows[0]["data"]["socket_id"], flows[1]["data"]["socket_id"]);
    assert!(flows.iter().all(|f| f["data"]["pid"].as_i64().is_some()));
}

#[test]
fn socket_queries_preserve_timer_signal_delivery_and_syscall_results() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = echo(listener, 100);
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture signals {port}"));
    assert!(
        output.status.success(),
        "{:?} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SIGNALS_OK\n");
    server.join().unwrap();
    let events = fixture.events();
    let flows: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.transfer")
        .collect();
    assert_eq!(flows.len(), 100);
    assert!(
        flows
            .iter()
            .all(|f| f["data"]["pid"].as_i64().is_some() && f["data"]["bytes_up"] == 1)
    );
}

#[test]
fn maximum_ipv4_udp_payload_preserves_bytes_and_reply_source() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut bytes = vec![0; 65536];
        let (size, source) = socket.recv_from(&mut bytes).unwrap();
        assert_eq!(size, 65507);
        socket.send_to(&bytes[..size], source).unwrap();
    });
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture udpmax {port}"));
    assert!(
        output.status.success(),
        "{:?} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let events = fixture.events();
    let summary = events
        .iter()
        .find(|e| e["kind"] == "network.udp.transfer")
        .unwrap();
    assert_eq!(summary["data"]["bytes_up"], 65507);
    assert_eq!(summary["data"]["bytes_down"], 65507);
    assert!(summary["data"]["pid"].as_i64().is_some());
}

#[test]
fn maximum_ipv6_udp_payload_preserves_bytes_and_reply_source() {
    let socket = match std::net::UdpSocket::bind("[::1]:0") {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("IPv6 unavailable: {error}");
            return;
        }
    };
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut bytes = vec![0; 65536];
        let (size, source) = socket.recv_from(&mut bytes).unwrap();
        assert_eq!(size, 65527);
        socket.send_to(&bytes[..size], source).unwrap();
    });
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture udp6max {port}"));
    assert!(
        output.status.success(),
        "{:?} {} {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        fixture.events()
    );
    server.join().unwrap();
    let events = fixture.events();
    let summary = events
        .iter()
        .find(|e| e["kind"] == "network.udp.transfer")
        .unwrap();
    assert_eq!(summary["data"]["bytes_up"], 65527);
    assert_eq!(summary["data"]["bytes_down"], 65527);
    assert!(summary["data"]["pid"].as_i64().is_some());
}

#[test]
fn queued_udp_and_zero_datagrams_are_drained_after_every_tracee_exits() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture udp-exit {port}"));
    assert!(output.status.success());
    let events = fixture.events();
    let mut bytes = [0; 16];
    assert_eq!(socket.recv(&mut bytes).unwrap(), 6);
    assert_eq!(&bytes[..6], b"queued");
    assert_eq!(socket.recv(&mut bytes).unwrap(), 0);
    let summary = events
        .iter()
        .find(|e| e["kind"] == "network.udp.transfer")
        .unwrap();
    assert_eq!(summary["data"]["packets_up"], 2);
    assert_eq!(summary["data"]["bytes_up"], 6);
    let first_send = events
        .iter()
        .find(|e| e["kind"] == "network.udp.send")
        .unwrap();
    assert_eq!(summary["data"]["start_source"], "proxy_first_datagram");
    assert!(
        summary["data"]["started_unix_ns"].as_u64().unwrap()
            <= first_send["observed_unix_ns"].as_u64().unwrap()
    );
    assert!(
        summary["data"]["completed_unix_ns"].as_u64().unwrap()
            <= summary["observed_unix_ns"].as_u64().unwrap()
    );
    assert!(summary["data"]["duration_ns"].as_u64().unwrap() > 0);
}

#[test]
fn udp_sockets_sharing_a_five_tuple_do_not_claim_one_process_owner() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let fixture = Fixture::compile("transparent");
    let output = fixture.run(&format!("exec ./fixture udp-shared-tuple {port}"));
    assert!(output.status.success());
    let events = fixture.events();
    let mut bytes = [0; 16];
    assert_eq!(socket.recv(&mut bytes).unwrap(), 5);
    assert_eq!(socket.recv(&mut bytes).unwrap(), 6);
    let summaries: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.udp.transfer")
        .collect();
    assert_eq!(summaries.len(), 1);
    let data = &summaries[0]["data"];
    assert_eq!(data["packets_up"], 2);
    assert_eq!(data["socket_ids"].as_array().unwrap().len(), 2);
    assert_eq!(data["participants"].as_array().unwrap().len(), 2);
    assert!(data["pid"].is_null());
    assert!(data["process_instance_id"].is_null());
    assert!(data["socket_id"].is_null());
}

#[test]
fn denied_namespace_creation_fails_without_running_the_command() {
    use nix::libc;
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::compile("transparent");
    let mut command = fixture.command("printf unsafe > should-not-run");
    unsafe {
        command.pre_exec(|| {
            let mut filters = [
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 1,
                    k: libc::SYS_unshare as u32,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ALLOW,
                },
            ];
            let program = libc::sock_fprog {
                len: filters.len() as u16,
                filter: filters.as_mut_ptr(),
            };
            nix::errno::Errno::result(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            nix::errno::Errno::result(libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program,
            ))?;
            Ok(())
        });
    }
    let output = Fixture::wait(command);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cannot create private user/network namespace")
    );
    assert!(!fixture.0.join("should-not-run").exists());
}

#[test]
fn network_records_come_only_from_the_proxy_and_include_original_endpoints() {
    let listener = TcpListener::bind("127.0.0.2:0").unwrap();
    let destination = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        stream.write_all(&bytes).unwrap();
    });
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture peer-ip {}", destination.port()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let events = fixture.events();
    for event in &events {
        assert_eq!(
            event["category"].as_str(),
            event["kind"].as_str().unwrap().split('.').next()
        );
    }
    let network: Vec<_> = events
        .iter()
        .filter(|e| e["kind"].as_str().unwrap().starts_with("network."))
        .collect();
    assert!(!network.is_empty());
    assert!(
        network.iter().all(|e| e["source"] == "tcp_proxy"),
        "duplicate syscall observations: {network:?}"
    );
    let flow = network
        .iter()
        .find(|e| e["kind"] == "network.transfer")
        .unwrap();
    assert_eq!(flow["data"]["destination"], destination.to_string());
    assert!(
        flow["data"]["source_address"]
            .as_str()
            .is_some_and(|s| s.starts_with("127.0.0.1:"))
    );
    assert!(flow["data"]["pid"].as_i64().is_some());
    assert_eq!(flow["data"]["bytes_up"], 65536);
    assert_eq!(flow["data"]["bytes_down"], 65536);
    let upstream = network
        .iter()
        .find(|e| e["kind"] == "network.upstream")
        .unwrap();
    assert_eq!(flow["data"]["start_source"], "proxy_accept");
    let start = flow["data"]["started_unix_ns"].as_u64().unwrap();
    let end = flow["data"]["completed_unix_ns"].as_u64().unwrap();
    assert!(start <= upstream["observed_unix_ns"].as_u64().unwrap());
    assert!(end >= start && end <= flow["observed_unix_ns"].as_u64().unwrap());
    assert!(flow["data"]["duration_ns"].as_u64().unwrap() > 0);
}
