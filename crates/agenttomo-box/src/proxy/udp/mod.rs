//! Persistent transparent UDP ingress, independent of TCP and syscall collection.
use crate::timing::OperationTiming as TransferTiming;
use crate::{
    network::{
        association::{self, Key, Metadata, Registry, Transport},
        packet, socket,
    },
    recording::Observation,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    net::UdpSocket,
    sync::mpsc,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

struct Datagram {
    bytes: Vec<u8>,
    received: TransferTiming,
}
struct Route {
    id: u64,
    flow_id: Arc<AtomicU64>,
    sender: mpsc::Sender<Datagram>,
}
#[derive(Default)]
struct Counts {
    up: u64,
    down: u64,
    packets_up: u64,
    packets_down: u64,
}

fn event(
    key: Key,
    id: u64,
    metadata: Option<&Metadata>,
    kind: &'static str,
    mut data: Value,
) -> Observation {
    association::fields(key, metadata, &mut data);
    data["flow_id"] = id.into();
    Observation {
        source: "udp_proxy",
        kind,
        data,
    }
}
async fn emit(events: &mpsc::Sender<Observation>, observation: Observation) -> io::Result<()> {
    events.send(observation).await.map_err(io::Error::other)
}

pub async fn run(
    endpoints: Vec<(std::net::UdpSocket, OwnedFd)>,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    finished: CancellationToken,
) -> io::Result<()> {
    let ids = Arc::new(AtomicU64::new(1));
    let mut services = JoinSet::new();
    for (socket, reply) in endpoints {
        services.spawn(serve(
            socket,
            reply,
            registry.clone(),
            events.clone(),
            finished.clone(),
            ids.clone(),
        ));
    }
    while let Some(result) = services.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(())
}

async fn serve(
    socket: std::net::UdpSocket,
    reply: OwnedFd,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    finished: CancellationToken,
    ids: Arc<AtomicU64>,
) -> io::Result<()> {
    let local = UdpSocket::from_std(socket)?;
    // OwnedFd remains open and unchanged for the registration's lifetime.
    let reply = Arc::new(unsafe { AsyncFd::register(reply) }.map_err(io::Error::other)?);
    let mut buffer = vec![0; 65536];
    let mut routes: HashMap<Key, Route> = HashMap::new();
    let mut tasks = JoinSet::new();
    let mut stopping = false;
    loop {
        let packet = if stopping {
            match socket::datagram(local.as_raw_fd(), &mut buffer) {
                Ok(packet) => packet,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        } else {
            tokio::select! {
                biased;
                _ = finished.cancelled() => { stopping = true; continue; }
                packet = local.async_io(Interest::READABLE, || socket::datagram(local.as_raw_fd(), &mut buffer)) => packet?,
                result = tasks.join_next(), if !tasks.is_empty() => {
                    let (key, id, result) = result.unwrap().map_err(io::Error::other)?;
                    result?;
                    if routes.get(&key).is_some_and(|route| route.id == id) { routes.remove(&key); }
                    continue;
                }
            }
        };
        let (size, source, destination) = packet;
        let timing = TransferTiming::begin("proxy_first_datagram");
        let key = Key {
            transport: Transport::Udp,
            source,
            destination,
        };
        if routes
            .get(&key)
            .is_some_and(|route| route.sender.is_closed())
        {
            routes.remove(&key);
        }
        if !routes.contains_key(&key) {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            if routes.len() >= 1024 {
                emit(&events, event(key, id, None, "network.udp.error", json!({"stage":"capacity", "error":"UDP flow limit reached; datagram dropped"}))).await?;
                continue;
            }
            let (sender, receiver) = mpsc::channel(64);
            let events = events.clone();
            let reply = reply.clone();
            let registry = registry.clone();
            let ids = ids.clone();
            let flow_id = Arc::new(AtomicU64::new(id));
            let current_flow = flow_id.clone();
            tasks.spawn(async move {
                (
                    key,
                    id,
                    forward(key, current_flow, receiver, reply, registry, events, ids).await,
                )
            });
            routes.insert(
                key,
                Route {
                    id,
                    flow_id,
                    sender,
                },
            );
        }
        let route = routes.get(&key).unwrap();
        if let Err(error) = route.sender.try_send(Datagram {
            bytes: buffer[..size].to_vec(),
            received: timing,
        }) {
            drop(error);
            emit(&events, event(key, route.flow_id.load(Ordering::Relaxed), None, "network.udp.error", json!({"stage":"queue", "error":"UDP flow queue unavailable; datagram dropped"}))).await?;
        }
        // Give flow workers a chance to drain their queues during ready bursts.
        tokio::task::consume_budget().await;
    }
    drop(routes); // Drain queued requests; no reply wait after every tracee exits.
    while let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)?.2?;
    }
    Ok(())
}

