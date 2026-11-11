//! Foreground client. It owns the caller's streams, but never owns ptrace.

pub(crate) mod child;
pub(crate) mod collector;
mod mounts;
pub(crate) mod protocol;
pub(crate) mod rootfs;
mod streams;
pub(crate) mod trust;

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Stdio;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpid, getppid, setsid};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::recording;
use protocol::{Input, Message};

pub async fn run(
    command: &OsStr,
    output_dir: Option<&Path>,
    preview_limits: recording::file_preview::Limits,
    http: crate::proxy::http::Options,
    rootfs: rootfs::Options,
    proxy: Option<crate::proxy::outbound::Proxy>,
) -> io::Result<u8> {
    let directory = output_dir
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(recording::default_output_dir)?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut worker = Command::new(std::env::current_exe()?);
    worker
        .arg("__collector")
        .arg("-c")
        .arg(command)
        .arg("--output-dir")
        .arg(directory)
        .arg("--file-preview-max-bytes")
        .arg(preview_limits.max_bytes.to_string())
        .arg("--file-preview-total-bytes")
        .arg(preview_limits.total_bytes.to_string())
        .args(http.arguments())
        .args(rootfs.arguments())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(proxy) = proxy {
        worker.arg("--proxy").arg(proxy.to_string());
    }
    let foreground = getpid();
    // Only syscalls run between fork and exec. The collector disarms this
    // notification before acknowledging a normal root exit.
    unsafe {
        worker.pre_exec(move || {
            setsid()?;
            nix::sys::prctl::set_pdeathsig(Signal::SIGUSR1)?;
            if getppid() != foreground {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
    // Explicitly manage errors below. A normal foreground exit must not kill it.
    let mut worker = worker.spawn()?;
    let input = worker.stdin.take().expect("piped worker stdin");
    let mut output = BufReader::new(worker.stdout.take().expect("piped worker stdout"));
    let (root_tx, mut root_rx) = watch::channel(None);
    let receive = async {
        loop {
            match protocol::receive(&mut output).await? {
                Message::Started { pid } => {
                    let _ = root_tx.send(Some(pid));
                }
                Message::Stdout(bytes) => forward("stdout", tokio::io::stdout(), &bytes).await?,
                Message::Stderr(bytes) => forward("stderr", tokio::io::stderr(), &bytes).await?,
                Message::RootExit(code) => return Ok(code),
                Message::Error(error) => return Err(io::Error::other(error)),
            }
        }
    };
    let signals = async {
        loop {
            let value = tokio::select! {
                _ = interrupt.recv() => Signal::SIGINT,
                _ = terminate.recv() => Signal::SIGTERM,
            };
            while root_rx.borrow().is_none() {
                root_rx.changed().await.map_err(io::Error::other)?;
            }
            if let Some(pid) = *root_rx.borrow() {
                match killpg(Pid::from_raw(pid), value) {
                    Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
                    Err(error) => return Err::<u8, _>(io::Error::from(error)),
                }
            }
        }
    };
    tokio::pin!(receive);
    let result = tokio::select! {
        result = &mut receive => result,
        result = send_input(input) => {
            match result {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => receive.await,
                Err(error) => Err(error),
                Ok(()) => unreachable!("input channel stays open after stdin EOF"),
            }
        }
        result = signals => result,
    };
    if result.is_err() {
        let _ = worker.kill().await;
        let _ = worker.wait().await;
    }
    result
}

async fn send_input(mut pipe: impl AsyncWrite + Unpin) -> io::Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buffer = [0; 8192];
    loop {
        let count = stdin.read(&mut buffer).await?;
        if count == 0 {
            protocol::send(&mut pipe, &Input::Closed).await?;
            // Keep IPC open: EOF here would mean the foreground disappeared.
            return std::future::pending().await;
        }
        protocol::send(&mut pipe, &Input::Bytes(buffer[..count].to_vec())).await?;
    }
}

async fn forward(name: &str, mut output: impl AsyncWrite + Unpin, bytes: &[u8]) -> io::Result<()> {
    async {
        output.write_all(bytes).await?;
        output.flush().await
    }
    .await
    .map_err(|error: io::Error| {
        io::Error::new(error.kind(), format!("{name} recording failed: {error}"))
    })
}
