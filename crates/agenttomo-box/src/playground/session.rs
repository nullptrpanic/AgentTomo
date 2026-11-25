//! One browser connection owns one container, its IPC streams and trace tail.

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use super::{App, bridge, docker, sandbox, trace_feed};
use crate::execution::protocol::{self, Input, Message};

pub enum Frame {
    Bytes(Vec<u8>),
    Json(Value),
    Complete(Value),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { rows: u16, cols: u16 },
    AgentPrompt { task_id: String, prompt: String },
    SampleRun { task_id: String, sample_id: String },
    AgentCancel,
}

pub async fn serve(
    mut socket: WebSocket,
    app: App,
    installation: Option<Arc<dyn crate::agent::Installation>>,
) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("agenttomo-{id}");
    app.running
        .lock()
        .expect("session registry")
        .insert(id.clone(), None);
    let mut create_attempted = false;
    let result = async {
        let configuration = match &installation {
            Some(agent) => {
                let prepared = agent.snapshot().await?;
                app.settings.credential_gateway.as_ref().ok_or_else(|| io::Error::other("host credential gateway is unavailable"))?.register(name.clone(), prepared.policy);
                prepared.files
            },
            None => Default::default(),
        };
        let agent = installation.as_ref().map(|installation| installation.adapter());
        let metadata = json!({"session_id":id,"container":name,"image":app.settings.image,"http_capture":app.settings.http.enabled,"agent_available":agent.is_some(),"agent":agent.map(|adapter| adapter.metadata())});
        let bootstrap = sandbox::Bootstrap {
            metadata,
            agent,
            configuration,
            proxy: app.settings.proxy.clone(),
            proxy_ca: app.settings.credential_gateway.as_ref().map(|gateway| gateway.certificate()),
        };
        socket
            .send(WsMessage::Text(
                json!({"type":"session","id":id,"container":name,"image":app.settings.image,"http_capture":app.settings.http.enabled,"agent_available":agent.is_some(),"agent":agent.map(|adapter| adapter.metadata())})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(io::Error::other)?;
        if app.shutdown.is_cancelled() {
            return Ok("server_shutdown");
        }
        // Do not cancel create on disconnect: obtain ownership first, then the
        // common cleanup below removes the known container even if start fails.
        create_attempted = true;
        docker::create(docker::Create {
            name: &name,
            image: &app.settings.image_id,
            binary: &app.settings.binary,
            tmpfs_size_mib: app.settings.session_tmpfs_size_mib,
            preview_limits: app.settings.file_preview,
            http: app.settings.http,
            rootfs: &app.settings.rootfs,
            agent: installation.as_deref(),
            agent_id: &id,
        })
        .await?;
        if app.shutdown.is_cancelled() {
            return Ok("server_shutdown");
        }
        drive(&mut socket, &id, &name, bootstrap, &app).await
    }
    .await;

    if let Some(gateway) = &app.settings.credential_gateway {
        gateway.remove(&name);
    }
    app.proxy_peers.remove(&name);
    if let Some(client) = app
        .bridges
        .as_ref()
        .unwrap()
        .lock()
        .expect("read channel registry")
        .remove(&id)
    {
        app.remember_ended_viewer(id.clone(), client.viewer().to_owned());
    }
    let cleanup = if create_attempted {
        docker::remove(&name).await
    } else {
        Ok(())
    };
    let errors: Vec<_> = [result.err(), cleanup.err()]
        .into_iter()
        .flatten()
        .map(|error| error.to_string())
        .collect();
    if !errors.is_empty() {
        let error = errors.join("; ");
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(WsMessage::Text(
                json!({"type":"error","message":error}).to_string().into(),
            )),
        )
        .await;
        eprintln!("agenttomo-box: session {id}: {error}");
    }
    app.running.lock().expect("session registry").remove(&id);
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.send(WsMessage::Close(None))).await;
}