async fn raw_reply(
    reply: &AsyncFd<OwnedFd>,
    bytes: &[u8],
    destination: std::net::SocketAddr,
) -> io::Result<()> {
    loop {
        let mut ready = reply.writable().await?;
        match ready.try_io(|fd| socket::send_raw(fd.get_ref().as_raw_fd(), bytes, destination)) {
            Ok(result) => return result,
            Err(_) => continue,
        }
    }
}

async fn forward(
    key: Key,
    flow_id: Arc<AtomicU64>,
    mut outgoing: mpsc::Receiver<Datagram>,
    reply: Arc<AsyncFd<OwnedFd>>,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    ids: Arc<AtomicU64>,
) -> io::Result<()> {
    let mut id = flow_id.load(Ordering::Relaxed);
    let ip = if key.destination.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let mut upstream = UdpSocket::bind((ip, 0)).await?;
    let mut incoming = vec![0; 65536];
    let mut counts = Counts::default();
    let mut metadata: Option<Metadata> = None;
    let mut timing: Option<TransferTiming> = None;
    let idle = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(idle);
    let status = loop {
        tokio::select! {
            biased;
            request = outgoing.recv() => {
                let Some(request) = request else { break "scope_finished"; };
                let next = registry.resolve(&key).await;
                if metadata.as_ref().map(|m| m.socket_id) != next.as_ref().map(|m| m.socket_id)
                    && let Some(previous) = timing.take()
                {
                    emit(&events, summary(key, id, metadata.as_ref(), &counts, previous, "scope_finished")).await?;
                    if let Some(metadata) = &metadata { registry.remove(&key, metadata.socket_id); }
                    upstream = UdpSocket::bind((ip, 0)).await?;
                    id = ids.fetch_add(1, Ordering::Relaxed);
                    flow_id.store(id, Ordering::Relaxed);
                    counts = Counts::default();
                }
                timing.get_or_insert(request.received);
                metadata = next;
                match upstream.send_to(&request.bytes, key.destination).await {
                    Ok(size) => {
                        counts.up += size as u64;
                        counts.packets_up += 1;
                        emit(&events, event(key, id, metadata.as_ref(), "network.udp.send", json!({"bytes":size, "peer":key.destination.to_string()}))).await?;
                    }
                    Err(error) => emit(&events, event(key, id, metadata.as_ref(), "network.udp.error", json!({"stage":"send", "error":error.to_string(), "errno":error.raw_os_error()}))).await?,
                }
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(120));
            }
            received = upstream.recv_from(&mut incoming) => {
                let (size, peer) = received?;
                let bytes = packet::reply(peer, key.source, &incoming[..size])?;
                match raw_reply(&reply, &bytes, key.source).await {
                    Ok(()) => {
                        counts.down += size as u64;
                        counts.packets_down += 1;
                        emit(&events, event(key, id, metadata.as_ref(), "network.udp.receive", json!({"bytes":size, "peer":peer.to_string()}))).await?;
                    }
                    Err(error) => emit(&events, event(key, id, metadata.as_ref(), "network.udp.error", json!({"stage":"reply", "error":error.to_string(), "errno":error.raw_os_error()}))).await?,
                }
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(120));
            }
            _ = &mut idle => break "idle_timeout",
        }
    };
    if let Some(metadata) = &metadata {
        registry.remove(&key, metadata.socket_id);
    }
    if let Some(timing) = timing {
        emit(
            &events,
            summary(key, id, metadata.as_ref(), &counts, timing, status),
        )
        .await?;
    }
    Ok(())
}

