//! Known-FD identity lookup and TCP connect interception. No file collection.

mod address;

use crate::{
    proxy,
    syscall::{Call, memory, redirect::Patch, result_fields},
};
use nix::{libc, unistd::Pid};
use serde_json::{Value, json};
use std::{collections::HashMap, io, path::PathBuf, time::Instant};

pub const SYSCALLS: &[i64] = &[libc::SYS_socket, libc::SYS_connect];

pub struct Socket {
    domain: i32,
    kind: i32,
    protocol: i32,
}

pub enum Attempt {
    Socket { tid: Pid, socket: Socket },
    Connect(Box<Connect>),
}

pub struct Connect {
    tid: Pid,
    data: Value,
    started: Instant,
    patch: Option<Patch>,
    lease: Option<proxy::Lease>,
}

pub struct Collector {
    sockets: HashMap<u64, Socket>,
    proxy: proxy::Handle,
    netns: Option<PathBuf>,
    next_id: u64,
}

fn socket_inode(tid: Pid, fd: i32) -> Option<u64> {
    let path = std::fs::read_link(format!("/proc/{tid}/fd/{fd}")).ok()?;
    path.to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

impl Collector {
    pub fn new(proxy: proxy::Handle) -> Self {
        Self {
            sockets: HashMap::new(),
            proxy,
            netns: std::fs::read_link("/proc/self/ns/net").ok(),
            next_id: 0,
        }
    }

    pub fn enter(&mut self, call: &Call) -> io::Result<Option<Attempt>> {
        if call.number == libc::SYS_socket {
            return Ok(Some(Attempt::Socket {
                tid: call.tid,
                socket: Socket {
                    domain: call.args[0] as i32,
                    kind: call.args[1] as i32 & 0xf,
                    protocol: call.args[2] as i32,
                },
            }));
        }
        if call.number != libc::SYS_connect {
            return Ok(None);
        }
        self.next_id += 1;
        let id = self.next_id;
        let fd = call.args[0] as i32;
        let mut data = json!({"tid":call.tid.as_raw(),"task_id":call.task_id,"fd":fd,"connection_id":id,"redirected":false,"result_source":"kernel"});
        let socket = socket_inode(call.tid, fd).and_then(|inode| self.sockets.get(&inode));
        let transport = match socket {
            Some(socket) if socket.domain == libc::AF_UNIX => "unix",
            Some(socket)
                if matches!(socket.domain, libc::AF_INET | libc::AF_INET6)
                    && socket.kind == libc::SOCK_STREAM
                    && matches!(socket.protocol, 0 | libc::IPPROTO_TCP) =>
            {
                "tcp"
            }
            Some(socket)
                if matches!(socket.domain, libc::AF_INET | libc::AF_INET6)
                    && socket.kind == libc::SOCK_DGRAM
                    && matches!(socket.protocol, 0 | libc::IPPROTO_UDP) =>
            {
                "udp"
            }
            Some(_) => "other",
            None => "unknown",
        };
        data["transport"] = transport.into();
        let length = call.args[2] as usize;
        let bytes = if (2..=128).contains(&length) {
            match memory::read(call.tid, call.args[1], length) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    data["capture_error"] = error.to_string().into();
                    None
                }
            }
        } else {
            data["capture_error"] = "invalid sockaddr length".into();
            None
        };
        let destination = bytes.as_deref().and_then(address::decode);
        if let Some(address) = destination {
            data["destination"] = address.to_string().into();
        }
        if let Some(bytes) = &bytes {
            data["address_family"] = u16::from_ne_bytes(bytes[..2].try_into().unwrap()).into();
        }
        let mut attempt = Connect {
            tid: call.tid,
            data,
            started: Instant::now(),
            patch: None,
            lease: None,
        };
        let reason = match destination {
            None => "unsupported_or_unreadable_address",
            Some(_) if transport != "tcp" => "socket_not_known_tcp",
            Some(destination)
                if socket.is_some_and(|socket| {
                    socket.domain
                        != if destination.is_ipv4() {
                            libc::AF_INET
                        } else {
                            libc::AF_INET6
                        }
                }) =>
            {
                "address_family_mismatch"
            }
            Some(_)
                if self.netns.is_none()
                    || std::fs::read_link(format!("/proc/{}/ns/net", call.tid)).ok()
                        != self.netns =>
            {
                "different_or_unreadable_network_namespace"
            }
            Some(destination) => {
                let lease = self.proxy.route(proxy::Context {
                    connection_id: id,
                    tid: call.tid.as_raw(),
                    task_id: call.task_id,
                    destination,
                })?;
                attempt.patch = Some(Patch::connect(call, &address::encode(lease.address))?);
                attempt.data["redirected"] = true.into();
                attempt.data["result_source"] = "proxy_endpoint".into();
                attempt.data["proxy_address"] = lease.address.to_string().into();
                attempt.lease = Some(lease);
                ""
            }
        };
        if !reason.is_empty() {
            attempt.data["skip_reason"] = reason.into();
        }
        Ok(Some(Attempt::Connect(Box::new(attempt))))
    }

    pub fn finish(&mut self, attempt: Attempt, result: i64) -> io::Result<Option<Value>> {
        match attempt {
            Attempt::Socket { tid, socket } => {
                if result >= 0
                    && let Some(inode) = socket_inode(tid, result as i32)
                {
                    self.sockets.insert(inode, socket);
                }
                Ok(None)
            }
            Attempt::Connect(mut attempt) => {
                if let Some(patch) = attempt.patch.take() {
                    patch.restore(attempt.tid)?;
                }
                if (result == 0 || result == -(libc::EINPROGRESS as i64))
                    && let Some(lease) = &mut attempt.lease
                {
                    lease.commit();
                }
                result_fields(&mut attempt.data, result);
                attempt.data["duration_ns"] = json!(attempt.started.elapsed().as_nanos());
                Ok(Some(attempt.data))
            }
        }
    }
}
