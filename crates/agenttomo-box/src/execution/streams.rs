//! Bind the tracer's descriptors to the collector's Tokio runtime.

use crate::{terminal::Pty, tracing};
use std::io;
use std::os::fd::AsRawFd;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::unix::pipe::{Receiver, Sender};

pub trait Output: AsyncRead + AsRawFd + Unpin + Send {}
impl<T: AsyncRead + AsRawFd + Unpin + Send> Output for T {}

pub struct Streams {
    pub input: Box<dyn AsyncWrite + Unpin + Send>,
    pub stdout: Box<dyn Output>,
    pub stderr: Option<Box<dyn Output>>,
    pub terminal: Option<Pty>,
}

impl Streams {
    pub fn new(streams: tracing::Streams) -> io::Result<Self> {
        Ok(match streams {
            tracing::Streams::Pipes(pipes) => Self {
                input: Box::new(Sender::from_owned_fd(pipes.stdin.into())?),
                stdout: Box::new(Receiver::from_owned_fd(pipes.stdout.into())?),
                stderr: Some(Box::new(Receiver::from_owned_fd(pipes.stderr.into())?)),
                terminal: None,
            },
            tracing::Streams::Terminal(fd) => {
                let terminal = Pty::new(fd)?;
                Self {
                    input: Box::new(terminal.clone()),
                    stdout: Box::new(terminal.clone()),
                    stderr: None,
                    terminal: Some(terminal),
                }
            }
        })
    }
}
