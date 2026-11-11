//! Private, ordered IPC between the foreground (or playground) and its collector.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
pub enum Message {
    Started { pid: i32 },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    RootExit(u8),
    Error(String),
}

#[derive(Serialize, Deserialize)]
pub enum Input {
    Bytes(Vec<u8>),
    Closed,
    Resize { rows: u16, cols: u16 },
    AgentPrompt { task_id: String, prompt: String },
    AgentCancel,
}

pub async fn send(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub async fn receive<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> io::Result<T> {
    let mut bytes = Vec::new();
    // Each sender emits at most an 8 KiB byte chunk. Bound malformed frames.
    let count = reader.take(64 * 1024).read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "collector IPC closed",
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete collector IPC frame",
        ));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Keep partial bytes across select cancellation on interactive control streams.
pub struct Reader<R> {
    input: R,
    pending: Vec<u8>,
}
impl<R: AsyncBufRead + Unpin> Reader<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            pending: Vec::new(),
        }
    }
    pub async fn receive<T: serde::de::DeserializeOwned>(&mut self) -> io::Result<T> {
        let remaining = (64 * 1024usize).saturating_sub(self.pending.len());
        let count = (&mut self.input)
            .take(remaining as u64)
            .read_until(b'\n', &mut self.pending)
            .await?;
        if count == 0 && self.pending.is_empty() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if self.pending.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete control frame",
            ));
        }
        let result = serde_json::from_slice(&self.pending).map_err(io::Error::other);
        self.pending.clear();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_cancelled_receive_retains_the_partial_control_frame() {
        let (mut write, read) = tokio::io::duplex(1024);
        let mut reader = Reader::new(tokio::io::BufReader::new(read));
        write.write_all(b"{\"value\":").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                reader.receive::<serde_json::Value>()
            )
            .await
            .is_err()
        );
        write.write_all(b"42}\n").await.unwrap();
        assert_eq!(
            reader.receive::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"value":42})
        );
    }
}
