#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;
use common::Fixture;
use serde_json::Value;
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Echo {
    address: SocketAddr,
    peers: Arc<Mutex<Vec<SocketAddr>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Echo {
    fn new() -> Self {
        Self::from_socket(UdpSocket::bind("127.0.0.1:0").unwrap())
    }
    fn from_socket(socket: UdpSocket) -> Self {
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let peers = Arc::new(Mutex::new(Vec::new()));
        let seen = peers.clone();
        let thread = std::thread::spawn(move || {
            let mut bytes = [0; 65536];
            while !flag.load(Ordering::Relaxed) {
                match socket.recv_from(&mut bytes) {
                    Ok((size, peer)) => {
                        seen.lock().unwrap().push(peer);
                        if size >= 12 && bytes[..4] == [0x12, 0x34, 1, 0] {
                            let mut response = bytes[..size].to_vec();
                            response[2..4].copy_from_slice(&[0x81, 0x80]);
                            response[7] = 1;
                            response.extend_from_slice(&[
                                0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 42,
                            ]);
                            socket.send_to(&response, peer).unwrap();
                        } else {
                            socket.send_to(&bytes[..size], peer).unwrap();
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("UDP server: {error}"),
                }
            }
        });
        Self {
            address,
            peers,
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

fn exercise(mode: &str) -> Vec<Value> {
    let first = Echo::new();
    let second = Echo::new();
    exercise_with(mode, first, second)
}
fn exercise_with(mode: &str, first: Echo, second: Echo) -> Vec<Value> {
    let fixture = Fixture::compile("udp");
    let output = fixture.run(&format!(
        "exec ./fixture {mode} {} {}",
        first.address.port(),
        second.address.port()
    ));
    assert_eq!(
        output.status.code(),
        Some(if mode == "background" { 23 } else { 0 }),
        "{mode}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fixture.events();
    assert!(
        events
            .iter()
            .filter(|event| event["kind"].as_str().unwrap().starts_with("network."))
            .all(|event| event["source"] == "udp_proxy" && event["category"] == "network")
    );
    let stdout = std::fs::read_to_string(fixture.0.join("run/stdout.log")).unwrap();
    assert!(stdout.contains("UDP_OK"), "{mode}: {stdout}");
    let port: u16 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PORT="))
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(
        first.peers.lock().unwrap()[0].port(),
        port,
        "UDP must actually traverse the proxy"
    );
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.udp.send" && event["source"] == "udp_proxy"),
        "no UDP send observations"
    );
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "network.udp.receive" && event["source"] == "udp_proxy"),
        "no restored source observations"
    );
    assert!(
        events.iter().any(|event| event["source"] == "udp_proxy"
            && event["data"]["packets_up"]
                .as_u64()
                .is_some_and(|count| count > 0)),
        "no proxy evidence"
    );
    events
}

#[test]
fn queued_datagrams_keep_the_generation_that_sent_them() {
    let events = exercise("rebind_burst");
    let sends: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.udp.send")
        .collect();
    assert_eq!(sends.len(), 30);
    for field in ["socket_id", "flow_id"] {
        let ids: std::collections::HashSet<_> = sends
            .iter()
            .filter_map(|e| e["data"][field].as_u64())
            .collect();
        assert_eq!(
            ids.len(),
            sends.len(),
            "a queued datagram acquired a later {field}"
        );
    }
}

#[test]
fn overlapping_reuseport_sockets_remain_participants_in_one_flow() {
    let events = exercise("overlap");
    let sends: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.udp.send")
        .map(|e| &e["data"])
        .collect();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["flow_id"], sends[1]["flow_id"]);
    assert_eq!(sends[1]["participants"].as_array().unwrap().len(), 2);
    assert_eq!(sends[1]["socket_ids"].as_array().unwrap().len(), 2);
    assert!(sends[1]["pid"].is_null());
}

#[test]
fn reopened_source_port_starts_a_new_flow_with_its_own_process() {
    let events = exercise("rebind");
    let sends: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "network.udp.send")
        .map(|e| &e["data"])
        .collect();
    assert_eq!(sends.len(), 2);
    assert_ne!(sends[0]["flow_id"], sends[1]["flow_id"]);
    assert_ne!(
        sends[0]["process_instance_id"],
        sends[1]["process_instance_id"]
    );
    assert!(
        sends
            .iter()
            .all(|data| data["pid"].is_number()
                && data["participants"].as_array().unwrap().len() == 1)
    );
}

#[test]
fn unconnected_datagrams_to_multiple_peers_are_forwarded_and_sources_restored() {
    exercise("plain");
}
#[test]
fn connected_write_writev_and_disconnect_use_udp_routes() {
    exercise("connected");
}
#[test]
fn readonly_sendmsg_headers_and_scatter_gather_are_preserved() {
    exercise("message");
}
#[test]
fn batched_datagrams_keep_lengths_zero_datagrams_and_original_sources() {
    exercise("batch");
}
#[test]
fn sendmmsg_partial_success_keeps_the_kernels_message_count() {
    exercise("partial");
}
#[test]
fn peek_truncation_and_zero_length_datagrams_remain_datagrams() {
    exercise("trunc");
}
#[test]
fn invalid_arguments_and_nonblocking_empty_receive_keep_kernel_errors() {
    let events = exercise("errors");
    // The C fixture asserts EAGAIN/EFAULT. Calls without packets are absent
    // from the authoritative proxy log.
    assert!(events.iter().all(|event| !(event["source"] == "ptrace"
        && event["kind"].as_str().unwrap().starts_with("network."))));
}
#[test]
fn shared_socket_threads_record_one_process_identity() {
    let events = exercise("shared");
    let pid = events
        .iter()
        .find(|event| event["kind"] == "process.exec")
        .unwrap()["data"]["pid"]
        .clone();
    let sends: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "network.udp.send")
        .collect();
    assert!(sends.iter().all(|event| event["data"]["pid"] == pid));
    assert!(sends.iter().all(|event| event["data"].get("tid").is_none()));
    assert!(
        events
            .iter()
            .filter(|event| event["kind"] == "process.exit")
            .all(|event| event["data"]["pid"] == pid),
        "PID must survive task exit"
    );
    let instances: std::collections::HashSet<_> = events
        .iter()
        .filter(|event| event["kind"] == "network.udp.send")
        .filter_map(|event| event["data"]["process_instance_id"].as_u64())
        .collect();
    assert_eq!(instances.len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "process.exit")
            .count(),
        1
    );
    assert!(sends.iter().all(|event| {
        let participants = event["data"]["participants"].as_array().unwrap();
        participants.len() == 1
            && participants[0]["pid"] == pid
            && participants[0].get("tid").is_none()
    }));
    assert!(
        events
            .iter()
            .filter(|event| event["source"] == "udp_proxy")
            .all(|event| event["data"]["attribution"] == "flow_participants")
    );
}
#[test]
fn udp_in_a_forked_dup_socket_continues_after_the_root_exits() {
    let events = exercise("background");
    let exit = events
        .iter()
        .position(|event| event["kind"] == "process.exit" && event["data"]["exit_code"] == 23)
        .unwrap();
    assert!(
        events[exit + 1..]
            .iter()
            .any(|event| event["kind"] == "network.udp.send")
    );
}
#[test]
fn dns_query_datagram_traverses_the_proxy() {
    exercise("dns");
}

#[test]
fn ipv6_udp_restores_the_original_peer_when_loopback_is_available() {
    let socket = match UdpSocket::bind("[::1]:0") {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!(
                "IPv6 loopback unavailable: {error}; run the fixture in an IPv6-enabled environment"
            );
            return;
        }
    };
    exercise_with("ipv6", Echo::from_socket(socket), Echo::new());
}

#[test]
fn truncated_source_buffers_do_not_overwrite_caller_memory_or_guess_a_flow() {
    let events = exercise("short_source");
    // The fixture checks every truncated caller buffer. The proxy reports the
    // complete peer actually seen on the wire, independently of recvfrom args.
    assert!(
        events
            .iter()
            .filter(|event| event["kind"] == "network.udp.receive")
            .all(|event| event["data"]["peer"].as_str().is_some())
    );
}

#[test]
fn recvmmsg_restores_completed_messages_before_an_invalid_header() {
    exercise("partial_receive");
}
