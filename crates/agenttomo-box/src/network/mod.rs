//! Namespace transport plumbing. Collection and forwarding policy live elsewhere.
pub mod address;
pub mod association;
pub mod namespace;
pub mod packet;
pub mod socket;

pub struct Endpoints {
    pub tcp: Vec<std::net::TcpListener>,
    pub udp: Vec<(std::net::UdpSocket, std::os::fd::OwnedFd)>,
}
