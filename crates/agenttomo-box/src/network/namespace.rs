//! Bootstrap a private user/net namespace in the single-threaded launch helper.
//! Only descriptors cross back; all upstream sockets are opened in the parent.
use super::{Endpoints, socket};
use nix::{
    errno::Errno,
    libc,
    sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg},
};
use std::{
    io::{self, IoSlice, IoSliceMut, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    process::Command,
};

const PORT: u16 = 15001;

fn tool(name: &str) -> PathBuf {
    // Administrative tools are often deliberately absent from a normal user's PATH.
    for directory in ["/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        let path = PathBuf::from(directory).join(name);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

fn run(name: &str, args: &[&str], lock: &std::path::Path) -> io::Result<()> {
    let output = Command::new(tool(name))
        .args(args)
        .env("XTABLES_LOCKFILE", lock)
        .output()
        .map_err(|e| io::Error::other(format!("netfilter setup requires {name}: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "netfilter setup: {name} {}: {} ({})",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim(),
            output.status
        )));
    }
    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn capabilities(setup: bool, root_session: bool) -> io::Result<()> {
    let header = CapHeader {
        version: 0x20080522,
        pid: 0,
    };
    let mut caps = [CapData::default(); 2];
    if setup {
        Errno::result(unsafe { libc::syscall(libc::SYS_capget, &header, caps.as_mut_ptr()) })?;
        caps[0].inheritable |= (1 << 12) | (1 << 13);
    } else if root_session {
        // SETGID/SETUID allow the root terminal to launch an unprivileged
        // browser. Network and mount administration stay out of the workload.
        let identity = (1 << 6) | (1 << 7);
        caps[0] = CapData {
            effective: identity,
            permitted: identity,
            inheritable: identity,
        };
    }
    Errno::result(unsafe { libc::syscall(libc::SYS_capset, &header, caps.as_ptr()) })?;
    if setup || root_session {
        // Ambient capabilities survive exec for the namespace setup tools, or
        // for dropping the root terminal's child to the browser's ordinary UID.
        for cap in if setup { [12, 13] } else { [6, 7] } {
            Errno::result(unsafe {
                libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_RAISE, cap, 0, 0)
            })?;
        }
    } else {
        Errno::result(unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        })?;
    }
    Ok(())
}

pub fn prepare(
    mut control: &UnixStream,
    root_session: bool,
    prepare_filesystem: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let ipv6_egress = has_outer_ipv6_route()?;
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    Errno::result(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) })
        .map_err(|e| io::Error::other(format!("cannot create private user/network namespace: {e}; namespace/netfilter permissions are required (no fallback)")))?;
    if root_session {
        control.write_all(b"U")?;
        let mut mapped = [0];
        control.read_exact(&mut mapped)?;
        if mapped != *b"M" {
            return Err(io::Error::other("root session identity mapping failed"));
        }
    } else {
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))?;
    }
    capabilities(true, root_session)?;
    let directory = std::env::temp_dir().join(format!("agenttomo-net-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)?;
    let result = setup(&directory.join("xtables.lock"), ipv6_egress);
    let _ = std::fs::remove_dir_all(&directory);
    let endpoints = result?;
    prepare_filesystem()?;
    // A root-mapped caller must not regain capabilities at exec, either.
    Errno::result(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, 3, 0, 0, 0) })?;
    for cap in 0..64 {
        if root_session && [6, 7].contains(&cap) {
            continue;
        }
        let result = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        if result < 0 && Errno::last() != Errno::EINVAL {
            return Err(Errno::last().into());
        }
    }
    capabilities(false, root_session)?;
    nix::sys::prctl::set_no_new_privs()?;
    // Mapping credentials clears dumpability. Restore only this same-UID helper
    // so its parent tracer can inspect known FDs and memory after the gate.
    nix::sys::prctl::set_dumpable(true)?;
    let mut fds: Vec<_> = endpoints.tcp.iter().map(AsRawFd::as_raw_fd).collect();
    for (udp, reply) in &endpoints.udp {
        fds.extend([udp.as_raw_fd(), reply.as_raw_fd()]);
    }
    sendmsg::<()>(
        control.as_raw_fd(),
        &[IoSlice::new(b"N")],
        &[ControlMessage::ScmRights(&fds)],
        MsgFlags::MSG_NOSIGNAL,
        None,
    )?;
    Ok(())
}

pub fn map_root_session(control: &mut UnixStream, child: nix::unistd::Pid) -> io::Result<()> {
    let mut ready = [0];
    control.read_exact(&mut ready)?;
    if ready != *b"U" {
        let mut error = String::new();
        control.take(4096).read_to_string(&mut error)?;
        return Err(io::Error::other(format!(
            "cannot initialize root session: {error}"
        )));
    }
    // Only the parent can map multiple identities. Reuse its mapped ranges so
    // Chrome can drop privileges and create its own normal user namespace.
    for map in ["uid_map", "gid_map"] {
        let outer = std::fs::read_to_string(format!("/proc/self/{map}"))?;
        let mut inner = String::new();
        for line in outer.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if let [start, _, count] = fields.as_slice() {
                inner.push_str(&format!("{start} {start} {count}\n"));
            } else {
                return Err(io::Error::other("invalid parent identity map"));
            }
        }
        std::fs::write(format!("/proc/{child}/{map}"), inner)?;
    }
    control.write_all(b"M")
}

