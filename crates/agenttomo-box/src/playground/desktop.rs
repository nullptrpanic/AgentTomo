//! Session-scoped display transport. Chrome itself starts inside the traced shell.
use std::{io, process::Stdio, time::Duration};

use axum::{
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{Access, App, same_origin};

pub async fn start(container: &str) -> io::Result<Child> {
    let mut child = Command::new("docker")
        .args(["exec", container, "/usr/local/bin/agenttomo-desktop"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut marker = [0; 6];
    let result = timeout(
        Duration::from_secs(25),
        child.stdout.as_mut().unwrap().read_exact(&mut marker),
    )
    .await;
    if matches!(result, Ok(Ok(_))) && &marker == b"READY\n" {
        return Ok(child);
    }
    let _ = child.kill().await;
    let mut error = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(4096).read_to_string(&mut error).await;
    }
    Err(io::Error::other(format!(
        "cannot start desktop: {}; --browser requires the agenttomo-playground-chrome image",
        if error.trim().is_empty() {
            "display did not become ready"
        } else {
            error.trim()
        }
    )))
}

pub async fn websocket(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(access): Query<Access>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let lifetime = app
        .desktops
        .lock()
        .expect("desktop registry")
        .get(&id)
        .cloned();
    let Some(lifetime) = lifetime.filter(|lifetime| !lifetime.is_cancelled()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tracked = app.sessions.token();
    upgrade
        .max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let _tracked = tracked;
            if let Err(error) = bridge(socket, &format!("agenttomo-{id}"), lifetime).await {
                eprintln!("agenttomo-box: desktop {id}: {error}");
            }
        })
}

async fn bridge(socket: WebSocket, container: &str, lifetime: CancellationToken) -> io::Result<()> {
    if lifetime.is_cancelled() {
        return Ok(());
    }
    // No published VNC port. nc transports raw RFB bytes; this server owns the
    // WebSocket framing, access checks, backpressure and cancellation.
    let mut child = Command::new("docker")
        .args([
            "exec",
            "--interactive",
            container,
            "nc",
            "-N",
            "127.0.0.1",
            "5900",
        ])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let (mut sink, mut stream) = socket.split();
    let result = {
        let to_desktop = async {
            while let Some(message) = stream.next().await {
                match message.map_err(io::Error::other)? {
                    Message::Binary(bytes) => input.write_all(&bytes).await?,
                    Message::Close(_) => break,
                    Message::Text(_) => {
                        return Err(io::Error::other("desktop expects binary RFB data"));
                    }
                    _ => {}
                }
            }
            Ok(())
        };
        let to_browser = async {
            let mut bytes = [0; 32 * 1024];
            loop {
                let count = output.read(&mut bytes).await?;
                if count == 0 {
                    return Ok(());
                }
                sink.send(Message::Binary(bytes[..count].to_vec().into()))
                    .await
                    .map_err(io::Error::other)?;
            }
        };
        tokio::select! {
            result = to_desktop => result,
            result = to_browser => result,
            _ = lifetime.cancelled() => Ok(()),
        }
    };
    drop(input);
    // Let Docker propagate stdin EOF and nc shut down its socket before
    // terminating the CLI; otherwise reconnects leave remote nc processes.
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    let _ = timeout(Duration::from_secs(1), sink.send(Message::Close(None))).await;
    result
}
