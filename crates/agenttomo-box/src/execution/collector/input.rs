//! Record every input frame while delivering controls independently of terminal backpressure.

use hyper::body::Bytes;
use std::io;
use std::os::fd::AsRawFd;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::pipe::Receiver;
use tokio::sync::{mpsc, oneshot, watch};

use crate::execution::protocol::{self, Input};
use crate::terminal::Pty;

#[derive(Clone, Default)]
struct Pending {
    bytes: u64,
    frames: u64,
    latest: Bytes,
    closed: bool,
}

async fn deliver(
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    mut log: File,
    mut pending: watch::Receiver<Pending>,
    acknowledgements: mpsc::Sender<()>,
) -> io::Result<()> {
    let mut stdin = Some(stdin);
    let mut delivered = 0;
    let mut acknowledged = 0;
    while pending.changed().await.is_ok() {
        let target = pending.borrow_and_update().clone();
        if let Some(output) = &mut stdin
            && delivered < target.bytes
        {
            let write = async {
                // The common case uses the latest frame directly. Earlier queued
                // frames are already in stdin.log and need no extra memory.
                let backlog = target.bytes - target.latest.len() as u64 - delivered;
                if backlog > 0 {
                    log.seek(io::SeekFrom::Start(delivered)).await?;
                    let count = tokio::io::copy(&mut (&mut log).take(backlog), output).await?;
                    if count != backlog {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                }
                output.write_all(&target.latest).await
            };
            match write.await {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    stdin.take();
                }
                result => result?,
            }
        }
        delivered = target.bytes;
        if target.closed {
            stdin.take();
        }
        while acknowledged < target.frames {
            acknowledgements.send(()).await.map_err(io::Error::other)?;
            acknowledged += 1;
        }
    }
    Ok(())
}

pub(super) async fn record(
    input: Receiver,
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    terminal: Option<Pty>,
    mut log: File,
    stop: oneshot::Receiver<()>,
    agents: mpsc::Sender<crate::agent::workspace::Action>,
    acknowledgements: mpsc::Sender<()>,
) -> io::Result<()> {
    let mut input = BufReader::new(input);
    // Reopen the same inode with its own read position, even if the path changes.
    let read_log = File::open(format!("/proc/self/fd/{}", log.as_raw_fd())).await?;
    let (writes, queued) = watch::channel(Pending::default());
    let write_input = deliver(stdin, read_log, queued, acknowledgements);
    let result: io::Result<()> = {
        let copy = async {
            loop {
                match protocol::receive(&mut input).await? {
                    Input::AgentPrompt {
                        task_id,
                        prompt,
                        sample,
                    } => {
                        if task_id.len() != 32
                            || !task_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                            || sample.is_none() && prompt.trim().is_empty()
                            || prompt.len() > 8192
                        {
                            return Err(io::Error::other("invalid agent prompt"));
                        }
                        let agent_instance_id =
                            std::env::var("AGENTTOMO_AGENT_ID").map_err(io::Error::other)?;
                        agents
                            .send(crate::agent::workspace::Action::Start {
                                scope: crate::agent::Scope {
                                    agent_instance_id,
                                    task_id,
                                },
                                prompt,
                                sample,
                                report_marker: None,
                            })
                            .await
                            .map_err(io::Error::other)?;
                    }
                    Input::AgentCancel => agents
                        .send(crate::agent::workspace::Action::Cancel { scope: None })
                        .await
                        .map_err(io::Error::other)?,
                    Input::Resize { rows, cols } => {
                        if let Some(terminal) = &terminal {
                            terminal.resize(rows, cols)?;
                        }
                    }
                    Input::Closed => writes.send_modify(|state| state.closed = true),
                    Input::Bytes(bytes) => {
                        log.write_all(&bytes).await?;
                        log.flush().await?;
                        writes.send_modify(|state| {
                            state.frames += 1;
                            if !state.closed {
                                state.bytes += bytes.len() as u64;
                                state.latest = bytes.into();
                            }
                        });
                    }
                }
            }
        };
        tokio::select! { result = copy => result, result = write_input => result, _ = stop => Ok(()) }
    };
    log.flush().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_reaches_the_agent_while_pipelined_terminal_input_is_blocked() {
        let path = std::env::temp_dir().join(format!("agenttomo-stdin-{}", uuid::Uuid::new_v4()));
        let log = File::create_new(&path).await.unwrap();
        let recorded = File::open(&path).await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        let (mut frontend, input) = tokio::net::unix::pipe::pipe().unwrap();
        let (stdin, mut child) = tokio::io::duplex(1);
        let (stop, stopped) = oneshot::channel();
        let (agents, mut actions) = mpsc::channel(1);
        let (acks, mut received) = mpsc::channel(32);
        let task = tokio::spawn(record(
            input,
            Box::new(stdin),
            None,
            log,
            stopped,
            agents,
            acks,
        ));
        let mut expected = Vec::new();
        for byte in 0..16 {
            let chunk = vec![byte; 128];
            expected.extend_from_slice(&chunk);
            protocol::send(&mut frontend, &Input::Bytes(chunk))
                .await
                .unwrap();
        }
        protocol::send(&mut frontend, &Input::Closed).await.unwrap();
        protocol::send(&mut frontend, &Input::AgentCancel)
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), actions.recv())
                .await
                .unwrap(),
            Some(crate::agent::workspace::Action::Cancel { .. })
        ));
        let mut delivered = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), child.read_to_end(&mut delivered))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered, expected);
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..16 {
                received.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        let mut bytes = Vec::new();
        let mut recorded = recorded;
        recorded.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, expected);
    }
}
