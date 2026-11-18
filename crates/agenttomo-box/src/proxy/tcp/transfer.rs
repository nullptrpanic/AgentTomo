//! Separate directions preserve half-close and upload draining. Only the upload
//! needs a read/write loop to inspect the preface; download uses Tokio's copy.

use super::{Context, protocol::Inspector};
use crate::recording::Observation;
use serde_json::json;
use std::io;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy as copy_stream},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

pub struct Outcome {
    pub up: u64,
    pub down: Option<u64>,
    pub reason: &'static str,
}

pub async fn copy(
    client: impl AsyncRead + AsyncWrite + Unpin,
    upstream: impl AsyncRead + AsyncWrite + Unpin,
    scope_finished: CancellationToken,
    context: &Context,
    events: &mpsc::Sender<Observation>,
) -> io::Result<Outcome> {
    let (client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, upstream_write) = tokio::io::split(upstream);
    let mut up = 0;
    let mut down = None;
    let reason = {
        let upload = upload(client_read, upstream_write, &mut up, context, events);
        let download = async {
            let count = copy_stream(&mut upstream_read, &mut client_write).await?;
            client_write.shutdown().await?;
            Ok::<_, io::Error>(count)
        };
        tokio::pin!(upload, download);
        let mut uploaded = false;
        loop {
            tokio::select! {
                biased;
                result = &mut upload, if !uploaded => { result?; uploaded = true; }
                _ = scope_finished.cancelled() => {
                    // Preserve queued writes after the last tracee exits, but
                    // an upstream that stops reading cannot hold cleanup forever.
                    if !uploaded {
                        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut upload).await {
                            Ok(result) => result?,
                            Err(_) => break "upload_drain_timeout",
                        }
                    }
                    break if down.is_some() { "closed" } else { "scope_finished" };
                }
                result = &mut download, if down.is_none() => down = Some(result?),
            }
            if uploaded && down.is_some() {
                break "closed";
            }
        }
    };
    Ok(Outcome { up, down, reason })
}

async fn upload(
    mut client: impl AsyncRead + Unpin,
    mut upstream: impl AsyncWrite + Unpin,
    sent: &mut u64,
    context: &Context,
    events: &mpsc::Sender<Observation>,
) -> io::Result<()> {
    let mut inspector = Inspector::default();
    let mut buffer = [0; 8192];
    loop {
        let length = client.read(&mut buffer).await?;
        if length == 0 {
            return upstream.shutdown().await;
        }
        let name = inspector.observe(&buffer[..length]);
        let mut remaining = &buffer[..length];
        while !remaining.is_empty() {
            let n = upstream.write(remaining).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            *sent += n as u64;
            remaining = &remaining[n..];
        }
        if let Some(name) = name {
            events
                .send(context.event(
                    "network.protocol",
                    json!({
                        "protocol": name.protocol, "domain": name.domain,
                        "domain_source": name.source, "scope": "connection_preface",
                    }),
                ))
                .await
                .map_err(io::Error::other)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::AsyncReadExt,
        net::{TcpListener, TcpStream},
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn finished_scope_bounds_a_blocked_upload_and_counts_forwarded_bytes() {
        let (mut writer, client) = tokio::io::duplex(256);
        writer.write_all(&[42; 128]).await.unwrap();
        writer.shutdown().await.unwrap();
        let (upstream, mut peer) = tokio::io::duplex(8);
        let finished = CancellationToken::new();
        finished.cancel();
        let (events, _receiver) = mpsc::channel(1);
        let context = Context {
            connection_id: 1,
            key: crate::network::association::Key {
                transport: crate::network::association::Transport::Tcp,
                source: ([127, 0, 0, 1], 1).into(),
                destination: ([127, 0, 0, 1], 2).into(),
            },
            metadata: None,
        };
        let result = timeout(
            Duration::from_secs(7),
            copy(client, upstream, finished, &context, &events),
        )
        .await
        .expect("an unresponsive upstream held the finished session")
        .unwrap();
        let mut delivered = Vec::new();
        peer.read_to_end(&mut delivered).await.unwrap();
        assert_eq!(result.reason, "upload_drain_timeout");
        assert_eq!(delivered.len(), 8);
        assert_eq!(result.up, delivered.len() as u64);
    }

    #[tokio::test]
    async fn finished_scope_drains_upload_even_if_the_download_half_is_closed() {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut target = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut client, _) = listener.accept().await.unwrap();
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.set_send_buffer_size(4096).unwrap();
            let upstream = socket
                .connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            // A dead target has no use for this direction, but its queued
            // request must still reach a temporarily slow upstream.
            client.shutdown().await.unwrap();
            server.write_all(b"early response").await.unwrap();
            target.write_all(&vec![42; 256 * 1024]).await.unwrap();
            target.shutdown().await.unwrap();
            let finished = CancellationToken::new();
            finished.cancel();
            let (events, _receiver) = mpsc::channel(1);
            let context = Context {
                connection_id: 1,
                key: crate::network::association::Key {
                    transport: crate::network::association::Transport::Tcp,
                    source: client.peer_addr().unwrap(),
                    destination: listener.local_addr().unwrap(),
                },
                metadata: None,
            };
            let transfer =
                tokio::spawn(
                    async move { copy(client, upstream, finished, &context, &events).await },
                );
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut received = Vec::new();
            server.read_to_end(&mut received).await.unwrap();
            let result = transfer.await.unwrap().unwrap();
            assert_eq!(result.up, 256 * 1024);
            assert_eq!(received, vec![42; 256 * 1024]);
            assert_eq!(result.reason, "scope_finished");
        })
        .await
        .expect("upload did not drain");
    }
}
