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
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    rc::Rc,
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
        socket: Rc<sockets::Socket>,
        destinations: Vec<Option<SocketAddr>>,
        pending: Option<Pending>,
    },
}
pub struct Query {
    pub call: Call,
    socket: Rc<sockets::Socket>,
    destinations: Vec<Option<SocketAddr>>,
    pending: Option<Pending>,
}
pub struct Collector {
    sockets: sockets::Registry,
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
            registry,
            netns,
        }
    }
    pub fn reap(&mut self) {
        self.sockets.reap();
    }
    pub fn enter(&mut self, call: Call) -> Option<Attempt> {
        if call.number == libc::SYS_socket {
            return Some(Attempt::Socket(call));
        }
        let socket = self.sockets.lookup(call.tid, call.args[0] as i32)?;
        if std::fs::read_link(format!("/proc/{}/ns/net", call.tid))
            .ok()
            .as_ref()
            != Some(&self.netns)
        {
            return None;
        }
        let peer = socket.peer.get();
        let destinations = match call.number {
            libc::SYS_connect => vec![remote(&call, call.args[1], call.args[2])],
            libc::SYS_bind => vec![],
            _ if socket.transport != Transport::Udp => return None,
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
        let pending = (socket.transport == Transport::Udp
            && !matches!(call.number, libc::SYS_connect | libc::SYS_bind))
        .then(|| {
            self.registry
                .begin_datagram(socket.source.get(), &destinations)
        });
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
        // The call holds the socket identity through its exit stop, even if
        // another thread has since closed the last FD and the index was reaped.
        if call.number == libc::SYS_bind {
            socket.source.set(None);
            return None;
        }
        if call.number == libc::SYS_connect {
            socket.source.set(None);
            socket.peer.set(destinations[0]);
            destinations[0]?;
        }
        if call.number == libc::SYS_sendmmsg {
            destinations.truncate(result as usize);
        }
        if destinations.iter().all(Option::is_none) {
            return None;
        }
        if let Some(source) = socket.source.get() {
            self.publish(&call, &socket, &destinations, source, pending.as_ref());
            return None;
        }
        Some(Query {
            call,
            socket,
            destinations,
            pending,
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
        query.socket.source.set(Some(source));
        if query.socket.transport != Transport::Udp || query.call.number != libc::SYS_connect {
            self.publish(
                &query.call,
                &query.socket,
                &query.destinations,
                source,
                query.pending.as_ref(),
            );
        }
    }
    fn publish(
        &self,
        call: &Call,
        socket: &sockets::Socket,
        destinations: &[Option<SocketAddr>],
        source: SocketAddr,
        pending: Option<&Pending>,
    ) {
        let Some(process) = call.process else {
            return;
        };
        for destination in destinations.iter().flatten() {
            self.registry.register(
                Key {
                    transport: socket.transport,
                    source,
                    destination: *destination,
                },
                socket.id,
                process,
                pending
                    .map(|send| send.started)
                    .unwrap_or_else(std::time::SystemTime::now),
                |ids| {
                    self.sockets
                        .any_udp_bound(call.tid, source.port(), ids)
                        .unwrap_or(true)
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Identity;
    use nix::unistd::Pid;
    use std::{net::UdpSocket, os::fd::AsRawFd};

    #[tokio::test]
    async fn a_completed_send_keeps_its_owner_if_the_socket_closes_before_the_exit_stop() {
        let registry = Registry::default();
        let mut collector = Collector::new(
            registry.clone(),
            std::fs::read_link("/proc/self/ns/net").unwrap(),
        );
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let source = socket.local_addr().unwrap();
        let destination = receiver.local_addr().unwrap();
        let address = address::encode(destination);
        let fd = socket.as_raw_fd() as u64;
        let call = |number, args| Call {
            tid: Pid::this(),
            process: Some(Identity {
                pid: 1,
                process_instance_id: 1,
            }),
            number,
            args,
        };
        let create = collector
            .enter(call(
                libc::SYS_socket,
                [libc::AF_INET as u64, libc::SOCK_DGRAM as u64, 0, 0, 0, 0],
            ))
            .unwrap();
        collector.finish(create, fd as i64);
        let connect = collector
            .enter(call(
                libc::SYS_connect,
                [fd, address.as_ptr() as u64, address.len() as u64, 0, 0, 0],
            ))
            .unwrap();
        socket.connect(destination).unwrap();
        let query = collector.finish(connect, 0).unwrap();
        collector.queried(query, Some(source));
        let write = collector
            .enter(call(libc::SYS_write, [fd, 0, 1, 0, 0, 0]))
            .unwrap();
        let sent = socket.send(b"x").unwrap();
        drop(socket);
        collector.reap();
        collector.finish(write, sent as i64);
        let flow = Key {
            transport: Transport::Udp,
            source,
            destination,
        };
        assert!(
            registry
                .resolve(&flow, None)
                .await
                .is_some_and(|metadata| metadata.socket_id == 1)
        );
    }
}
