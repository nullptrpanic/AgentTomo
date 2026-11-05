#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

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

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("agenttomo-collection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let output = Command::new("cc")
            .args(["-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/syscalls.c"))
            .arg("-o")
            .arg(path.join("fixture"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(path)
    }

    fn run(&self, command: &str) -> Output {
        Self::wait(self.command(command))
    }

    fn command(&self, command: &str) -> Command {
        let mut process = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"));
        process
            .args(["run", "--output-dir", "run", "-c", command])
            .current_dir(&self.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        process
    }

    fn wait(mut command: Command) -> Output {
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("command did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }

    fn events(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = std::fs::read(self.0.join("run/collection-status.json"))
                .ok()
                .and_then(|data| serde_json::from_slice::<Value>(&data).ok());
            if let Some(status) = status {
                assert_ne!(status["state"], "failed", "{status}");
                if status["state"] == "complete" {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "collection did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read_to_string(self.0.join("run/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn file_open_records_preserve_paths_results_and_original_file_behavior() {
    let fixture = Fixture::new();
    let output = fixture.run("exec ./fixture files");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"FILES_OK\n");
    let events = fixture.events();
    let opens: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "file.open")
        .map(|event| &event["data"])
        .collect();
    assert!(!opens.is_empty(), "no file events were captured");
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
    for mode in ["tcp4", "nonblocking", "dup", "inherited", "threads"] {
        let server = Echo::new("127.0.0.1");
        let fixture = Fixture::new();
        let output = fixture.run(&format!("exec ./fixture {mode} {}", server.address.port()));
        assert!(
            output.status.success(),
            "{mode}: {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let events = fixture.events();
        let connections: Vec<_> = events
            .iter()
            .filter(|event| event["kind"] == "network.connect")
            .collect();
        assert_eq!(connections.len(), if mode == "threads" { 4 } else { 1 });
        for connection in connections {
            assert_eq!(connection["data"]["redirected"], true);
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
            assert_eq!(transfer["data"]["task_id"], connection["data"]["task_id"]);
            assert_eq!(transfer["data"]["bytes_up"], 65536);
            assert_eq!(transfer["data"]["bytes_down"], 65536);
        }
    }
}

#[test]
fn invalid_and_repeated_connect_calls_preserve_errors_and_release_unused_routes() {
    let server = Echo::new("127.0.0.1");
    let fixture = Fixture::new();
    let output = fixture.run(&format!("exec ./fixture invalid {}", server.address.port()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fixture.events();
    for errno in [9, 14, 22, 106] {
        assert!(
            events
                .iter()
                .any(|event| event["kind"] == "network.connect" && event["data"]["errno"] == errno),
            "missing errno {errno}"
        );
    }
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.transfer"
                && event["data"]["status"] == "cancelled")
    );
}

#[test]
fn denied_seccomp_installation_fails_before_running_user_code() {
    use nix::libc;
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
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
            .any(|event| event["kind"] == "network.connect" && event["data"]["result"] == 0)
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
    let fixture = Fixture::new();
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
fn udp_connect_is_observed_without_tcp_redirection() {
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
    let fixture = Fixture::new();
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
            .any(|event| event["kind"] == "network.connect"
                && event["data"]["redirected"] == false
                && event["data"]["transport"] == "udp")
    );
}
