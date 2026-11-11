//! Traced parent for the terminal and chat children. Commands never enter the PTY.
use super::Scope;
use crate::execution::protocol;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, os::unix::fs::PermissionsExt, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
};

pub const SOCKET: &str = "/records/control/workspace.sock";
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Start { scope: Scope, prompt: String },
    Cancel,
}

pub async fn run(browser: bool) -> io::Result<u8> {
    let listener = UnixListener::bind(SOCKET)?;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
    let mut terminal = if browser {
        Command::new("/usr/local/bin/agenttomo-browser-session")
    } else {
        let mut terminal = Command::new("/bin/bash");
        terminal.args(["--noprofile", "--norc", "-i"]);
        terminal
    };
    super::codex::configure(&mut terminal)?;
    let mut terminal = terminal.kill_on_drop(true).spawn()?;
    let connection = tokio::select! {
        accepted = listener.accept() => accepted?.0,
        status = terminal.wait() => return Ok(status?.code().unwrap_or(125) as u8),
    };
    let (read, mut write) = connection.into_split();
    let mut read = protocol::Reader::new(BufReader::new(read));
    let mut task: Option<Child> = None;
    let mut output = None;
    let mut current = None;
    let mut thread = None;
    let (diagnostics, mut notices) = tokio::sync::mpsc::channel::<String>(32);
    loop {
        tokio::select! {
            status = terminal.wait() => { kill_group(task.as_mut()); return Ok(status?.code().unwrap_or(125) as u8) },
            action = read.receive::<Action>() => {
                match action? {
                    Action::Cancel => kill_group(task.as_mut()),
                    Action::Start { scope, prompt } => {
                        if task.is_some() || prompt.trim().is_empty() || prompt.len() > 8192 {
                            protocol::send(&mut write, &json!({"kind":"agent.task.rejected","scope":scope,"error":"one active task is allowed; prompt must be 1–8192 bytes"})).await?;
                            continue;
                        }
                        let start = async {
                            let mut child = super::codex::command(&scope, thread.as_deref())?
                                .process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
                            let mut stdin = child.stdin.take().unwrap();
                            stdin.write_all(prompt.as_bytes()).await?;
                            drop(stdin);
                            let stdout = child.stdout.take().unwrap();
                            let stderr = child.stderr.take().unwrap();
                            let diagnostics = diagnostics.clone();
                            output = Some(tokio::spawn(async move { tokio::try_join!(super::codex::read_runtime_output(stdout, diagnostics), read_output(stderr)) }));
                            task = Some(child);
                            current = Some(scope.clone());
                            Ok::<_, io::Error>(())
                        }.await;
                        let event = match start {
                            Ok(()) => json!({"kind":"agent.task.started","scope":scope,"prompt":prompt}),
                            Err(error) => json!({"kind":"agent.task.rejected","scope":scope,"error":error.to_string()}),
                        };
                        protocol::send(&mut write, &event).await?;
                    }
                }
            },
            Some(error) = notices.recv() => {
                if let Some(scope) = &current {
                    protocol::send(&mut write, &json!({"kind":"agent.task.diagnostic","scope":scope,"error":error})).await?;
                }
            },
            status = async { match &mut task { Some(child) => child.wait().await, None => std::future::pending().await } } => {
                let code = status?.code();
                // Bound pipe draining even when a surviving tool retains stdout.
                let captured = match output.take() {
                    Some(mut output) => match tokio::time::timeout(std::time::Duration::from_secs(2), &mut output).await {
                        Ok(Ok(Ok(value))) => value,
                        _ => { output.abort(); (super::codex::RuntimeOutput::default(), String::new()) },
                    },
                    None => (super::codex::RuntimeOutput::default(), String::new()),
                };
                if let Some(id) = captured.0.thread { thread = Some(id); }
                let error = captured.0.error.unwrap_or_else(|| {
                    if captured.1.trim().is_empty() { format!("Codex exited unsuccessfully ({code:?})") }
                    else { captured.1.chars().take(8192).collect::<String>() }
                });
                protocol::send(&mut write, &json!({"kind":"agent.task.finished","scope":current.take(),"exit_code":code,"error":if code == Some(0) { Value::Null } else { error.into() }})).await?;
                while notices.try_recv().is_ok() {}
                task = None;
            }
        }
    }
}

fn kill_group(child: Option<&mut Child>) {
    if let Some(child) = child
        && let Some(pid) = child.id()
    {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}
async fn read_output(mut input: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let count = count.min((1024 * 1024usize).saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn connect() -> io::Result<UnixStream> {
    for _ in 0..300 {
        match UnixStream::connect(SOCKET).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "workspace chat control channel did not become ready",
    ))
}

pub async fn drive(
    mut actions: tokio::sync::mpsc::Receiver<Action>,
    events: tokio::sync::mpsc::Sender<crate::recording::Observation>,
    agents: super::Hub,
) -> io::Result<()> {
    let stream = connect().await?;
    let (read, mut write) = stream.into_split();
    let mut read = protocol::Reader::new(BufReader::new(read));
    let mut running = None;
    let mut cancelled = false;
    loop {
        tokio::select! {
            action = actions.recv() => match action {
                Some(action) => {
                    if let Action::Start { scope, .. } = &action && running.is_none() {
                        running = Some(scope.clone());
                        cancelled = false;
                    }
                    if matches!(action, Action::Cancel) { cancelled = true; }
                    if matches!(action, Action::Cancel) && let Some(scope) = &running && let Err(error) = agents.cancel(scope) {
                        events.send(crate::recording::Observation { source:"agent_launcher", kind:"agent.capture.error", data:json!({"task_id":scope.task_id,"agent_instance_id":scope.agent_instance_id,"error":format!("some task processes could not be stopped and remain traced: {error}"),"captured_unix_ns":super::now()}) }).await.map_err(io::Error::other)?;
                    }
                    protocol::send(&mut write, &action).await?;
                },
                None => return Ok(()),
            },
            event = read.receive::<Value>() => {
                let mut data = match event {
                    Ok(data) => data,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                };
                let kind = match data["kind"].as_str() {
                    Some("agent.task.started") => "agent.task.started",
                    Some("agent.task.finished") => "agent.task.finished",
                    Some("agent.task.rejected") => "agent.task.rejected",
                    Some("agent.task.diagnostic") => "agent.task.diagnostic",
                    _ => return Err(io::Error::other("invalid workspace control event")),
                };
                let scope: Scope = serde_json::from_value(data["scope"].take())?;
                if kind == "agent.task.started" { running = Some(scope.clone()); }
                if kind == "agent.task.rejected" && running.as_ref() == Some(&scope) {
                    data["cancelled"] = cancelled.into();
                    running = None;
                }
                if kind == "agent.task.finished" {
                    data["cancelled"] = cancelled.into();
                    if cancelled { data["error"] = Value::Null; }
                    running = None;
                }
                data["agent_instance_id"] = scope.agent_instance_id.into();
                data["task_id"] = scope.task_id.into();
                data["captured_unix_ns"] = json!(super::now());
                events.send(crate::recording::Observation { source:"agent_launcher", kind, data }).await.map_err(io::Error::other)?;
            }
        }
    }
}
