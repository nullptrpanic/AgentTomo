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
