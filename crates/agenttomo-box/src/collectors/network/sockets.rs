//! Identity of sockets created inside this diagnostic scope. No protocol state.
use crate::syscall::Call;
use nix::{libc, unistd::Pid};
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub struct Socket {
    pub id: u64,
    pub domain: i32,
    pub kind: i32,
    pub protocol: i32,
}
impl Socket {
    pub fn transport(self) -> &'static str {
        match (self.domain, self.kind, self.protocol) {
            (libc::AF_UNIX, _, _) => "unix",
            (libc::AF_INET | libc::AF_INET6, libc::SOCK_STREAM, 0 | libc::IPPROTO_TCP) => "tcp",
            (libc::AF_INET | libc::AF_INET6, libc::SOCK_DGRAM, 0 | libc::IPPROTO_UDP) => "udp",
            _ => "other",
        }
    }
}
fn inode(tid: Pid, fd: i32) -> Option<u64> {
    let path = std::fs::read_link(format!("/proc/{tid}/fd/{fd}")).ok()?;
    path.to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}
#[derive(Default)]
pub struct Registry {
    by_inode: HashMap<u64, Socket>,
    next_id: u64,
}
impl Registry {
    pub fn created(&mut self, call: Call, result: i64) {
        if result >= 0
            && let Some(inode) = inode(call.tid, result as i32)
        {
            // Kernel inode reuse must not reuse a closed socket's UDP routes.
            self.next_id += 1;
            self.by_inode.insert(
                inode,
                Socket {
                    id: self.next_id,
                    domain: call.args[0] as i32,
                    kind: call.args[1] as i32 & 0xf,
                    protocol: call.args[2] as i32,
                },
            );
        }
    }
    pub fn lookup(&self, tid: Pid, fd: i32) -> Option<Socket> {
        self.by_inode.get(&inode(tid, fd)?).copied()
    }
}
