//! Owner cursor reads the container journal through the persistent read channel.
use super::{bridge::Client, session::Frame};
use serde_json::json;
use std::{io, time::Duration};
use tokio::sync::mpsc;

pub async fn follow(
    client: Client,
    id: &str,
    token: &str,
    sender: mpsc::Sender<Frame>,
) -> io::Result<()> {
    let mut offset = 0;
    loop {
        let batch = client.events(id, token, offset).await?;
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
