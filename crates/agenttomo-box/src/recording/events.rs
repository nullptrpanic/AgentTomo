//! Ordered event persistence, with file preview I/O outside the collector loop.
use super::{Observation, file_preview};
use crate::collectors::file::Opened;
use serde_json::json;
use std::{
    io,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

pub(super) enum Content {
    Event(Observation),
    File(Opened),
}

struct Pending {
    observed: u128,
    elapsed: u128,
    content: Content,
}

pub(super) struct Writer {
    sender: Option<mpsc::Sender<Pending>>,
    task: Option<JoinHandle<io::Result<()>>>,
    started: Instant,
}

fn unix_ns() -> io::Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(io::Error::other)
}

impl Writer {
    pub fn start(
        output: impl AsyncWrite + Unpin + Send + 'static,
        previews: file_preview::Store,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        Self {
            sender: Some(sender),
            task: Some(tokio::spawn(write(receiver, output, previews))),
            started: Instant::now(),
        }
    }

    pub async fn send(&mut self, content: Content) -> io::Result<()> {
        let pending = Pending {
            content,
            observed: unix_ns()?,
            elapsed: self.started.elapsed().as_nanos(),
        };
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| io::Error::other("event writer is closed"))?;
        if sender.send(pending).await.is_err() {
            return self.failure().await;
        }
        Ok(())
    }

    pub async fn failure(&mut self) -> io::Result<()> {
        let result = self
            .task
            .as_mut()
            .ok_or_else(|| io::Error::other("event writer is closed"))?
            .await;
        self.task.take();
        result.map_err(io::Error::other)??;
        Err(io::Error::other(
            "event writer stopped before collection finished",
        ))
    }

    pub async fn finish(&mut self) -> io::Result<()> {
        self.sender.take();
        match self.task.take() {
            Some(task) => task.await.map_err(io::Error::other)?,
            None => Ok(()), // A previous failure was already returned to the collector.
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn write(
    mut receiver: mpsc::Receiver<Pending>,
    mut output: impl AsyncWrite + Unpin,
    mut previews: file_preview::Store,
) -> io::Result<()> {
    let mut sequence = 0_u64;
    while let Some(pending) = receiver.recv().await {
        let Observation { source, kind, data } = match pending.content {
            Content::Event(event) => event,
            Content::File(opened) => Observation {
                source: "ptrace",
                kind: "file.open",
                data: previews.capture(opened).await,
            },
        };
        sequence += 1;
        let category = match kind.split('.').next() {
            Some("network") => "network",
            Some("file") => "file",
            Some("process") => "process",
            Some("memory") => "memory",
            Some("agent") => "agent",
            _ => "other",
        };
        let mut bytes = serde_json::to_vec(&json!({
            "schema_version": 2, "sequence": sequence, "kind": kind,
            "category": category, "source": source, "data": data,
            "observed_unix_ns": pending.observed, "monotonic_ns": pending.elapsed,
            "recorded_unix_ns": unix_ns()?,
        }))?;
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
    }
    output.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::Recording;
    use std::{ffi::OsStr, path::PathBuf, time::Duration};
    use tokio::io::{AsyncReadExt, duplex};

    async fn recording(output: impl AsyncWrite + Unpin + Send + 'static) -> (PathBuf, Recording) {
        let directory =
            std::env::temp_dir().join(format!("agenttomo-event-writer-{}", uuid::Uuid::new_v4()));
        let limits = file_preview::Limits {
            max_bytes: 0,
            total_bytes: 0,
        };
        let (mut recording, input) =
            Recording::create(OsStr::new("test"), &directory, limits, false)
                .await
                .unwrap();
        drop(input);
        let previews = file_preview::Store::create(&directory, limits, false)
            .await
            .unwrap();
        let mut original =
            std::mem::replace(&mut recording.events, Writer::start(output, previews));
        original.finish().await.unwrap();
        (directory, recording)
    }

    #[tokio::test]
    async fn slow_event_storage_keeps_root_exit_responsive_and_drains_in_order() {
        let (output, mut reader) = duplex(1);
        let (directory, mut recording) = recording(output).await;
        recording
            .event("ptrace", "process.exec", json!({"n":0}))
            .await
            .unwrap();
        let mut prefix = [0];
        reader.read_exact(&mut prefix).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), recording.root_exit(7))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(directory.join("exit-code.txt")).unwrap(),
            b"7\n"
        );
        for n in 1..=256 {
            recording
                .event("ptrace", "process.exec", json!({"n":n}))
                .await
                .unwrap();
        }
        assert_eq!(recording.events.sender.as_ref().unwrap().capacity(), 0);
        let consume = {
            let enqueue = recording.event("ptrace", "process.exit", json!({"n":257}));
            tokio::pin!(enqueue);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut enqueue)
                    .await
                    .is_err()
            );
            let consume = tokio::spawn(async move {
                let mut bytes = prefix.to_vec();
                reader.read_to_end(&mut bytes).await.unwrap();
                bytes
            });
            enqueue.as_mut().await.unwrap();
            consume
        };
        recording.finish().await.unwrap();
        let bytes = consume.await.unwrap();
        let rows: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 258);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["sequence"], index + 1);
            assert_eq!(row["data"]["n"], index);
            assert!(
                row["recorded_unix_ns"].as_u64().unwrap()
                    >= row["observed_unix_ns"].as_u64().unwrap()
            );
        }
        assert!(
            rows.windows(2)
                .all(|pair| pair[0]["monotonic_ns"].as_u64().unwrap()
                    <= pair[1]["monotonic_ns"].as_u64().unwrap())
        );
        drop(recording);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn event_writer_errors_reach_the_collector() {
        let (output, reader) = duplex(1);
        let (directory, mut recording) = recording(output).await;
        drop(reader);
        recording
            .event("ptrace", "process.exec", json!({}))
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), recording.writer_failure())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        // Failure reporting and final cleanup must be safe in the same run.
        recording.finish().await.unwrap();
        drop(recording);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
