#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
mod common;
use common::Fixture;

struct Echo {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Echo {
    fn new(ip: &str) -> Self {
        Self::from_listener(TcpListener::bind((ip, 0)).unwrap())
    }

    fn from_listener(listener: TcpListener) -> Self {
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut clients = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut client, _)) => clients.push(std::thread::spawn(move || {
                        client
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        client
                            .set_write_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let mut bytes = Vec::new();
                        client.read_to_end(&mut bytes).unwrap();
                        client.write_all(&bytes).unwrap();
                    })),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn file_open_records_preserve_paths_results_and_original_file_behavior() {
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run("exec ./fixture files");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"FILES_OK\n");
    let events = fixture.events();
    let pid = &events
        .iter()
        .find(|event| event["kind"] == "process.exec")
        .unwrap()["data"]["pid"];
    let opens: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "file.open")
        .map(|event| &event["data"])
        .collect();
    assert!(!opens.is_empty(), "no file events were captured");
    assert!(opens.iter().all(|event| event["pid"] == *pid));
    let relative = opens
        .iter()
        .find(|event| event["path"] == "relative.txt" && event["syscall"] == "openat")
        .unwrap();
    assert!(relative["result"].as_i64().unwrap() >= 0);
    assert!(
        relative["resolved_path"]
            .as_str()
            .unwrap()
            .ends_with("/files/relative.txt")
    );
    assert!(relative["base_path"].as_str().unwrap().ends_with("/files"));
    let openat2 = opens
        .iter()
        .find(|event| event["syscall"] == "openat2")
        .unwrap();
    assert_eq!(openat2["resolve"], 8);
    assert!(opens.iter().any(|event| event["syscall"] == "creat"));
    assert!(
        opens
            .iter()
            .any(|event| event["syscall"] == "open" && event["path"] == "legacy-created.txt")
    );
    assert!(
        opens
            .iter()
            .any(|event| event["path"] == "missing.txt" && event["errno"] == 2)
    );
    assert!(
        opens
            .iter()
            .any(|event| event["path_capture"] == "unreadable" && event["errno"] == 14)
    );
    assert!(
        opens
            .iter()
            .any(|event| event["path_bytes"] == serde_json::json!([114, 97, 119, 45, 255]))
    );
    assert_eq!(
        std::fs::read(fixture.0.join("files/relative.txt")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn tcp_uses_the_builtin_proxy_with_nonblocking_dup_fork_and_shared_readonly_addresses() {
    for mode in [
        "tcp4",
        "peer-ip",
        "nonblocking",
        "dup",
        "inherited",
        "threads",
    ] {
        let server = Echo::new(if mode == "peer-ip" {
            "127.0.0.2"
        } else {
            "127.0.0.1"
        });
        let fixture = Fixture::compile("syscalls");
        let output = fixture.run(&format!("exec ./fixture {mode} {}", server.address.port()));
        assert!(
            output.status.success(),
            "{mode}: {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let events = fixture.events();
        let root_pid = &events
            .iter()
            .find(|event| event["kind"] == "process.exec")
            .unwrap()["data"]["pid"];
        let connections: Vec<_> = events
            .iter()
            .filter(|event| event["kind"] == "network.upstream")
            .collect();
        assert_eq!(connections.len(), if mode == "threads" { 4 } else { 1 });
        for connection in connections {
            let expected_pid = if mode == "inherited" {
                &events
                    .iter()
                    .find(|event| event["kind"] == "process.fork")
                    .unwrap()["data"]["child_pid"]
            } else {
                root_pid
            };
            assert_eq!(&connection["data"]["pid"], expected_pid);
            assert_eq!(connection["source"], "tcp_proxy");
            assert_eq!(connection["data"]["transport"], "tcp");
            assert!(connection["data"]["source_address"].as_str().is_some());
            assert_eq!(
                connection["data"]["destination"],
                server.address.to_string()
            );
            let id = &connection["data"]["connection_id"];
            let transfer = events
                .iter()
                .find(|event| {
                    event["kind"] == "network.transfer" && event["data"]["connection_id"] == *id
                })
                .unwrap();
            assert_eq!(transfer["source"], "tcp_proxy");
            assert!(connection["data"]["process_instance_id"].as_u64().is_some());
            assert_eq!(
                transfer["data"]["process_instance_id"],
                connection["data"]["process_instance_id"]
            );
            assert_eq!(&transfer["data"]["pid"], expected_pid);
            assert_eq!(transfer["data"]["bytes_up"], 65536);
            assert_eq!(transfer["data"]["bytes_down"], 65536);
        }
    }
}

#[test]
fn http_host_and_tls_sni_are_observed_without_changing_forwarded_bytes() {
    let cases: &[(&[u8], &str, Option<&str>)] = &[
        (
            b"POST / HTTP/1.1\r\nhOsT: Http.Example.Test:8080\r\nContent-Length: 4\r\n\r\nbody",
            "http",
            Some("http.example.test"),
        ),
        (
            include_bytes!("fixtures/client-hello-sni.bin"),
            "tls",
            Some("tls.example.test"),
        ),
        (
            include_bytes!("fixtures/client-hello-no-sni.bin"),
            "tls",
            None,
        ),
    ];
    for (payload, protocol, domain) in cases {
        let server = Echo::new("127.0.0.1");
        let fixture = Fixture::compile("syscalls");
        std::fs::write(fixture.0.join("payload.bin"), payload).unwrap();
        let output = fixture.run(&format!(
            "exec ./fixture protocol {}",
            server.address.port()
        ));
        assert!(
            output.status.success(),
            "{:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"TCP_OK\n", "echo must preserve every byte");
        let events = fixture.events();
        let names: Vec<_> = events
            .iter()
            .filter(|event| event["kind"] == "network.protocol")
            .collect();
        assert_eq!(names.len(), 1, "missing or duplicate protocol observation");
        let data = &names[0]["data"];
        assert_eq!(data["protocol"], *protocol);
        assert_eq!(data["domain"].as_str(), *domain);
        assert_eq!(
            data["domain_source"],
            if *protocol == "tls" {
                "tls_sni"
            } else {
                "http_host"
            }
        );
        let connect = events
            .iter()
            .find(|e| e["kind"] == "network.upstream")
            .unwrap();
        for field in ["connection_id", "pid", "process_instance_id", "destination"] {
            assert_eq!(data[field], connect["data"][field]);
        }
        let transfer = events
            .iter()
            .find(|event| event["kind"] == "network.transfer")
            .unwrap();
        assert_eq!(transfer["data"]["bytes_up"], payload.len());
        assert_eq!(transfer["data"]["bytes_down"], payload.len());
    }
}

#[test]
fn invalid_and_repeated_connect_calls_preserve_errors_and_release_unused_routes() {
    let server = Echo::new("127.0.0.1");
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture invalid {}", server.address.port()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fixture.events();
    // The C fixture asserts EBADF/EFAULT/EINVAL/EISCONN. Only the single
    // accepted upstream connection is exported, rather than failed syscalls.
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "network.upstream")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .filter(|event| event["kind"].as_str().unwrap().starts_with("network."))
            .all(|event| event["source"] == "tcp_proxy")
    );
}

#[test]
fn denied_seccomp_installation_fails_before_running_user_code() {
    use nix::libc;
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::compile("syscalls");
    let mut command = fixture.command("printf unsafe > should-not-run");
    unsafe {
        command.pre_exec(|| {
            let statement = |code, k| libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            };
            let jump = |k, jf| libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf,
                k,
            };
            let mut filter = [
                statement(0x20, 0),
                jump(libc::SYS_prctl as u32, 3),
                statement(0x20, 16),
                jump(libc::PR_SET_SECCOMP as u32, 1),
                statement(0x06, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
                statement(0x06, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot install syscall filter"));
    assert!(!fixture.0.join("should-not-run").exists());
    let status: Value = serde_json::from_slice(
        &std::fs::read(fixture.0.join("run/collection-status.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(status["state"], "failed");
}

#[test]
fn ipv6_tcp_uses_the_proxy_when_ipv6_loopback_is_available() {
    let listener = match TcpListener::bind(("::1", 0)) {
        Ok(listener) => listener,
        Err(error) if matches!(error.raw_os_error(), Some(97 | 99)) => {
            eprintln!("IPv6 integration unavailable on this host: {error}");
            return;
        }
        Err(error) => panic!("IPv6 bind: {error}"),
    };
    let server = Echo::from_listener(listener);
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture tcp6 {}", server.address.port()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture
            .events()
            .iter()
            .any(|event| event["kind"] == "network.transfer"
                && event["data"]["bytes_up"] == 65536
                && event["data"]["bytes_down"] == 65536)
    );
}

#[test]
fn interrupted_and_restarted_file_opens_do_not_desynchronize_collection() {
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run("exec ./fixture signals");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SIGNALS_OK\n");
    let events = fixture.events();
    assert!(events.iter().any(|event| event["kind"] == "file.open"
        && event["data"]["path"] == "fifo"
        && event["data"]["restart"] == true));
    assert!(events.iter().any(|event| event["kind"] == "file.open"
        && event["data"]["path"] == "after-signal.txt"
        && event["data"]["result"].as_i64().is_some_and(|fd| fd >= 0)));
}

#[test]
fn queued_upload_is_delivered_even_when_all_tracees_exit_without_reading_a_reply() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture upload-exit {port}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fixture.events();
    let bytes = server.join().unwrap();
    assert_eq!(bytes.len(), 65536);
    assert!(
        bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == index as u8)
    );
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.transfer" && event["data"]["bytes_up"] == 65536)
    );
}

#[test]
fn upstream_failure_is_recorded_separately_from_the_local_connect_result() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture refused {port}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"UPSTREAM_CLOSED\n");
    let events = fixture.events();
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.upstream" && event["data"]["status"] == "failed")
    );
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.upstream" && event["data"]["errno"] == 111)
    );
}

#[test]
fn child_file_and_proxy_activity_continue_after_the_foreground_returns() {
    let server = Echo::new("127.0.0.1");
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!(
        "exec ./fixture background {}",
        server.address.port()
    ));
    assert_eq!(output.status.code(), Some(23));
    assert!(output.stdout.is_empty());
    let events = fixture.events();
    let root_exit = events
        .iter()
        .position(|event| event["kind"] == "process.exit" && event["data"]["exit_code"] == 23)
        .unwrap();
    assert!(
        events[root_exit + 1..]
            .iter()
            .any(|event| event["kind"] == "file.open" && event["data"]["path"] == "late-child.txt")
    );
    assert!(
        events[root_exit + 1..]
            .iter()
            .any(|event| event["kind"] == "network.transfer" && event["data"]["bytes_up"] == 65536)
    );
    assert!(
        std::fs::read_to_string(fixture.0.join("run/stdout.log"))
            .unwrap()
            .contains("TCP_OK")
    );
}

#[test]
fn connected_udp_payload_is_observed_by_its_own_proxy() {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let thread = std::thread::spawn(move || {
        let mut bytes = [0; 16];
        let (count, peer) = socket.recv_from(&mut bytes).unwrap();
        socket.send_to(&bytes[..count], peer).unwrap();
    });
    let fixture = Fixture::compile("syscalls");
    let output = fixture.run(&format!("exec ./fixture udp {port}"));
    thread.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"UDP_OK\n");
    assert!(
        fixture
            .events()
            .iter()
            .any(|event| event["kind"] == "network.udp.send"
                && event["source"] == "udp_proxy"
                && event["data"]["transport"] == "udp")
    );
}
