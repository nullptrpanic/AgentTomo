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
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let upload = async {
        let mut inspector = Inspector::default();
        let mut buffer = [0; 8192];
        let mut count = 0;
        loop {
            let length = client_read.read(&mut buffer).await?;
            if length == 0 {
                break;
            }
            let name = inspector.observe(&buffer[..length]);
            // Forward each chunk immediately, even while the parser needs more
            // bytes or has rejected its format. Never send TLS parser alerts.
            upstream_write.write_all(&buffer[..length]).await?;
            count += length as u64;
            if let Some(name) = name {
                events
                    .send(context.event(
                        "network.protocol",
                        json!({
                            "protocol":name.protocol, "domain":name.domain,
                            "domain_source":name.source, "scope":"connection_preface",
                        }),
                    ))
                    .await
                    .map_err(io::Error::other)?;
            }
        }
        upstream_write.shutdown().await?;
        Ok::<_, io::Error>(count)
    };
    let download = async {
        let count = copy_stream(&mut upstream_read, &mut client_write).await?;
        client_write.shutdown().await?;
        Ok::<_, io::Error>(count)
    };
    tokio::pin!(upload, download);
    let (mut up, mut down) = (None, None);
    let mut finished = false;
    loop {
        if let Some(up) = up
            && (down.is_some() || finished)
        {
            return Ok(Outcome {
                up,
                down,
                reason: if down.is_some() {
                    "closed"
                } else {
                    "scope_finished"
                },
            });
        }
        tokio::select! {
            biased;
            result = &mut upload, if up.is_none() => up = Some(result?),
            _ = scope_finished.cancelled(), if !finished => finished = true,
            result = &mut download, if down.is_none() && !finished => down = Some(result?),
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
