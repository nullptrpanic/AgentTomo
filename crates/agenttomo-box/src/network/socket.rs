//! Linux transparent sockets, original-destination ancillary data and FD I/O.
use super::address;
use nix::{errno::Errno, libc};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, UdpSocket},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

fn create(ipv6: bool, kind: i32, protocol: i32) -> io::Result<OwnedFd> {
    let fd = Errno::result(unsafe {
        libc::socket(
            if ipv6 { libc::AF_INET6 } else { libc::AF_INET },
            kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            protocol,
        )
    })?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if ipv6 && kind != libc::SOCK_RAW {
        option(fd.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;
    }
    Ok(fd)
}

fn option(fd: RawFd, level: i32, option: i32, value: i32) -> io::Result<()> {
    Errno::result(unsafe {
        libc::setsockopt(fd, level, option, (&value as *const i32).cast(), 4)
    })?;
    Ok(())
}

fn listener(ipv6: bool, kind: i32, port: u16) -> io::Result<OwnedFd> {
    let fd = create(ipv6, kind, 0)?;
    option(
        fd.as_raw_fd(),
        if ipv6 {
            libc::IPPROTO_IPV6
        } else {
            libc::IPPROTO_IP
        },
        if ipv6 { 75 } else { libc::IP_TRANSPARENT },
        1,
    )?;
    option(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_MARK, 2)?;
    let ip = if ipv6 {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    let bytes = address::encode(SocketAddr::new(ip, port));
    Errno::result(unsafe {
        libc::bind(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len() as u32)
    })?;
    Ok(fd)
}

pub fn tcp_listener(ipv6: bool, port: u16) -> io::Result<TcpListener> {
    let fd = listener(ipv6, libc::SOCK_STREAM, port)?;
    Errno::result(unsafe { libc::listen(fd.as_raw_fd(), 1024) })?;
    Ok(TcpListener::from(fd))
}

pub fn udp_listener(ipv6: bool, port: u16) -> io::Result<UdpSocket> {
    let fd = listener(ipv6, libc::SOCK_DGRAM, port)?;
    option(
        fd.as_raw_fd(),
        if ipv6 {
            libc::IPPROTO_IPV6
        } else {
            libc::IPPROTO_IP
        },
        if ipv6 { 74 } else { libc::IP_RECVORIGDSTADDR },
        1,
    )?;
    option(
        fd.as_raw_fd(),
        libc::SOL_SOCKET,
        libc::SO_RCVBUF,
        1024 * 1024,
    )?;
    Ok(UdpSocket::from(fd))
}

pub fn raw_reply(ipv6: bool) -> io::Result<OwnedFd> {
    let fd = create(ipv6, libc::SOCK_RAW, libc::IPPROTO_RAW)?;
    option(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_MARK, 2)?;
    option(
        fd.as_raw_fd(),
        if ipv6 {
            libc::IPPROTO_IPV6
        } else {
            libc::IPPROTO_IP
        },
        if ipv6 {
            libc::IPV6_HDRINCL
        } else {
            libc::IP_HDRINCL
        },
        1,
    )?;
    Ok(fd)
}

pub fn datagram(fd: RawFd, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr, SocketAddr)> {
    let mut source = [0u8; 128];
    let mut ancillary = [0usize; 32];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_name = source.as_mut_ptr().cast();
    header.msg_namelen = source.len() as u32;
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = ancillary.as_mut_ptr().cast();
    header.msg_controllen = std::mem::size_of_val(&ancillary);
    let count =
        Errno::result(unsafe { libc::recvmsg(fd, &mut header, libc::MSG_DONTWAIT) })? as usize;
    if header.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
        return Err(io::Error::other("truncated transparent UDP datagram"));
    }
    let mut destination = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            let item = &*cmsg;
            if (item.cmsg_level == libc::IPPROTO_IP && item.cmsg_type == libc::IP_ORIGDSTADDR)
                || (item.cmsg_level == libc::IPPROTO_IPV6 && item.cmsg_type == 74)
            {
                let len = item.cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize);
                destination =
                    address::decode(std::slice::from_raw_parts(libc::CMSG_DATA(cmsg), len));
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }
    let source = address::decode(&source[..header.msg_namelen as usize]);
    match (source, destination) {
        (Some(source), Some(destination)) => Ok((count, source, destination)),
        _ => Err(io::Error::other(
            "UDP original source/destination unavailable",
        )),
    }
}

pub fn send_raw(fd: RawFd, packet: &[u8], destination: SocketAddr) -> io::Result<()> {
    let address = address::encode(SocketAddr::new(destination.ip(), 0));
    let count = Errno::result(unsafe {
        libc::sendto(
            fd,
            packet.as_ptr().cast(),
            packet.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            address.as_ptr().cast(),
            address.len() as u32,
        )
    })? as usize;
    if count != packet.len() {
        return Err(io::Error::other("partial raw UDP reply"));
    }
    Ok(())
}
