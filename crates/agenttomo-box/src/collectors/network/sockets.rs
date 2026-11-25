//! Per-socket identity and routes, shared by all aliases of the kernel socket.
use crate::{network::association::Transport, syscall::Call};
use nix::{
    errno::Errno,
    libc,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor},
    unistd::Pid,
};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::{self, BufRead},
    net::SocketAddr,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    rc::Rc,
};

pub struct Socket {
    pub id: u64,
    inode: u64,
    table: &'static str,
    pub source: Cell<Option<SocketAddr>>,
    pub peer: Cell<Option<SocketAddr>>,
    pub transport: Transport,
}
impl Socket {
    pub(super) fn local_addr(&self, call: &Call) -> Option<SocketAddr> {
        // pidfd_getfd is optional (Linux 5.6 and ptrace permissions). Verify
        // the inode because another thread can replace the fd after syscall entry.
        if let Some(address) = self.duplicate_addr(call) {
            return Some(address);
        }
        let file = std::fs::File::open(format!("/proc/{}/net/{}", call.tid, self.table)).ok()?;
        for line in io::BufReader::new(file).lines().skip(1) {
            let line = line.ok()?;
            let mut fields = line.split_ascii_whitespace();
            let local = fields.nth(1)?;
            if fields.nth(7)?.parse::<u64>().ok()? != self.inode {
                continue;
            }
            let (ip, port) = local.split_once(':')?;
            let mut octets = Vec::new();
            for word in ip.as_bytes().as_chunks::<8>().0 {
                octets.extend(
                    u32::from_str_radix(std::str::from_utf8(word).ok()?, 16)
                        .ok()?
                        .to_ne_bytes(),
                );
            }
            let ip = match octets.as_slice() {
                &[a, b, c, d] => std::net::Ipv4Addr::new(a, b, c, d).into(),
                bytes => std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                    <[u8; 16]>::try_from(bytes).ok()?,
                )),
            };
            return Some(SocketAddr::new(ip, u16::from_str_radix(port, 16).ok()?));
        }
        None
    }

    fn duplicate_addr(&self, call: &Call) -> Option<SocketAddr> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let pidfd =
            Errno::result(unsafe { libc::syscall(libc::SYS_pidfd_open, call.tid.as_raw(), 0) })
                .ok()?;
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
        let fd = Errno::result(unsafe {
            libc::syscall(
                libc::SYS_pidfd_getfd,
                pidfd.as_raw_fd(),
                call.args[0] as i32,
                0,
            )
        })
        .ok()?;
        let file = unsafe { std::fs::File::from_raw_fd(fd as i32) };
        if file.metadata().ok()?.ino() != self.inode {
            return None;
        }
        let mut bytes = [0u8; 128];
        let mut len = bytes.len() as libc::socklen_t;
        Errno::result(unsafe {
            libc::getsockname(file.as_raw_fd(), bytes.as_mut_ptr().cast(), &mut len)
        })
        .ok()?;
        crate::network::address::decode(bytes.get(..len as usize)?)
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
pub struct Registry {
    by_inode: HashMap<u64, Rc<Socket>>,
    watches: Option<Inotify>,
    by_watch: HashMap<WatchDescriptor, (u64, u64)>,
    resync: bool,
    untracked: std::collections::VecDeque<(u64, u64)>,
    next_id: u64,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            by_inode: HashMap::new(),
            watches: Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK).ok(),
            by_watch: HashMap::new(),
            resync: false,
            untracked: Default::default(),
            next_id: 0,
        }
    }
}
impl Registry {
    pub fn created(&mut self, call: Call, result: i64) {
        if result < 0 {
            return;
        }
        self.reap();
        // Pin the inode while attaching the watch: another thread can replace the
        // tracee's FD. O_PATH does not keep the socket's open file description alive.
        let pin = self.watches.as_ref().and_then(|_| {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(format!("/proc/{}/fd/{result}", call.tid))
                .ok()?;
            let stat = file.metadata().ok()?;
            stat.file_type().is_socket().then_some((stat.ino(), file))
        });
        // A restricted procfs may still expose the socket link when it denies
        // opening the FD. Preserve attribution there without assuming a close.
        let Some(ino) = pin
            .as_ref()
            .map(|(ino, _)| *ino)
            .or_else(|| inode(call.tid, result as i32))
        else {
            return;
        };
        self.next_id += 1;
        let transport = match (
            call.args[0] as i32,
            call.args[1] as i32 & 0xf,
            call.args[2] as i32,
        ) {
            (libc::AF_INET | libc::AF_INET6, libc::SOCK_STREAM, 0 | libc::IPPROTO_TCP) => {
                Transport::Tcp
            }
            (libc::AF_INET | libc::AF_INET6, libc::SOCK_DGRAM, 0 | libc::IPPROTO_UDP) => {
                Transport::Udp
            }
            _ => {
                self.by_inode.remove(&ino);
                return;
            }
        };
        if let (Some(watches), Some((_, pin))) = (&self.watches, pin)
            && let Ok(wd) = watches.add_watch(
                format!("/proc/self/fd/{}", pin.as_raw_fd()).as_str(),
                AddWatchFlags::IN_ONESHOT | AddWatchFlags::IN_CLOSE,
            )
        {
            // If the original descriptor disappeared before the mark was armed,
            // its final close might already have happened.
            if inode(call.tid, result as i32) != Some(ino) {
                let _ = watches.rm_watch(wd);
                return;
            }
            self.by_watch.insert(wd, (ino, self.next_id));
        } else {
            // Missing close notifications must not retain unbounded inode history.
            self.untracked.push_back((ino, self.next_id));
            if self.untracked.len() > 16384
                && let Some((old, id)) = self.untracked.pop_front()
            {
                remove_generation(&mut self.by_inode, old, id);
            }
        }
        self.by_inode.insert(
            ino,
            Rc::new(Socket {
                id: self.next_id,
                inode: ino,
                table: match (transport, call.args[0] as i32 == libc::AF_INET6) {
                    (Transport::Tcp, false) => "tcp",
                    (Transport::Tcp, true) => "tcp6",
                    (Transport::Udp, false) => "udp",
                    (Transport::Udp, true) => "udp6",
                },
                source: Cell::new(None),
                peer: Cell::new(None),
                transport,
            }),
        );
    }
    pub fn reap(&mut self) {
        let Some(watches) = &self.watches else { return };
        loop {
            match watches.read_events() {
                Ok(events) if events.is_empty() => break,
                Ok(events) => {
                    for event in events {
                        if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                            self.resync = true;
                        } else if event.mask.contains(AddWatchFlags::IN_IGNORED)
                            && let Some((inode, id)) = self.by_watch.remove(&event.wd)
                        {
                            remove_generation(&mut self.by_inode, inode, id);
                        }
                    }
                }
                Err(Errno::EINTR) => continue,
                Err(_) => break,
            }
        }
        // After overflow, the kernel's still-armed one-shot marks are authoritative.
        // Failed reads or watches cannot establish that the last alias closed.
        if self.resync
            && let Ok(active) = active_watches(watches)
        {
            self.by_watch.retain(|wd, &mut (inode, id)| {
                let live = active.contains(&(wd.as_raw(), inode));
                if !live {
                    remove_generation(&mut self.by_inode, inode, id);
                }
                live
            });
            self.resync = false;
        }
    }
    pub fn lookup(&self, tid: Pid, fd: i32) -> Option<Rc<Socket>> {
        self.by_inode.get(&inode(tid, fd)?).cloned()
    }

    /// Only queried when a different socket first appears on an occupied UDP
    /// tuple. The kernel keeps a bound inode here until its last reference closes,
    /// including descriptors inherited by another process.
    pub fn any_udp_bound(&self, tid: Pid, port: u16, ids: &[u64]) -> io::Result<bool> {
        for table in ["udp", "udp6"] {
            let file = match std::fs::File::open(format!("/proc/{tid}/net/{table}")) {
                Err(e) if table == "udp6" && e.kind() == io::ErrorKind::NotFound => continue,
                other => other?,
            };
            for line in io::BufReader::new(file).lines().skip(1) {
                let line = line?;
                let mut fields = line.split_whitespace();
                let local_port = fields
                    .nth(1)
                    .and_then(|address| address.rsplit_once(':'))
                    .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
                    .ok_or_else(|| io::Error::other("missing UDP local port"))?;
                if local_port != port {
                    continue;
                }
                // Fields 0 and 1 were consumed above. The inode is field 9.
                let inode = fields
                    .nth(7)
                    .and_then(|field| field.parse::<u64>().ok())
                    .ok_or_else(|| io::Error::other("missing UDP socket inode"))?;
                if self
                    .by_inode
                    .get(&inode)
                    .is_some_and(|socket| ids.contains(&socket.id))
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn remove_generation(sockets: &mut HashMap<u64, Rc<Socket>>, inode: u64, id: u64) {
    if sockets.get(&inode).is_some_and(|s| s.id == id) {
        sockets.remove(&inode);
    }
}

fn active_watches(watches: &Inotify) -> io::Result<HashSet<(i32, u64)>> {
    let info =
        std::fs::read_to_string(format!("/proc/self/fdinfo/{}", watches.as_fd().as_raw_fd()))?;
    info.lines()
        .filter_map(|line| line.strip_prefix("inotify "))
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let wd = fields
                .next()
                .and_then(|s| s.strip_prefix("wd:"))
                .and_then(|s| i32::from_str_radix(s, 16).ok());
            let inode = fields
                .next()
                .and_then(|s| s.strip_prefix("ino:"))
                .and_then(|s| u64::from_str_radix(s, 16).ok());
            wd.zip(inode)
                .ok_or_else(|| io::Error::other("invalid kernel watch record"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::UdpSocket,
        os::fd::{AsRawFd, OwnedFd},
        process::{Command, Stdio},
    };

    fn track(registry: &mut Registry, socket: &UdpSocket) -> u64 {
        let fd = socket.as_raw_fd();
        let call = Call {
            tid: Pid::this(),
            process: None,
            number: libc::SYS_socket,
            args: [libc::AF_INET as u64, libc::SOCK_DGRAM as u64, 0, 0, 0, 0],
        };
        registry.created(call, i64::from(fd));
        registry.lookup(call.tid, fd).unwrap().id
    }

    #[test]
    fn short_lived_sockets_are_reclaimed_but_inherited_descriptors_keep_their_identity() {
        let mut registry = Registry::default();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let survivor = inode(Pid::this(), socket.as_raw_fd()).unwrap();
        let id = track(&mut registry, &socket);
        let duplicate: OwnedFd = socket.try_clone().unwrap().into();
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(duplicate))
            .spawn()
            .unwrap();
        drop(socket);
        for _ in 0..512 {
            track(&mut registry, &UdpSocket::bind("127.0.0.1:0").unwrap());
        }
        registry.reap();
        let retained = registry.by_inode.len();
        let inherited_id = registry.by_inode.get(&survivor).map(|s| s.id);
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        for _ in 0..512 {
            track(&mut registry, &UdpSocket::bind("127.0.0.1:0").unwrap());
        }
        registry.reap();
        assert_eq!(inherited_id, Some(id));
        assert_eq!(retained, 1, "only the inherited socket should remain");
        assert!(registry.by_inode.is_empty(), "all descriptors have closed");
    }

    #[test]
    fn sockets_remain_identifiable_when_close_watches_are_unavailable() {
        let mut registry = Registry {
            watches: None,
            ..Registry::default()
        };
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let id = track(&mut registry, &socket);
        let duplicate = socket.try_clone().unwrap();
        drop(socket);
        registry.reap();
        assert_eq!(
            registry
                .lookup(Pid::this(), duplicate.as_raw_fd())
                .unwrap()
                .id,
            id
        );
    }

    #[test]
    fn lost_close_notifications_reconcile_against_live_kernel_marks() {
        let mut registry = Registry::default();
        let live = UdpSocket::bind("127.0.0.1:0").unwrap();
        let live_id = track(&mut registry, &live);
        let closed = UdpSocket::bind("127.0.0.1:0").unwrap();
        let old_inode = inode(Pid::this(), closed.as_raw_fd()).unwrap();
        track(&mut registry, &closed);
        drop(closed);
        // Drop the notification as an overflowing kernel queue would.
        registry.watches.as_ref().unwrap().read_events().unwrap();
        registry.resync = true;
        registry.reap();
        assert_eq!(
            registry.lookup(Pid::this(), live.as_raw_fd()).unwrap().id,
            live_id
        );
        assert!(!registry.by_inode.contains_key(&old_inode));
        assert_eq!(registry.by_watch.len(), 1);
    }
}