fn summary(
    key: Key,
    id: u64,
    metadata: Option<&Metadata>,
    counts: &Counts,
    timing: TransferTiming,
    status: &str,
) -> Observation {
    let mut data = json!({"status":status, "bytes_up":counts.up, "bytes_down":counts.down, "packets_up":counts.packets_up, "packets_down":counts.packets_down});
    timing.complete(&mut data);
    event(key, id, metadata, "network.udp.transfer", data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Identity;

    fn reply_socket() -> Arc<AsyncFd<OwnedFd>> {
        // These tests exercise forwarding only. The peer receives without replying.
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        // The owned, nonblocking descriptor is used only through this registration.
        Arc::new(unsafe { AsyncFd::register(socket.into()) }.unwrap())
    }

    fn datagram(byte: u8) -> Datagram {
        Datagram {
            bytes: vec![byte],
            received: TransferTiming::begin("proxy_first_datagram"),
        }
    }

    #[tokio::test]
    async fn waiting_flow_does_not_hold_other_flows_and_keeps_packet_order() {
        let registry = Registry::default();
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let first = Key {
            transport: Transport::Udp,
            source: "127.0.0.1:1001".parse().unwrap(),
            destination: remote.local_addr().unwrap(),
        };
        let second = Key {
            source: "127.0.0.1:1002".parse().unwrap(),
            ..first
        };
        registry.register(
            first,
            1,
            Identity {
                pid: 1,
                process_instance_id: 1,
            },
        );
        registry.register(
            second,
            2,
            Identity {
                pid: 2,
                process_instance_id: 2,
            },
        );
        let pending = registry.begin_datagram(Some(first.source), &[Some(first.destination)]);
        let (events, mut collected) = mpsc::channel(16);
        let ids = Arc::new(AtomicU64::new(3));
        let (first_sender, first_queue) = mpsc::channel(64);
        let (second_sender, second_queue) = mpsc::channel(64);
        let first_task = tokio::spawn(forward(
            first,
            Arc::new(AtomicU64::new(1)),
            first_queue,
            reply_socket(),
            registry.clone(),
            events.clone(),
            ids.clone(),
        ));
        let second_task = tokio::spawn(forward(
            second,
            Arc::new(AtomicU64::new(2)),
            second_queue,
            reply_socket(),
            registry.clone(),
            events,
            ids,
        ));
        first_sender.send(datagram(1)).await.unwrap();
        first_sender.send(datagram(2)).await.unwrap();
        second_sender.send(datagram(3)).await.unwrap();
        let mut buffer = [0; 8];
        tokio::time::timeout(Duration::from_millis(100), remote.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(buffer[0], 3);
        drop(pending);
        for expected in [1, 2] {
            tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(buffer[0], expected);
        }
        drop(first_sender);
        drop(second_sender);
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();
        let mut summaries = Vec::new();
        while let Some(event) = collected.recv().await {
            if event.kind == "network.udp.transfer" {
                summaries.push(event.data);
            }
        }
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries
                .iter()
                .any(|data| data["pid"] == 1 && data["packets_up"] == 2)
        );
        assert!(
            summaries
                .iter()
                .any(|data| data["pid"] == 2 && data["packets_up"] == 1)
        );
    }

    #[tokio::test]
    async fn socket_generation_change_starts_a_new_flow_and_summary() {
        let registry = Registry::default();
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let key = Key {
            transport: Transport::Udp,
            source: "127.0.0.1:1001".parse().unwrap(),
            destination: remote.local_addr().unwrap(),
        };
        registry.register(
            key,
            1,
            Identity {
                pid: 1,
                process_instance_id: 1,
            },
        );
        let (events, mut collected) = mpsc::channel(16);
        let (sender, outgoing) = mpsc::channel(64);
        let flow_id = Arc::new(AtomicU64::new(1));
        let task = tokio::spawn(forward(
            key,
            flow_id.clone(),
            outgoing,
            reply_socket(),
            registry.clone(),
            events,
            Arc::new(AtomicU64::new(2)),
        ));
        sender.send(datagram(1)).await.unwrap();
        let mut buffer = [0; 8];
        tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        registry.remove(&key, 1);
        registry.register(
            key,
            2,
            Identity {
                pid: 2,
                process_instance_id: 2,
            },
        );
        sender.send(datagram(2)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        drop(sender);
        task.await.unwrap().unwrap();
        let mut summaries = Vec::new();
        while let Some(event) = collected.recv().await {
            if event.kind == "network.udp.transfer" {
                summaries.push(event.data);
            }
        }
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0]["pid"], 1);
        assert_eq!(summaries[1]["pid"], 2);
        assert_ne!(summaries[0]["flow_id"], summaries[1]["flow_id"]);
        assert_eq!(summaries[1]["flow_id"], flow_id.load(Ordering::Relaxed));
        assert!(summaries.iter().all(|data| data["packets_up"] == 1));
    }
}
