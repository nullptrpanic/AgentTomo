use nix::libc;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

pub fn decode(bytes: &[u8]) -> Option<SocketAddr> {
    let family = u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?) as i32;
    let port = u16::from_be_bytes(bytes.get(2..4)?.try_into().ok()?);
    match family {
        libc::AF_INET if bytes.len() >= 16 => Some(SocketAddr::new(
            Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]).into(),
            port,
        )),
        libc::AF_INET6 if bytes.len() >= 28 => Some(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).ok()?),
            port,
            u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            u32::from_ne_bytes(bytes[24..28].try_into().ok()?),
        ))),
        _ => None,
    }
}

pub fn encode(address: SocketAddr) -> Vec<u8> {
    let mut bytes = vec![0; if address.is_ipv4() { 16 } else { 28 }];
    let family = if address.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    } as u16;
    bytes[..2].copy_from_slice(&family.to_ne_bytes());
    bytes[2..4].copy_from_slice(&address.port().to_be_bytes());
    match address {
        SocketAddr::V4(address) => bytes[4..8].copy_from_slice(&address.ip().octets()),
        SocketAddr::V6(address) => {
            bytes[4..8].copy_from_slice(&address.flowinfo().to_be_bytes());
            bytes[8..24].copy_from_slice(&address.ip().octets());
            bytes[24..28].copy_from_slice(&address.scope_id().to_ne_bytes());
        }
    }
    bytes
}
