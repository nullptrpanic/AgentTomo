//! Persistent transparent UDP ingress, independent of TCP and syscall collection.
use super::timing::TransferTiming;
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
    metadata: Option<Metadata>,
}
struct Route {
    id: u64,
    socket_id: Option<u64>,
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
        let metadata = registry.resolve(&key).await;
        let socket_id = metadata.as_ref().map(|m| m.socket_id);
        if routes
            .get(&key)
            .is_some_and(|route| route.socket_id != socket_id || route.sender.is_closed())
        {
            routes.remove(&key);
        }
        if !routes.contains_key(&key) {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            if routes.len() >= 1024 {
                emit(&events, event(key, id, metadata.as_ref(), "network.udp.error", json!({"stage":"capacity", "error":"UDP flow limit reached; datagram dropped"}))).await?;
                continue;
            }
            let (sender, receiver) = mpsc::channel(64);
            let events = events.clone();
            let reply = reply.clone();
            let registry = registry.clone();
            tasks.spawn(async move {
                (
                    key,
                    id,
                    forward(key, id, receiver, reply, registry, events, timing).await,
                )
            });
            routes.insert(
                key,
                Route {
                    id,
                    socket_id,
                    sender,
                },
            );
        }
        let route = routes.get(&key).unwrap();
        if let Err(error) = route.sender.try_send(Datagram {
            bytes: buffer[..size].to_vec(),
            metadata,
        }) {
            emit(&events, event(key, route.id, error.into_inner().metadata.as_ref(), "network.udp.error", json!({"stage":"queue", "error":"UDP flow queue unavailable; datagram dropped"}))).await?;
        }
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
    id: u64,
    mut outgoing: mpsc::Receiver<Datagram>,
    reply: Arc<AsyncFd<OwnedFd>>,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    timing: TransferTiming,
) -> io::Result<()> {
    let ip = if key.destination.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let upstream = UdpSocket::bind((ip, 0)).await?;
    let mut incoming = vec![0; 65536];
    let mut counts = Counts::default();
    let mut metadata: Option<Metadata> = None;
    let idle = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(idle);
    let status = loop {
        tokio::select! {
            biased;
            request = outgoing.recv() => {
                let Some(request) = request else { break "scope_finished"; };
                metadata = request.metadata;
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
    let mut summary = json!({"status":status, "bytes_up":counts.up, "bytes_down":counts.down, "packets_up":counts.packets_up, "packets_down":counts.packets_down});
    timing.complete(&mut summary);
    emit(
        &events,
        event(key, id, metadata.as_ref(), "network.udp.transfer", summary),
    )
    .await
}
