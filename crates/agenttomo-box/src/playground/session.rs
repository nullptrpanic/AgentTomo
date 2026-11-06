//! One browser connection owns one container, its IPC streams and trace tail.

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{App, desktop, docker, trace_feed};
use crate::execution::protocol::{self, Input, Message};
use crate::recording::create_log;

pub enum Frame {
    Bytes(Vec<u8>),
    Json(Value),
    Complete(Value),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { rows: u16, cols: u16 },
}

pub async fn serve(mut socket: WebSocket, app: App) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("agenttomo-{id}");
    let directory = app.settings.output_dir.join(&id);
    let result = async {
        tokio::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .await?;
        let metadata = json!({"session_id":id,"container":name,"image":app.settings.image,"browser":app.settings.browser});
        let mut file = create_log(&directory, "session.json").await?;
        file.write_all(&serde_json::to_vec(&metadata)?).await?;
        file.flush().await?;
        socket
            .send(WsMessage::Text(
                json!({"type":"session","id":id,"container":name,"image":app.settings.image,"http_capture":app.settings.http.enabled,"browser":app.settings.browser})
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
        docker::create(
            &name,
            &app.settings.image,
            &app.settings.binary,
            &directory,
            app.settings.file_preview,
            app.settings.http,
            app.settings.browser,
        )
        .await?;
        if app.shutdown.is_cancelled() {
            return Ok("server_shutdown");
        }
        drive(&mut socket, &id, &name, directory.join("run"), &app).await
    }
    .await;

    if let Some(lifetime) = app.desktops.lock().expect("desktop registry").remove(&id) {
        lifetime.cancel();
    }
    let cleanup = docker::remove(&name).await;
    let error = result
        .as_ref()
        .err()
        .or_else(|| cleanup.as_ref().err())
        .map(ToString::to_string);
    if let Some(error) = &error {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(WsMessage::Text(
                json!({"type":"error","message":error}).to_string().into(),
            )),
        )
        .await;
        eprintln!("agenttomo-box: session {id}: {error}");
    }
    if let Ok(mut file) = create_log(&directory, "session-result.json").await {
        let result = json!({"reason":result.as_ref().ok(), "error":error, "container_removed":cleanup.is_ok()});
        let _ = file.write_all(result.to_string().as_bytes()).await;
        let _ = file.flush().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.send(WsMessage::Close(None))).await;
}

async fn drive(
    socket: &mut WebSocket,
    id: &str,
    name: &str,
    run_directory: std::path::PathBuf,
    app: &App,
) -> io::Result<&'static str> {
    let shutdown = &app.shutdown;
    let mut child = docker::start(name)?;
    let mut input = child.stdin.take().expect("piped Docker stdin");
    let mut output = BufReader::new(child.stdout.take().expect("piped Docker stdout"));
    let stderr = child.stderr.take().expect("piped Docker stderr");
    let (outbound, mut frames) = mpsc::channel(32);
    let (incoming, mut inputs) = mpsc::channel(32);
    let finished = CancellationToken::new();
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
            finished.cancel();
            Ok::<_, io::Error>(status)
        };
        let traces = trace_feed::follow(
            run_directory.join("events.jsonl"),
            finished.clone(),
            outbound.clone(),
        );
        let (status, (), stderr) = tokio::try_join!(worker, traces, read_stderr(stderr))?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "container collector exited with {status}: {stderr}"
            )));
        }
        let status: Value = serde_json::from_slice(
            &tokio::fs::read(run_directory.join("collection-status.json")).await?,
        )?;
        if status["state"] != "complete" {
            return Err(io::Error::other(format!("incomplete collection: {status}")));
        }
        outbound
            .send(Frame::Complete(status))
            .await
            .map_err(io::Error::other)?;
        // Let the output future flush all queued frames and the completion frame.
        std::future::pending::<io::Result<&'static str>>().await
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
