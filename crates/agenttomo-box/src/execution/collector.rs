//! Long-lived owner of tracing, recording, and the built-in proxy.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::pipe::Receiver;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::protocol::{self, Input, Message};
use super::streams::{Output, Streams};
use crate::recording::Recording;
use crate::terminal::Pty;
use crate::tracing::{self, Event};

nix::ioctl_read_bad!(bytes_available, nix::libc::FIONREAD, nix::libc::c_int);

pub async fn run(
    command: &OsStr,
    directory: &Path,
    with_terminal: bool,
    preview_limits: crate::recording::file_preview::Limits,
    http: crate::proxy::http::Options,
) -> io::Result<()> {
    let parent_lost = signal(SignalKind::user_defined1())?;
    let (mut recording, input_log) =
        Recording::create(command, directory, preview_limits, http.enabled).await?;
    let result = collect(
        command,
        with_terminal,
        input_log,
        &mut recording,
        parent_lost,
        preview_limits.enabled(),
        http,
    )
    .await;
    if let Err(error) = &result {
        let _ = recording.finish().await;
        let _ = recording.status("failed", Some(&error.to_string())).await;
    }
    result
}

async fn collect(
    command: &OsStr,
    with_terminal: bool,
    input_log: File,
    recording: &mut Recording,
    mut parent_lost: Signal,
    capture_files: bool,
    http: crate::proxy::http::Options,
) -> io::Result<()> {
    nix::sys::prctl::set_child_subreaper(true)?;
    let (observations, mut proxy_events) = mpsc::channel(256);
    let inspection = crate::proxy::http::Inspection::create(recording.directory(), http).await?;
    let trust = inspection
        .as_ref()
        .map(|inspection| super::trust::Overlay::new(&inspection.authority.pem))
        .transpose()?;
    let registry = crate::network::association::Registry::default();
    let associations = registry.clone();
    let (network, endpoints) = oneshot::channel::<crate::network::Endpoints>();
    let scope_finished = CancellationToken::new();
    let scope = scope_finished.clone();
    let mut proxy_task = tokio::spawn(async move {
        let endpoints = endpoints.await.map_err(io::Error::other)?;
        tokio::try_join!(
            crate::proxy::tcp::run(
                endpoints.tcp,
                associations.clone(),
                observations.clone(),
                scope.clone(),
                inspection,
            ),
            crate::proxy::udp::run(endpoints.udp, associations, observations, scope),
        )?;
        Ok::<_, io::Error>(())
    });
    let (events_tx, mut events) = mpsc::channel(256);
    let thread = tracing::spawn(
        command.to_owned(),
        with_terminal,
        events_tx,
        registry,
        network,
        capture_files,
        trust.as_ref().map(super::trust::Overlay::handoff),
    )?;
    let (pid, streams) = match events.recv().await {
        Some(Event::Started { pid, streams }) => (pid, streams),
        Some(Event::Failed(error)) => return Err(io::Error::other(error)),
        _ => return Err(io::Error::other("tracer ended before starting the command")),
    };
    let Streams {
        input: stdin,
        mut stdout,
        mut stderr,
        terminal,
    } = Streams::new(streams)?;
    // This process's stdin is a private pipe; take sole ownership of fd 0.
    let input = Receiver::from_owned_fd(unsafe { OwnedFd::from_raw_fd(0) })?;
    let (stop_input, stopped) = oneshot::channel();
    let mut stop_input = Some(stop_input);
    let mut input = tokio::spawn(record_input(input, stdin, terminal, input_log, stopped));
    let mut frontend = Some(tokio::io::stdout());
    protocol::send(frontend.as_mut().unwrap(), &Message::Started { pid }).await?;
    let mut out_buffer = [0; 8192];
    let mut err_buffer = [0; 8192];
    let mut stdout_open = true;
    let mut stderr_open = stderr.is_some();
    let mut input_done = false;
    let mut root_exited = false;
    let mut tracing_done = false;
    let mut proxy_done = false;
    let mut proxy_exited = false;
    loop {
        if tracing_done && proxy_done && proxy_exited {
            break;
        }
        tokio::select! {
            result = recording.writer_failure() => result?,
            event = events.recv(), if !tracing_done => match event {
                Some(Event::Process { kind, data }) => recording.event("ptrace", kind, data).await?,
                Some(Event::FileOpen(opened)) => recording.file_open(opened).await?,
                Some(Event::RootExit(code)) => {
                    // Drain a snapshot, not EOF or an unbounded busy pipe.
                    if let Some(stop) = stop_input.take() { let _ = stop.send(()); }
                    if !input_done { (&mut input).await.map_err(io::Error::other)??; input_done = true; }
                    drain(stdout.as_mut(), &mut recording.stdout, &mut frontend, true).await?;
                    if let Some(stderr) = stderr.as_mut() {
                        drain(stderr.as_mut(), &mut recording.stderr, &mut frontend, false).await?;
                    }
                    recording.root_exit(code).await?;
                    // This call runs on the worker's main OS thread, which
                    // inherited PDEATHSIG from the foreground's pre_exec.
                    nix::sys::prctl::set_pdeathsig(None)?;
                    if let Some(mut output) = frontend.take() {
                        protocol::send(&mut output, &Message::RootExit(code)).await?;
                    }
                    root_exited = true;
                }
                Some(Event::Finished) => { tracing_done = true; scope_finished.cancel(); },
                Some(Event::Failed(error)) => return Err(io::Error::other(error)),
                _ => return Err(io::Error::other("tracer closed without completing collection")),
            },
            event = proxy_events.recv(), if !proxy_done => match event {
                Some(event) => recording.event(event.source, event.kind, event.data).await?,
                None => proxy_done = true,
            },
            result = &mut proxy_task, if !proxy_exited => {
                result.map_err(io::Error::other)??;
                proxy_exited = true;
            },
            count = stdout.read(&mut out_buffer), if stdout_open => {
                let count = count?;
                stdout_open = count != 0;
                record_output(&out_buffer[..count], &mut recording.stdout, &mut frontend, true).await?;
            }
            count = async { stderr.as_mut().expect("enabled stderr").read(&mut err_buffer).await }, if stderr_open => {
                let count = count?;
                stderr_open = count != 0;
                record_output(&err_buffer[..count], &mut recording.stderr, &mut frontend, false).await?;
            }
            result = &mut input, if !input_done => {
                input_done = true;
                result.map_err(io::Error::other)??;
            }
            _ = parent_lost.recv(), if !root_exited => {
                return Err(io::Error::other("foreground exited before root completion"));
            }
        }
    }
    if !root_exited {
        return Err(io::Error::other(
            "tracer finished without a root exit status",
        ));
    }
    drain(stdout.as_mut(), &mut recording.stdout, &mut frontend, true).await?;
    if let Some(stderr) = stderr.as_mut() {
        drain(stderr.as_mut(), &mut recording.stderr, &mut frontend, false).await?;
    }
    recording.finish().await?;
    // Finished is the final message; the join cannot wait on a live tracee.
    thread
        .join()
        .map_err(|_| io::Error::other("tracer thread panicked"))?;
    recording.status("complete", None).await
}

