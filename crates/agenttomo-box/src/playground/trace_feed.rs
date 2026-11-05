//! Read only appended, complete JSONL records; preserve partial lines across polls.

use super::session::Frame;
use serde_json::{Value, json};
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::{fs::File, io::AsyncReadExt, sync::mpsc};
use tokio_util::sync::CancellationToken;

pub async fn follow(
    path: PathBuf,
    finished: CancellationToken,
    sender: mpsc::Sender<Frame>,
) -> io::Result<()> {
    let mut file = None;
    let mut partial = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        // Only an EOF read started after collector completion is final. This
        // prevents a concurrent last append + completion from losing a record.
        let final_read = finished.is_cancelled();
        if file.is_none() {
            match File::open(&path).await {
                Ok(opened) => file = Some(opened),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if final_read {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let count = file.as_mut().unwrap().read(&mut buffer).await?;
        if count == 0 {
            if final_read {
                return if partial.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::other(
                        "trace ended with an incomplete JSONL record",
                    ))
                };
            }
            tokio::select! {
                _ = finished.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            continue;
        }
        partial.extend_from_slice(&buffer[..count]);
        if partial.len() > 16 * 1024 * 1024 {
            return Err(io::Error::other("trace record exceeds 16 MiB"));
        }
        let mut start = 0;
        for end in partial
            .iter()
            .enumerate()
            .filter_map(|(i, b)| (*b == b'\n').then_some(i))
        {
            let event: Value = serde_json::from_slice(&partial[start..end])?;
            sender
                .send(Frame::Json(json!({"type":"trace", "event":event})))
                .await
                .map_err(|_| io::Error::other("trace receiver closed"))?;
            start = end + 1;
        }
        partial.drain(..start);
    }
}
