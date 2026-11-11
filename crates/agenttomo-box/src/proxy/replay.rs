//! Replay a bounded inspected prefix without losing bidirectional stream semantics.
use super::BoxIo;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub fn replay(prefix: Vec<u8>, io: BoxIo) -> BoxIo {
    Box::new(Replay {
        prefix: std::io::Cursor::new(prefix),
        io,
    })
}

// Detection retains only a bounded preface. Replaying it is essential for
// fragmented HTTP and protocols that happen to start with ASCII.
struct Replay {
    prefix: std::io::Cursor<Vec<u8>>,
    io: BoxIo,
}
impl AsyncRead for Replay {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            Pin::new(&mut self.prefix).poll_read(cx, buf)
        } else {
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }
}
impl AsyncWrite for Replay {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
