//! Traced parent for the terminal and chat children. Commands never enter the PTY.
mod conversation;
use super::Scope;
use crate::execution::protocol;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, os::unix::fs::PermissionsExt, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_marker: Option<String>,
    },
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<Scope>,
    },
}

pub async fn run() -> io::Result<u8> {
    let adapter = super::Adapter::selected()?
        .ok_or_else(|| io::Error::other("workspace requires an agent adapter"))?;
    let listener = UnixListener::bind(SOCKET)?;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o600))?;
    let parent = nix::unistd::getppid().as_raw();
    let connection = loop {
        let (peer, _) = listener.accept().await?;
        if peer.peer_cred()?.pid() == Some(parent) {
            break peer;
        }
    };
    drop(listener);
    let mut terminal = Command::new("/bin/bash");
    terminal.args(["--noprofile", "--norc", "-i"]);
    adapter.configure(&mut terminal)?;
    let mut terminal = terminal.kill_on_drop(true).spawn()?;
    if adapter.conversation_server() {
        return conversation::serve(adapter, terminal, connection).await;
    }
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
                    Action::Cancel { scope } => {
                        if scope.is_none() || scope.as_ref() == current.as_ref() {
                            cancelled = true;
                            kill_group(task.as_mut());
                        }
                    }
                    Action::Start { scope, mut prompt, sample, report_marker } => {
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
                                let run = prepare_sample(&mut write, &scope, id, report_marker).await?;
                                prompt = run.prompt.clone();
                                sample_run = Some(run);
                            }
                            let mut child = start(adapter, &scope, if sample.is_some() { None } else { conversation.as_deref() }, &prompt, sample.as_deref() == Some("tool-leak")).await?;
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
                    finish_sample(&mut write, current.as_ref().unwrap(), run, code, cancelled).await?;
                }
                protocol::send(&mut write, &json!({"kind":"agent.task.finished","scope":current,"exit_code":code,"error":if code == Some(0) { Value::Null } else { error.into() }})).await?;
                current = None;
                while notices.try_recv().is_ok() {}
                task = None;
            }
        }
    }
}

async fn prepare_sample(
    writer: &mut (impl AsyncWrite + Unpin),
    scope: &Scope,
    id: &str,
    marker: Option<String>,
) -> io::Result<crate::playground::samples::Run> {
    protocol::send(writer, &json!({"kind":"security.sample.progress","scope":scope,"sample_id":id,"status":"running","phase":"preparing"})).await?;
    let mut run = crate::playground::samples::prepare(id, &scope.task_id, marker).await?;
    protocol::send(writer, &json!({"kind":"security.sample.progress","scope":scope,"sample_id":id,"status":"running","phase":"injecting"})).await?;
    run.inject().await?;
    let mut event = run.verify_injection().await?;
    event["kind"] = "security.sample.started".into();
    event["scope"] = json!(scope);
    protocol::send(writer, &event).await?;
    Ok(run)
}

async fn finish_sample(
    writer: &mut (impl AsyncWrite + Unpin),
    scope: &Scope,
    run: crate::playground::samples::Run,
    code: Option<i32>,
    cancelled: bool,
) -> io::Result<()> {
    protocol::send(writer, &json!({"kind":"security.sample.progress","scope":scope,"sample_id":run.id(),"status":"running","phase":"verifying"})).await?;
    let mut event = run.evaluate(code, cancelled).await;
    event["kind"] = "security.sample.finished".into();
    event["scope"] = json!(scope);
    protocol::send(writer, &event).await
}

pub(super) async fn start(
    adapter: super::Adapter,
    scope: &Scope,
    conversation: Option<&str>,
    prompt: &str,
    observe_report: bool,
) -> io::Result<Child> {
    let mut launch = adapter.launch(&super::adapter::LaunchRequest {
        conversation,
        prompt,
    })?;
    launch
        .command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn(&mut launch.command, scope, observe_report)?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&launch.input).await?;
    drop(stdin);
    Ok(child)
}

