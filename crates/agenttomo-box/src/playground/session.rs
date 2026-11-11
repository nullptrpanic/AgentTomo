//! One browser connection owns one container, its IPC streams and trace tail.

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use super::{App, bridge, desktop, docker, sandbox, trace_feed};
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
    AgentCancel,
}

pub async fn serve(mut socket: WebSocket, app: App) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("agenttomo-{id}");
    app.running
        .lock()
        .expect("session registry")
        .insert(id.clone(), None);
    let result = async {
        let configuration = match &app.settings.codex {
            Some(codex) => {
                let prepared = codex.snapshot().await?;
                app.settings.credential_gateway.as_ref().ok_or_else(|| io::Error::other("host credential gateway is unavailable"))?.register(name.clone(), prepared.policy);
                prepared.files
            },
            None => Default::default(),
        };
        let metadata = json!({"session_id":id,"container":name,"image":app.settings.image,"browser":app.settings.browser,"http_capture":app.settings.http.enabled,"agent_available":app.settings.codex.is_some()});
        let bootstrap = sandbox::Bootstrap {
            metadata,
            configuration,
            proxy: app.settings.proxy.clone(),
            proxy_ca: app.settings.credential_gateway.as_ref().map(|gateway| gateway.certificate()),
        };
        socket
            .send(WsMessage::Text(
                json!({"type":"session","id":id,"container":name,"image":app.settings.image,"http_capture":app.settings.http.enabled,"browser":app.settings.browser,"agent_available":app.settings.codex.is_some()})
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
        docker::create(docker::Create {
            name: &name,
            image: &app.settings.image_id,
            binary: &app.settings.binary,
            tmpfs_size_mib: app.settings.session_tmpfs_size_mib,
            preview_limits: app.settings.file_preview,
            http: app.settings.http,
            browser: app.settings.browser,
            rootfs: &app.settings.rootfs,
            codex: app.settings.codex.as_ref(),
            agent_id: &id,
        })
        .await?;
        if app.shutdown.is_cancelled() {
            return Ok("server_shutdown");
        }
        drive(&mut socket, &id, &name, bootstrap, &app).await
    }
    .await;

    if let Some(lifetime) = app.desktops.lock().expect("desktop registry").remove(&id) {
        lifetime.cancel();
    }
    if let Some(gateway) = &app.settings.credential_gateway {
        gateway.remove(&name);
    }
    let cleanup = docker::remove(&name).await;
    app.proxy_peers.remove(&name);
    app.bridges
        .as_ref()
        .unwrap()
        .lock()
        .expect("read channel registry")
        .remove(&id);
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
    let bootstrap_sent = sandbox::send(&mut input, &bootstrap);
    let initialized = tokio::time::timeout(Duration::from_secs(30), bootstrap_sent)
        .await
        .map_err(|_| io::Error::other("sandbox configuration timed out"))
        .and_then(|result| result);
    drop(bootstrap);
    if let Err(error) = initialized {
        // A failed `docker start` can close stdin before we consume its error.
        // Preserve that diagnostic instead of reporting only "Broken pipe".
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
        let detail = tokio::time::timeout(Duration::from_secs(2), read_stderr(stderr))
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
    let mut connected = Some(connected);
    let (outbound, mut frames) = mpsc::channel(32);
    let (incoming, mut inputs) = mpsc::channel(32);
    let (mut sink, mut stream) = socket.split();

    let source = async {
        let worker = async {
            let mut desktop: Option<tokio::process::Child> = None;
            loop {
                let result = tokio::select! {
                    result = protocol::receive(&mut output) => result,
                    status = async {
                        match &mut desktop {
                            Some(child) => child.wait().await,
                            None => std::future::pending().await,
                        }
                    } => return Err(io::Error::other(format!("desktop exited unexpectedly: {}", status?))),
                };
                let message = match result {
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    result => result?,
                };
                let frame = match message {
                    Message::Started { pid } => {
                        let client = bridge::Client::connect(name, id, &app.settings.token).await?;
                        app.bridges
                            .as_ref()
                            .unwrap()
                            .lock()
                            .expect("read channel registry")
                            .insert(id.to_owned(), client.clone());
                        if let Some(sender) = connected.take() {
                            let _ = sender.send(client);
                        }

                        if app.settings.proxy.is_some() {
                            app.proxy_peers.register(name).await?;
                        }
                        app.running
                            .lock()
                            .expect("session registry")
                            .insert(id.to_owned(), Some(pid));
                        if app.settings.browser {
                            desktop = Some(desktop::start(name).await?);
                            app.desktops
                                .lock()
                                .expect("desktop registry")
                                .insert(id.to_owned(), app.shutdown.child_token());
                        }
                        Frame::Json(json!({"type":"ready","pid":pid}))
                    }
                    Message::Stdout(bytes) | Message::Stderr(bytes) => Frame::Bytes(bytes),
                    Message::RootExit(code) => Frame::Json(json!({"type":"root_exit","code":code})),
                    Message::Error(error) => return Err(io::Error::other(error)),
                };
                outbound.send(frame).await.map_err(io::Error::other)?;
            }
            let status = child.wait().await?;
            Ok::<_, io::Error>(status)
        };
        let traces = async {
            let client = connection.await.map_err(io::Error::other)?;
            trace_feed::follow(client, id, &app.settings.token, outbound.clone()).await
        };
        let (status, (), stderr) = tokio::try_join!(worker, traces, read_stderr(stderr))?;
        Err::<&'static str, _>(io::Error::other(format!(
            "container collector exited with {status}: {stderr}"
        )))
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
                WsMessage::Text(text) => match serde_json::from_str::<Control>(&text)? {
                    Control::AgentPrompt { task_id, prompt }
                        if app.settings.codex.is_some()
                            && task_id.len() == 32
                            && task_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && !prompt.trim().is_empty()
                            && prompt.len() <= 8192 =>
                    {
                        Input::AgentPrompt { task_id, prompt }
                    }
                    Control::AgentCancel if app.settings.codex.is_some() => Input::AgentCancel,
                    Control::Resize { rows, cols }
                        if (1..=1000).contains(&rows) && (1..=1000).contains(&cols) =>
                    {
                        Input::Resize { rows, cols }
                    }
                    _ => return Err(io::Error::other("invalid terminal dimensions")),
                },
                _ => continue,
            };
            // Browser input uses one acknowledged chunk at a time. Keep the
            // reader responsive to Close even if the target stops reading stdin.
            incoming
                .try_send(input)
                .map_err(|_| io::Error::other("terminal input queue is full"))?;
        }
        Ok("browser_disconnected")
    };
    let write_input = async {
        while let Some(message) = inputs.recv().await {
            let acknowledge = matches!(message, Input::Bytes(_));
            if let Err(error) = protocol::send(&mut input, &message).await {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    return std::future::pending::<io::Result<&'static str>>().await;
                }
                return Err(error);
            }
            if acknowledge {
                outbound
                    .send(Frame::Json(json!({"type":"input_ack"})))
                    .await
                    .map_err(io::Error::other)?;
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
