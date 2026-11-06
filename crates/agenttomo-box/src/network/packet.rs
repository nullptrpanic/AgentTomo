//! UDP reply envelopes for the namespace-owned raw socket. The kernel supplies
//! the IPv4 header checksum; UDP checksums include the original endpoint pair.
use std::{io, net::SocketAddr};

pub fn reply(source: SocketAddr, destination: SocketAddr, payload: &[u8]) -> io::Result<Vec<u8>> {
    let udp_len = payload
        .len()
        .checked_add(8)
        .filter(|n| *n <= u16::MAX as usize)
        .ok_or_else(|| io::Error::other("UDP reply too large"))?;
    let (mut packet, mut pseudo) = match (source, destination) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) if udp_len <= 65515 => {
            let mut ip = vec![0; 20];
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&((20 + udp_len) as u16).to_be_bytes());
            ip[8] = 64;
            ip[9] = 17;
            ip[12..16].copy_from_slice(&source.ip().octets());
            ip[16..20].copy_from_slice(&destination.ip().octets());
            let mut pseudo = ip[12..20].to_vec();
            pseudo.extend([0, 17]);
            pseudo.extend((udp_len as u16).to_be_bytes());
            (ip, pseudo)
        }
        (SocketAddr::V6(source), SocketAddr::V6(destination)) => {
            let mut ip = vec![0; 40];
            ip[0] = 0x60;
            ip[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
            ip[6] = 17;
            ip[7] = 64;
            ip[8..24].copy_from_slice(&source.ip().octets());
            ip[24..40].copy_from_slice(&destination.ip().octets());
            let mut pseudo = ip[8..40].to_vec();
            pseudo.extend((udp_len as u32).to_be_bytes());
            pseudo.extend([0, 0, 0, 17]);
            (ip, pseudo)
        }
        _ => return Err(io::Error::other("invalid UDP reply endpoints or size")),
    };
    let offset = packet.len();
    packet.extend(source.port().to_be_bytes());
    packet.extend(destination.port().to_be_bytes());
    packet.extend((udp_len as u16).to_be_bytes());
    packet.extend([0, 0]);
    packet.extend(payload);
    pseudo.extend(&packet[offset..]);
    let mut sum = 0u32;
    for word in pseudo.chunks(2) {
        sum += u16::from_be_bytes([word[0], *word.get(1).unwrap_or(&0)]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let checksum = !(sum as u16);
    packet[offset + 6..offset + 8]
        .copy_from_slice(&(if checksum == 0 { 0xffff } else { checksum }).to_be_bytes());
    Ok(packet)
}