fn has_outer_ipv6_route() -> io::Result<bool> {
    let output = Command::new(tool("ip"))
        .args(["-j", "-6", "route", "show", "table", "all"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot inspect upstream IPv6 routes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let routes: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    Ok(routes.iter().any(|route| {
        matches!(route["type"].as_str(), None | Some("unicast"))
            && matches!(route["dst"].as_str(), Some(dst) if dst != "::1" && dst != "::1/128")
    }))
}

fn setup(lock: &std::path::Path, ipv6_egress: bool) -> io::Result<Endpoints> {
    // Maximum ordinary IPv6 packet: 40-byte header + 65535-byte payload.
    // IP_HDRINCL raw reply sockets do not fragment these packets themselves.
    run("ip", &["link", "set", "lo", "up", "mtu", "65575"], lock)?;
    let mut endpoints = Endpoints {
        tcp: Vec::new(),
        udp: Vec::new(),
    };
    for (version, firewall, loopback, all, ipv6) in [
        ("-4", "iptables", "127.0.0.1", "0.0.0.0/0", false),
        ("-6", "ip6tables", "::1", "::/0", true),
    ] {
        // Do not advertise global IPv6 reachability on an IPv4-only outer
        // network. A local proxy handshake would otherwise succeed before the
        // upstream reports ENETUNREACH, defeating browsers' IPv4 fallback.
        // Keep IPv6 loopback listeners/rules so local IPv6 services still work.
        if !ipv6 || ipv6_egress {
            run(
                "ip",
                &[
                    version, "route", "add", "default", "dev", "lo", "src", loopback,
                ],
                lock,
            )?;
        }
        run(
            "ip",
            &[
                version, "rule", "add", "priority", "100", "fwmark", "1", "lookup", "100",
            ],
            lock,
        )?;
        run(
            "ip",
            &[
                version, "route", "add", "local", all, "dev", "lo", "table", "100",
            ],
            lock,
        )?;
        for protocol in ["tcp", "udp"] {
            run(
                firewall,
                &[
                    "-t",
                    "mangle",
                    "-A",
                    "OUTPUT",
                    "-p",
                    protocol,
                    "-m",
                    "mark",
                    "!",
                    "--mark",
                    "2",
                    "-j",
                    "MARK",
                    "--set-mark",
                    "1",
                ],
                lock,
            )?;
            run(
                firewall,
                &[
                    "-t",
                    "mangle",
                    "-A",
                    "PREROUTING",
                    "-p",
                    protocol,
                    "-d",
                    if ipv6 { "::1/128" } else { "127.0.0.0/8" },
                    "-m",
                    "mark",
                    "--mark",
                    "1",
                    "-m",
                    "socket",
                    "--nowildcard",
                    "-j",
                    "ACCEPT",
                ],
                lock,
            )?;
            // Preserve real inner loopback services. If no local socket owns
            // the endpoint, external loopback services (including DNS) still
            // go through the proxy. Restrict this exception to loopback: an
            // INADDR_ANY listener must not capture a remote Internet address.
            run(
                firewall,
                &[
                    "-t",
                    "mangle",
                    "-A",
                    "PREROUTING",
                    "-p",
                    protocol,
                    "-m",
                    "mark",
                    "--mark",
                    "1",
                    "-j",
                    "TPROXY",
                    "--on-ip",
                    loopback,
                    "--on-port",
                    &PORT.to_string(),
                    "--tproxy-mark",
                    "1",
                ],
                lock,
            )?;
        }
        endpoints.tcp.push(socket::tcp_listener(ipv6, PORT)?);
        endpoints
            .udp
            .push((socket::udp_listener(ipv6, PORT)?, socket::raw_reply(ipv6)?));
    }
    Ok(endpoints)
}

pub fn receive(control: &mut UnixStream) -> io::Result<Endpoints> {
    let mut byte = [0];
    let mut ancillary = nix::cmsg_space!([libc::c_int; 6]);
    let mut fds = Vec::new();
    let (count, flags) = {
        let mut io = [IoSliceMut::new(&mut byte)];
        let message = recvmsg::<()>(
            control.as_raw_fd(),
            &mut io,
            Some(&mut ancillary),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )?;
        for item in message.cmsgs()? {
            if let ControlMessageOwned::ScmRights(raw) = item {
                fds.extend(
                    raw.into_iter()
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                );
            }
        }
        (message.bytes, message.flags)
    };
    if count != 1 || byte != *b"N" || fds.len() != 6 || flags.contains(MsgFlags::MSG_CTRUNC) {
        let mut error = String::new();
        control.take(4096).read_to_string(&mut error)?;
        return Err(io::Error::other(format!(
            "network namespace initialization failed: {error}"
        )));
    }
    let mut fds = fds.into_iter();
    Ok(Endpoints {
        tcp: [fds.next().unwrap(), fds.next().unwrap()]
            .into_iter()
            .map(std::net::TcpListener::from)
            .collect(),
        udp: (0..2)
            .map(|_| {
                (
                    std::net::UdpSocket::from(fds.next().unwrap()),
                    fds.next().unwrap(),
                )
            })
            .collect(),
    })
}
