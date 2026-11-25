//! One traced AgentKit server and one SDK session live across turns in a sandbox.
use super::{Action, Scope, finish_sample, prepare_sample, spawn};
use crate::{
    agent::{Adapter, adapter::LaunchRequest, agentkit::http::Session},
    execution::protocol,
};
use serde_json::json;
use std::{
    future::pending,
    io,
    net::{Ipv4Addr, TcpListener},
    process::Stdio,
};
use tokio::{io::BufReader, net::UnixStream, process::Child, task::JoinSet};

struct Process {
    child: Child,
    group: nix::unistd::Pid,
    session: Session,
}

impl Process {
    fn start(adapter: Adapter, scope: &Scope, sample: Option<&str>) -> io::Result<Self> {
        let address = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?;
        let mut launch = adapter.launch(&LaunchRequest {
            conversation: None,
            prompt: "",
        })?;
        launch
            .command
            .args(["-host", "127.0.0.1"])
            .arg("-port")
            .arg(address.port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if sample.is_none() {
            launch
                .command
                .env("AGENTTOMO_CONVERSATION_ID", &scope.task_id);
        }
        let child = spawn(&mut launch.command, scope, sample == Some("tool-leak"))?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("AgentKit did not start"))?;
        Ok(Self {
            child,
            group: nix::unistd::Pid::from_raw(pid as i32),
            session: Session::new(address, pid),
        })
    }
}

async fn stop(process: &mut Option<Process>) {
    if let Some(mut process) = process.take() {
        let _ = nix::sys::signal::killpg(process.group, nix::sys::signal::Signal::SIGKILL);
        let _ = process.child.wait().await;
    }
}

pub(super) async fn serve(
    adapter: Adapter,
    mut terminal: Child,
    connection: UnixStream,
) -> io::Result<u8> {
    let (input, mut writer) = connection.into_split();
    let mut control = protocol::Reader::new(BufReader::new(input));
    let mut process: Option<Process> = None;
    let mut saved_chat = None;
    let mut sample_run = None;
    let mut active: Option<Scope> = None;
    let mut requests = JoinSet::new();
    let mut reset = false;
    loop {
        let result = tokio::select! {
            status = terminal.wait() => {
                stop(&mut process).await;
                stop(&mut saved_chat).await;
                return Ok(status?.code().unwrap_or(125) as u8);
            }
            action = control.receive::<Action>() => {
                match action? {
                    Action::Start { scope, mut prompt, sample, report_marker } => {
                        if active.is_some() || sample.is_none() && prompt.trim().is_empty() || prompt.len() > 8192 {
                            if let Some(id) = &sample {
                                protocol::send(&mut writer, &json!({"kind":"security.sample.finished","scope":scope,
                                    "sample_id":id,"status":"error","error":"Another task is already running, or the request is invalid."})).await?;
                            }
                            protocol::send(&mut writer, &json!({
                                "kind":"agent.task.rejected", "scope":scope,
                                "error":"one active task is allowed; prompt must be 1–8192 bytes"
                            })).await?;
                            continue;
                        }
                        let launch = async {
                            if let Some(id) = &sample {
                                let run = prepare_sample(&mut writer, &scope, id, report_marker).await?;
                                prompt = run.prompt.clone();
                                sample_run = Some(run);
                                saved_chat = process.take();
                            }
                            let first = process.is_none();
                            if first {
                                process = Some(Process::start(adapter, &scope, sample.as_deref())?);
                            }
                            Ok::<_, io::Error>(first)
                        }.await;
                        let first = match launch {
                            Ok(first) => first,
                            Err(error) => {
                                if sample_run.take().is_some() {
                                    stop(&mut process).await;
                                    process = saved_chat.take();
                                }
                                if let Some(id) = &sample {
                                    protocol::send(&mut writer, &json!({"kind":"security.sample.finished","scope":scope,
                                        "sample_id":id,"status":"error","error":error.to_string()})).await?;
                                }
                                protocol::send(&mut writer, &json!({"kind":"agent.task.rejected", "scope":scope,
                                    "error":error.to_string(), "context_reset":reset})).await?;
                                continue;
                            }
                        };
                        let session = process.as_ref().unwrap().session.clone();
                        protocol::send(&mut writer, &json!({
                            "kind":"agent.task.started", "scope":scope, "prompt":prompt, "context_reset":reset && sample.is_none()
                        })).await?;
                        if sample.is_none() { reset = false; }
                        active = Some(scope);
                        requests.spawn(async move { session.run(&prompt, first).await });
                    }
                    Action::Cancel => {
                        requests.shutdown().await;
                        stop(&mut process).await;
                        let sample = sample_run.take();
                        let was_sample = sample.is_some();
                        if was_sample { process = saved_chat.take(); }
                        else { reset = true; }
                        if let Some(scope) = active.take() {
                            if let Some(run) = sample {
                                finish_sample(&mut writer, &scope, run, None, true).await?;
                            }
                            protocol::send(&mut writer, &json!({
                                "kind":"agent.task.finished", "scope":scope, "status":"cancelled", "context_reset":!was_sample
                            })).await?;
                        }
                    }
                }
                continue;
            }
            Some(reply) = requests.join_next(), if !requests.is_empty() => reply.map_err(io::Error::other).and_then(|r| r),
            status = async {
                match process.as_mut() {
                    Some(server) => server.child.wait().await,
                    None => pending().await,
                }
            } => Err(io::Error::other(format!("AgentKit server exited ({})", status?))),
        };
        let error = result.err().map(|e| e.to_string());
        let was_sample = sample_run.is_some();
        if error.is_some() || was_sample {
            requests.shutdown().await;
            stop(&mut process).await;
            if was_sample {
                process = saved_chat.take();
            } else {
                reset = true;
            }
        }
        if let Some(scope) = active.take() {
            if let Some(run) = sample_run.take() {
                finish_sample(
                    &mut writer,
                    &scope,
                    run,
                    Some(if error.is_some() { 1 } else { 0 }),
                    false,
                )
                .await?;
            }
            let status = if error.is_some() {
                "failed"
            } else {
                "completed"
            };
            protocol::send(&mut writer, &json!({
                "kind":"agent.task.finished", "scope":scope, "status":status, "error":error, "context_reset":reset && !was_sample
            })).await?;
        }
    }
}
