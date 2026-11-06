//! Private socket attribution only. Never rewrites destinations or emits events.
mod sockets;

use crate::{
    network::{
        address,
        association::{Key, Pending, Registry, Transport},
    },
    syscall::{Call, memory},
};
use nix::libc;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};

pub fn watched_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_write,
        libc::SYS_writev,
    ]
}
pub enum Attempt {
    Socket(Call),
    Operation {
        call: Call,
        socket: sockets::Socket,
        destinations: Vec<Option<SocketAddr>>,
        pending: Option<Pending>,
    },
}
pub struct Query {
    pub call: Call,
    socket: sockets::Socket,
    destinations: Vec<Option<SocketAddr>>,
    _pending: Option<Pending>,
}
pub struct Collector {
    sockets: sockets::Registry,
    peers: HashMap<u64, SocketAddr>,
    sources: HashMap<u64, SocketAddr>,
    registry: Registry,
    netns: PathBuf,
}

fn remote(call: &Call, pointer: u64, length: u64) -> Option<SocketAddr> {
    if pointer == 0 {
        return None;
    }
    memory::read(call.tid, pointer, length.min(128) as usize)
        .ok()
        .and_then(|bytes| address::decode(&bytes))
        .map(address::canonical)
}
fn message(call: &Call, pointer: u64) -> Option<SocketAddr> {
    let bytes = memory::read(call.tid, pointer, std::mem::size_of::<libc::msghdr>()).ok()?;
    let header = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::msghdr>()) };
    remote(call, header.msg_name as u64, header.msg_namelen as u64)
}
impl Collector {
    pub fn new(registry: Registry, netns: PathBuf) -> Self {
        Self {
            sockets: Default::default(),
            peers: HashMap::new(),
            sources: HashMap::new(),
            registry,
            netns,
        }
    }
    pub fn enter(&mut self, call: Call) -> Option<Attempt> {
        if call.number == libc::SYS_socket {
            return Some(Attempt::Socket(call));
        }
        let socket = self.sockets.lookup(call.tid, call.args[0] as i32)?;
        if !matches!(socket.transport(), "tcp" | "udp")
            || std::fs::read_link(format!("/proc/{}/ns/net", call.tid))
                .ok()
                .as_ref()
                != Some(&self.netns)
        {
            return None;
        }
        let peer = self.peers.get(&socket.id).copied();
        let destinations = match call.number {
            libc::SYS_connect => vec![remote(&call, call.args[1], call.args[2])],
            libc::SYS_bind => vec![],
            _ if socket.transport() != "udp" => return None,
            libc::SYS_sendto => vec![remote(&call, call.args[4], call.args[5]).or(peer)],
            libc::SYS_sendmsg => vec![message(&call, call.args[1]).or(peer)],
            libc::SYS_sendmmsg => (0..call.args[2].min(1024))
                .map(|index| {
                    call.args[1]
                        .checked_add(index * std::mem::size_of::<libc::mmsghdr>() as u64)
                        .and_then(|pointer| message(&call, pointer))
                        .or(peer)
                })
                .collect(),
            libc::SYS_write | libc::SYS_writev => vec![peer],
            _ => return None,
        };
        // The proxy waits for in-flight datagram calls to finish so a failed
        // send cannot become an owner, nor an older writer be reported as sole
        // owner while another writer's metadata is still being resolved.
        let pending = (socket.transport() == "udp"
            && !matches!(call.number, libc::SYS_connect | libc::SYS_bind))
        .then(|| self.registry.begin_datagram());
        Some(Attempt::Operation {
            call,
            socket,
            destinations,
            pending,
        })
    }
    pub fn finish(&mut self, attempt: Attempt, result: i64) -> Option<Query> {
        let (call, socket, mut destinations, pending) = match attempt {
            Attempt::Socket(call) => {
                self.sockets.created(call, result);
                return None;
            }
            Attempt::Operation {
                call,
                socket,
                destinations,
                pending,
            } => (call, socket, destinations, pending),
        };
        if result < 0
            && !(call.number == libc::SYS_connect && result == -(libc::EINPROGRESS as i64))
        {
            return None;
        }
        if call.number == libc::SYS_bind {
            self.sources.remove(&socket.id);
            return None;
        }
        if call.number == libc::SYS_connect {
            self.sources.remove(&socket.id);
            match destinations[0] {
                Some(peer) => {
                    self.peers.insert(socket.id, peer);
                }
                None => {
                    self.peers.remove(&socket.id);
                    return None;
                }
            }
        }
        if call.number == libc::SYS_sendmmsg {
            destinations.truncate(result as usize);
        }
        if destinations.iter().all(Option::is_none) {
            return None;
        }
        if let Some(source) = self.sources.get(&socket.id).copied() {
            self.publish(&call, socket, &destinations, source);
            return None;
        }
        Some(Query {
            call,
            socket,
            destinations,
            _pending: pending,
        })
    }
    pub fn queried(&mut self, query: Query, source: Option<SocketAddr>) {
        // Another thread can close/reuse this FD during the query. A different
        // socket must not inherit the original call's identity.
        if self
            .sockets
            .lookup(query.call.tid, query.call.args[0] as i32)
            .map(|socket| socket.id)
            != Some(query.socket.id)
        {
            return;
        }
        let Some(mut source) = source.map(address::canonical).filter(|s| s.port() != 0) else {
            return;
        };
        // This namespace has only lo and routes egress through it. An unconnected
        // auto-bound UDP socket reports INADDR_ANY until connect.
        if source.ip().is_unspecified() {
            source.set_ip(if source.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            });
        }
        self.sources.insert(query.socket.id, source);
        if query.socket.transport() != "udp" || query.call.number != libc::SYS_connect {
            self.publish(&query.call, query.socket, &query.destinations, source);
        }
    }
    fn publish(
        &self,
        call: &Call,
        socket: sockets::Socket,
        destinations: &[Option<SocketAddr>],
        source: SocketAddr,
    ) {
        let Some(process) = call.process else {
            return;
        };
        let transport = if socket.transport() == "tcp" {
            Transport::Tcp
        } else {
            Transport::Udp
        };
        for destination in destinations.iter().flatten() {
            self.registry.register(
                Key {
                    transport,
                    source,
                    destination: *destination,
                },
                socket.id,
                process,
            );
        }
    }
}
