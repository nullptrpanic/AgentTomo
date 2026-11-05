//! Tokio's copy owns buffering/backpressure. Separate directions allow the
//! target's write-half close to coexist with a still-readable response.

use std::io;
use tokio::{
    io::{AsyncWriteExt, copy as copy_stream},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;

pub struct Outcome {
    pub up: u64,
    pub down: Option<u64>,
    pub reason: &'static str,
}

pub async fn copy(
    mut client: TcpStream,
    mut upstream: TcpStream,
    scope_finished: CancellationToken,
) -> io::Result<Outcome> {
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();
    let upload = async {
        let count = copy_stream(&mut client_read, &mut upstream_write).await?;
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
        net::TcpListener,
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
            let transfer = tokio::spawn(copy(client, upstream, finished));
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