fn spawn(command: &mut Command, scope: &Scope, observe_report: bool) -> io::Result<Child> {
    if observe_report {
        let filter = crate::syscall::prepare(crate::collectors::file::report::EXTRA_SYSCALLS);
        // The prepared hook is allocation-free between fork and exec.
        unsafe {
            command.pre_exec(filter);
        }
    }
    command
        .env("AGENTTOMO_AGENT_ID", &scope.agent_instance_id)
        .env("AGENTTOMO_TASK_ID", &scope.task_id)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
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
    reports: crate::collectors::file::report::Watch,
) -> io::Result<()> {
    let stream = connect().await?;
    let (read, mut write) = stream.into_split();
    let mut read = protocol::Reader::new(BufReader::new(read));
    let conversation =
        super::Adapter::selected()?.is_some_and(|adapter| adapter.conversation_server());
    let mut running = None;
    let mut started = false;
    let mut cancelled = false;
    loop {
        tokio::select! {
            action = actions.recv() => match action {
                Some(mut action) => {
                    // The collector owns the turn binding. A second start must not cross
                    // the completion acknowledgement while the subprocess is reusable.
                    if let Action::Start { scope, sample, .. } = &action && let Some(active) = &running {
                        if active != scope {
                            if let Some(id) = sample {
                                events.send(crate::recording::Observation { source:"agent_launcher", kind:"security.sample.finished",
                                    data:json!({"agent_instance_id":scope.agent_instance_id,"task_id":scope.task_id,"sample_id":id,"status":"error","error":"Another task is already running.","captured_unix_ns":super::now()}) }).await.map_err(io::Error::other)?;
                            }
                            events.send(crate::recording::Observation {
                                source: "agent_launcher", kind: "agent.task.rejected",
                                data: json!({"agent_instance_id":scope.agent_instance_id,"task_id":scope.task_id,
                                    "error":"Another task is already running.","captured_unix_ns":super::now()}),
                            }).await.map_err(io::Error::other)?;
                        }
                        continue;
                    }
                    if matches!(action, Action::Cancel { .. }) && running.is_none() { continue; }
                    if let Action::Start { scope, sample, report_marker, .. } = &mut action && running.is_none() {
                        running = Some(scope.clone());
                        agents.begin_turn(scope, conversation && sample.is_none());
                        cancelled = false;
                        if sample.as_deref() == Some("tool-leak") {
                            // Arm the tracer before sending Start so it can match the first write.
                            let marker = crate::playground::samples::new_marker();
                            let root = crate::playground::samples::directory(std::path::Path::new("/home"), &scope.task_id);
                            reports.arm(scope, root.join("public/report.json"), marker.clone());
                            *report_marker = Some(marker);
                        }
                    }
                    if let Action::Cancel { scope } = &mut action
                        && let Some(active) = &running
                    {
                        *scope = Some(active.clone());
                        cancelled = true;
                        if let Err(error) = agents.cancel(active) {
                            events.send(crate::recording::Observation { source:"agent_launcher", kind:"agent.capture.error", data:json!({"task_id":active.task_id,"agent_instance_id":active.agent_instance_id,"error":format!("some task processes could not be stopped and remain traced: {error}"),"captured_unix_ns":super::now()}) }).await.map_err(io::Error::other)?;
                        }
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
                if kind == "security.sample.finished" && cancelled && running.as_ref() == Some(&scope) { data["status"] = "cancelled".into(); }
                if kind == "security.sample.finished" && data["sample_id"] == "tool-leak"
                    && data.get("exit_code").is_some() && let Some(evidence) = reports.finish(&scope) {
                    crate::playground::samples::apply_report_evidence(&mut data,evidence);
                }
                if kind == "agent.task.started" {
                    running = Some(scope.clone());
                    started = true;
                }
                if kind == "agent.task.rejected" && !started && running.as_ref() == Some(&scope) {
                    agents.finish_turn(&scope);
                    data["cancelled"] = cancelled.into();
                    reports.finish(&scope);
                    running = None;
                    cancelled = false;
                }
                if kind == "agent.task.finished" && running.as_ref() == Some(&scope) {
                    agents.finish_turn(&scope);
                    started = false;
                    data["cancelled"] = cancelled.into();
                    if cancelled { data["error"] = Value::Null; }
                    if cancelled && conversation {
                        data["status"] = "cancelled".into();
                    }
                    running = None;
                    cancelled = false;
                }
                data["agent_instance_id"] = scope.agent_instance_id.into();
                data["task_id"] = scope.task_id.into();
                data["captured_unix_ns"] = json!(super::now());
                events.send(crate::recording::Observation { source:"agent_launcher", kind, data }).await.map_err(io::Error::other)?;
            }
        }
    }
}
