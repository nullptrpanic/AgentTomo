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
    Start {
        scope: Scope,
        prompt: String,
        #[serde(default)]
        sample: Option<String>,
    },
    Cancel,
}

pub async fn run(browser: bool) -> io::Result<u8> {
    let adapter = super::Adapter::selected()?
        .ok_or_else(|| io::Error::other("workspace requires an agent adapter"))?;
    let listener = UnixListener::bind(SOCKET)?;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
    let mut terminal = if browser {
        Command::new("/usr/local/bin/agenttomo-browser-session")
    } else {
        let mut terminal = Command::new("/bin/bash");
        terminal.args(["--noprofile", "--norc", "-i"]);
        terminal
    };
    adapter.configure(&mut terminal)?;
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
    let mut conversation = None;
    let mut sample_run: Option<crate::playground::samples::Run> = None;
    let mut cancelled = false;
    let (diagnostics, mut notices) = tokio::sync::mpsc::channel::<String>(32);
    loop {
        tokio::select! {
            status = terminal.wait() => { kill_group(task.as_mut()); return Ok(status?.code().unwrap_or(125) as u8) },
            action = read.receive::<Action>() => {
                match action? {
                    Action::Cancel => { cancelled = true; kill_group(task.as_mut()); },
                    Action::Start { scope, mut prompt, sample } => {
                        if task.is_some() || sample.is_none() && prompt.trim().is_empty() || prompt.len() > 8192 {
                            if let Some(id) = &sample {
                                protocol::send(&mut write, &json!({"kind":"security.sample.finished","scope":scope,"sample_id":id,"status":"error","error":"Another task is already running, or the request is invalid."})).await?;
                            }
                            protocol::send(&mut write, &json!({"kind":"agent.task.rejected","scope":scope,"error":"one active task is allowed; prompt must be 1–8192 bytes"})).await?;
                            continue;
                        }
                        cancelled = false;
                        let start = async {
                            if let Some(id) = sample.as_deref() {
                                protocol::send(&mut write, &json!({"kind":"security.sample.progress","scope":scope,"sample_id":id,"status":"running","phase":"preparing"})).await?;
                                let run = crate::playground::samples::prepare(id, &scope.task_id).await?;
                                protocol::send(&mut write, &json!({"kind":"security.sample.progress","scope":scope,"sample_id":id,"status":"running","phase":"injecting"})).await?;
                                run.inject().await?;
                                prompt = run.prompt.clone();
                                let mut event = run.started();
                                event["kind"] = "security.sample.started".into();
                                event["scope"] = json!(scope);
                                protocol::send(&mut write, &event).await?;
                                sample_run = Some(run);
                            }
                            let mut child = start(adapter, &scope, if sample.is_some() { None } else { conversation.as_deref() }, &prompt).await?;
                            let stdout = child.stdout.take().unwrap();
                            let stderr = child.stderr.take().unwrap();
                            let diagnostics = diagnostics.clone();
                            output = Some(tokio::spawn(async move { tokio::try_join!(adapter.read_output(Box::new(stdout), diagnostics), read_output(stderr)) }));
                            task = Some(child);
                            current = Some(scope.clone());
                            Ok::<_, io::Error>(())
                        }.await;
                        if start.is_err() && let Some(id) = sample {
                            let mut event = match sample_run.take() {
                                Some(run) => run.evaluate(Some(125), false).await,
                                None => json!({"sample_id":id,"status":"error"}),
                            };
                            event["kind"] = "security.sample.finished".into();
                            event["scope"] = json!(scope);
                            event["error"] = start.as_ref().unwrap_err().to_string().into();
                            protocol::send(&mut write, &event).await?;
                        }
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
                        _ => { output.abort(); (super::adapter::RuntimeOutput::default(), String::new()) },
                    },
                    None => (super::adapter::RuntimeOutput::default(), String::new()),
                };
                if sample_run.is_none() && let Some(id) = captured.0.conversation_id { conversation = Some(id); }
                let error = captured.0.error.unwrap_or_else(|| {
                    if captured.1.trim().is_empty() { format!("{} exited unsuccessfully ({code:?})", adapter.metadata().name) }
                    else { captured.1.chars().take(8192).collect::<String>() }
                });
                if let Some(run) = sample_run.take() {
                    protocol::send(&mut write, &json!({"kind":"security.sample.progress","scope":current,"sample_id":run.id(),"status":"running","phase":"verifying"})).await?;
                    let mut event = run.evaluate(code, cancelled).await;
                    event["kind"] = "security.sample.finished".into();
                    event["scope"] = json!(current);
                    protocol::send(&mut write, &event).await?;
                }
                protocol::send(&mut write, &json!({"kind":"agent.task.finished","scope":current,"exit_code":code,"error":if code == Some(0) { Value::Null } else { error.into() }})).await?;
                current = None;
                while notices.try_recv().is_ok() {}
                task = None;
            }
        }
    }
}

pub(super) async fn start(
    adapter: super::Adapter,
    scope: &Scope,
    conversation: Option<&str>,
    prompt: &str,
) -> io::Result<Child> {
    let mut launch = adapter.launch(&super::adapter::LaunchRequest {
        conversation,
        prompt,
    })?;
    let mut child = launch
        .command
        .env("AGENTTOMO_AGENT_ID", &scope.agent_instance_id)
        .env("AGENTTOMO_TASK_ID", &scope.task_id)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&launch.input).await?;
    drop(stdin);
    Ok(child)
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
                    Some("security.sample.started") => "security.sample.started",
                    Some("security.sample.progress") => "security.sample.progress",
                    Some("security.sample.finished") => "security.sample.finished",
                    _ => return Err(io::Error::other("invalid workspace control event")),
                };
                let scope: Scope = serde_json::from_value(data["scope"].take())?;
                if kind == "security.sample.finished" && cancelled { data["status"] = "cancelled".into(); }
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
