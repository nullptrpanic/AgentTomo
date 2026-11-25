use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio_util::sync::CancellationToken;

pub use crate::proxy::BoxIo;

#[derive(Default)]
pub struct Counters {
    pub up: AtomicU64,
    pub down: AtomicU64,
}
pub struct Metered<T> {
    pub io: T,
    pub counts: Arc<Counters>,
}
impl<T: AsyncRead + Unpin> AsyncRead for Metered<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        self.counts
            .up
            .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Metered<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.io).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result {
            self.counts.down.fetch_add(count as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

pub async fn detect(
    mut client: BoxIo,
    upstream: BoxIo,
    finished: &CancellationToken,
) -> io::Result<(bool, BoxIo, BoxIo)> {
    let mut upstream = BufReader::new(upstream);
    let mut prefix = Vec::new();
    let mut buffer = [0; 8192];
    let mut deadline = None;
    let mut http = false;
    loop {
        let available = buffer.len().min(65536 - prefix.len());
        let count = tokio::select! {
            biased;
            count = client.read(&mut buffer[..available]) => count?,
            bytes = upstream.fill_buf() => { bytes?; break; },
            _ = finished.cancelled() => break,
            _ = async { if let Some(at) = deadline { tokio::time::sleep_until(at).await } else { std::future::pending().await } } => break,
        };
        if count == 0 {
            break;
        }
        deadline.get_or_insert_with(|| {
            tokio::time::Instant::now() + std::time::Duration::from_millis(500)
        });
        prefix.extend_from_slice(&buffer[..count]);
        let mut headers = [httparse::EMPTY_HEADER; 128];
        match httparse::Request::new(&mut headers).parse(&prefix) {
            Ok(httparse::Status::Complete(_)) => {
                http = true;
                break;
            }
            Ok(httparse::Status::Partial) if prefix.len() < 65536 => {}
            _ => break,
        }
    }
    Ok((
        http,
        crate::proxy::replay::replay(prefix, client),
        Box::new(upstream),
    ))
}

pub async fn opaque(
    mut client: BoxIo,
    mut upstream: BoxIo,
    finished: &CancellationToken,
) -> io::Result<()> {
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut upstream) => { result?; },
        _ = finished.cancelled() => { let _ = tokio::time::timeout(std::time::Duration::from_millis(250), async {
            tokio::join!(client.shutdown(), upstream.shutdown())
        }).await; },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_preconnection_still_recognizes_http_headers() {
        let (mut client, incoming) = tokio::io::duplex(128);
        let (upstream, _server) = tokio::io::duplex(128);
        let stop = CancellationToken::new();
        let read = detect(Box::new(incoming), Box::new(upstream), &stop);
        let send = async {
            tokio::time::sleep(std::time::Duration::from_millis(650)).await;
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n")
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(read, send);
        assert!(result.unwrap().0);
    }
}