async fn drive(
    socket: &mut WebSocket,
    id: &str,
    name: &str,
    bootstrap: sandbox::Bootstrap,
    app: &App,
) -> io::Result<&'static str> {
    let shutdown = &app.shutdown;
    let mut child = docker::start(name)?;
    let mut input = child.stdin.take().expect("piped Docker stdin");
    let mut output = BufReader::new(child.stdout.take().expect("piped Docker stdout"));
    let stderr = child.stderr.take().expect("piped Docker stderr");
    let diagnostics = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(read_stderr(stderr)));
    let diagnostics = async { diagnostics.await.map_err(io::Error::other)? };
    let agent_available = bootstrap.agent.is_some();
    let bootstrap_sent = async {
        // The task cannot run until its bootstrap is delivered.
        if app.settings.proxy.is_some() {
            app.proxy_peers.register(name).await?;
        }
        sandbox::send(&mut input, &bootstrap).await
    };
    let initialized = tokio::select! {
        status = child.wait() => Err(io::Error::other(format!("container exited with {}", status?))),
        result = tokio::time::timeout(Duration::from_secs(30), bootstrap_sent) => {
            result.unwrap_or_else(|_| Err(io::Error::other("sandbox configuration timed out")))
        }
        _ = shutdown.cancelled() => return Ok("server_shutdown"),
    };
    drop(bootstrap);
    if let Err(error) = initialized {
        drop(input);
        // A failed `docker start` can close stdin before we consume its error.
        // Preserve that diagnostic instead of reporting only "Broken pipe".
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
        let detail = tokio::time::timeout(Duration::from_secs(2), diagnostics)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        return Err(io::Error::other(format!(
            "cannot initialize sandbox: {error}; {}",
            detail.trim()
        )));
    }
    let (connected, connection) = tokio::sync::oneshot::channel();
    let (outbound, mut frames) = mpsc::channel(32);
    let (incoming, mut inputs) = mpsc::channel(32);
    let (mut sink, mut stream) = socket.split();

    let source = async {
        let worker = async {
            let mut connected = Some(connected);
            loop {
                let message = match protocol::receive(&mut output).await {
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    result => result?,
                };
                let frame = match message {
                    Message::Started { pid } => {
                        let journal = bridge::Client::connect(name, id).await?;
                        // Open a separate monitor connection on first use; a slow
                        // payload download must not hold the terminal's journal cursor.
                        let monitor = bridge::Client::new(name, id);
                        let viewer = monitor.viewer().to_owned();
                        app.bridges
                            .as_ref()
                            .unwrap()
                            .lock()
                            .expect("read channel registry")
                            .insert(id.to_owned(), monitor);
                        if let Some(sender) = connected.take() {
                            let _ = sender.send(journal);
                        }

                        app.running
                            .lock()
                            .expect("session registry")
                            .insert(id.to_owned(), Some(pid));
                        Frame::Json(json!({"type":"ready","pid":pid,"monitor_token":viewer}))
                    }
                    Message::Stdout(bytes) | Message::Stderr(bytes) => Frame::Bytes(bytes),
                    Message::InputAck => Frame::Json(json!({"type":"input_ack"})),
                    Message::RootExit(code) => Frame::Json(json!({"type":"root_exit","code":code})),
                    Message::Error(error) => return Err(io::Error::other(error)),
                };
                outbound.send(frame).await.map_err(io::Error::other)?;
            }
            let status = child.wait().await?;
            Ok::<_, io::Error>(status)
        };
        let traces = async {
            let Ok(client) = connection.await else {
                return std::future::pending().await;
            };
            trace_feed::follow(client, id, outbound.clone()).await?;
            // The WebSocket writer owns successful delivery of the Complete frame.
            std::future::pending::<io::Result<&'static str>>().await
        };
        tokio::select! {
            result = async {
                let (status, detail) = tokio::try_join!(worker, diagnostics)?;
                Err(io::Error::other(format!("container collector exited with {status}: {}", detail.trim())))
            } => result,
            result = traces => result,
        }
    };
    let receive = async {
        while let Some(message) = stream.next().await {
            let message = message.map_err(io::Error::other)?;
            let input = match message {
                WsMessage::Close(_) => return Ok("browser_disconnected"),
                WsMessage::Binary(bytes) => {
                    if bytes.len() > 8192 {
                        return Err(io::Error::other("terminal input frame exceeds 8192 bytes"));
                    }
                    Input::Bytes(bytes.to_vec())
                }
                WsMessage::Text(text) => match serde_json::from_str::<Control>(&text) {
                    Err(_) => {
                        outbound.send(Frame::Json(json!({"type":"control_error","message":"Invalid control message."}))).await.map_err(io::Error::other)?;
                        continue;
                    }
                    Ok(control) => match control {
                        Control::AgentPrompt { task_id, prompt }
                            if agent_available
                                && task_id.len() == 32
                                && task_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && !prompt.trim().is_empty()
                                && prompt.len() <= 8192 =>
                        {
                            Input::AgentPrompt {
                                task_id,
                                prompt,
                                sample: None,
                            }
                        }
                        Control::SampleRun { task_id, sample_id }
                            if agent_available
                                && task_id.len() == 32
                                && task_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && super::samples::contains(&sample_id) =>
                        {
                            Input::AgentPrompt {
                                task_id,
                                prompt: String::new(),
                                sample: Some(sample_id),
                            }
                        }
                        Control::AgentCancel if agent_available => Input::AgentCancel,
                        Control::SampleRun { .. } if !agent_available => continue,
                        Control::Resize { rows, cols }
                            if (1..=1000).contains(&rows) && (1..=1000).contains(&cols) =>
                        {
                            Input::Resize { rows, cols }
                        }
                        _ => {
                            outbound.send(Frame::Json(json!({"type":"control_error","message":"Invalid control parameters or unavailable agent."}))).await.map_err(io::Error::other)?;
                            continue;
                        }
                    },
                },
                _ => continue,
            };
            incoming
                .send(input)
                .await
                .map_err(|_| io::Error::other("terminal input channel closed"))?;
        }
        Ok("browser_disconnected")
    };
    let write_input = async {
        while let Some(message) = inputs.recv().await {
            if let Err(error) = protocol::send(&mut input, &message).await {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    return std::future::pending::<io::Result<&'static str>>().await;
                }
                return Err(error);
            }
        }
        std::future::pending::<io::Result<&'static str>>().await
    };
    let send_output = async {
        while let Some(frame) = frames.recv().await {
            let complete = matches!(frame, Frame::Complete(_));
            let message = match frame {
                Frame::Bytes(bytes) => WsMessage::Binary(bytes.into()),
                Frame::Json(value) => WsMessage::Text(value.to_string().into()),
                Frame::Complete(status) => WsMessage::Text(
                    json!({"type":"complete","status":status})
                        .to_string()
                        .into(),
                ),
            };
            sink.send(message).await.map_err(io::Error::other)?;
            if complete {
                return Ok("completed");
            }
        }
        Err(io::Error::other("session output channel closed"))
    };
    tokio::select! {
        result = source => result,
        result = receive => result,
        result = write_input => result,
        result = send_output => result,
        _ = shutdown.cancelled() => Ok("server_shutdown"),
    }
}

async fn read_stderr(mut stream: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut captured = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min((64 * 1024usize).saturating_sub(captured.len()));
        captured.extend_from_slice(&buffer[..keep]);
    }
    Ok(String::from_utf8_lossy(&captured).into_owned())
}
