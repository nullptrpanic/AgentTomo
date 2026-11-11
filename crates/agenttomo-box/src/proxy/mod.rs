//! Independent protocol services; lifecycle coordination belongs to execution.
pub mod http;
pub mod outbound;
mod replay;
pub mod tcp;
mod tls;
pub mod udp;

pub trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Stream for T {}
pub type BoxIo = Box<dyn Stream>;
