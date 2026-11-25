//! Owner cursor reads the container journal through the persistent read channel.
use super::{bridge::Client, session::Frame};
use serde_json::json;
use std::{io, time::Duration};
use tokio::sync::mpsc;

pub async fn follow(client: Client, id: &str, sender: mpsc::Sender<Frame>) -> io::Result<()> {
    let mut offset = 0;
    let interval = Duration::from_millis(250);
    let mut retry = interval;
    loop {
        let batch = match client.events(id, offset).await {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(4));
                continue;
            }
            result => result?,
        };
        retry = interval;
        for event in batch["events"]
            .as_array()
            .ok_or_else(|| io::Error::other("invalid event batch"))?
        {
            sender
                .send(Frame::Json(json!({"type":"trace", "event":event})))
                .await
                .map_err(io::Error::other)?;
        }
        offset = batch["next_offset"]
            .as_u64()
            .ok_or_else(|| io::Error::other("invalid event cursor"))?;
        if batch["has_more"] == true {
            continue;
        }
        if batch["active"] == false {
            if batch["incomplete"] == true || batch["result"]["status"]["state"] != "complete" {
                return Err(io::Error::other("sandbox collection ended incompletely"));
            }
            sender
                .send(Frame::Complete(batch["result"]["status"].clone()))
                .await
                .map_err(io::Error::other)?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