async fn record_input(
    input: Receiver,
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    terminal: Option<Pty>,
    mut log: File,
    stop: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut input = BufReader::new(input);
    let mut stdin = Some(stdin);
    let result: io::Result<()> = {
        let copy = async {
            loop {
                match protocol::receive(&mut input).await? {
                    Input::Resize { rows, cols } => {
                        if let Some(terminal) = &terminal {
                            terminal.resize(rows, cols)?;
                        }
                    }
                    Input::Closed => {
                        stdin.take();
                    }
                    Input::Bytes(bytes) => {
                        log.write_all(&bytes).await?;
                        if let Some(output) = stdin.as_mut() {
                            match output.write_all(&bytes).await {
                                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                                    stdin.take();
                                }
                                result => result?,
                            }
                        }
                    }
                }
            }
        };
        tokio::select! { result = copy => result, _ = stop => Ok(()) }
    };
    log.flush().await?;
    result
}

async fn record_output(
    bytes: &[u8],
    log: &mut File,
    frontend: &mut Option<tokio::io::Stdout>,
    stdout: bool,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    log.write_all(bytes).await?;
    if let Some(output) = frontend.as_mut() {
        let message = if stdout {
            Message::Stdout(bytes.to_vec())
        } else {
            Message::Stderr(bytes.to_vec())
        };
        protocol::send(output, &message).await?;
    }
    Ok(())
}

async fn drain(
    pipe: &mut dyn Output,
    log: &mut File,
    frontend: &mut Option<tokio::io::Stdout>,
    stdout: bool,
) -> io::Result<()> {
    let mut queued = 0;
    // FIONREAD snapshots bytes in this pipe. This process is its sole reader.
    unsafe {
        bytes_available(pipe.as_raw_fd(), &mut queued)?;
    }
    let mut remaining = queued as usize;
    let mut buffer = [0; 8192];
    while remaining != 0 {
        let count = remaining.min(buffer.len());
        pipe.read_exact(&mut buffer[..count]).await?;
        record_output(&buffer[..count], log, frontend, stdout).await?;
        remaining -= count;
    }
    Ok(())
}
