#![cfg(target_os = "linux")]

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::timeout,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

struct Server {
    child: tokio::process::Child,
    directory: PathBuf,
    ws_url: String,
}

impl Server {
    async fn stop(&mut self) {
        self.terminate();
        assert!(
            timeout(Duration::from_secs(15), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }

    fn terminate(&self) {
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Even a failed assertion must ask the server to clean up its containers.
        self.terminate();
    }
}

async fn start_server() -> Server {
    let directory = std::env::temp_dir().join(format!(
        "agenttomo-playground-test-{}",
        uuid::Uuid::new_v4()
    ));
    let mut server = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"))
        .args(["playground", "--listen", "127.0.0.1:0", "--output-dir"])
        .arg(&directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let line = timeout(Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("playground did not start");
    let url = line
        .strip_prefix("AgentTomo playground: ")
        .expect("missing access URL");
    let ws_url = url
        .replacen("http://", "ws://", 1)
        .replacen("/?", "/ws?", 1);
    Server {
        child: server,
        directory,
        ws_url,
    }
}

type Connection =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ready(socket: &mut Connection) -> (String, String) {
    timeout(Duration::from_secs(15), async {
        let mut container = String::new();
        let mut id = String::new();
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(value["type"], "error", "{value}");
                if value["type"] == "session" {
                    container = value["container"].as_str().unwrap().into();
                    id = value["id"].as_str().unwrap().into();
                }
                if value["type"] == "ready" {
                    return (container, id);
                }
            }
        }
    })
    .await
    .expect("container shell did not start")
}

async fn removed(container: &str) {
    timeout(Duration::from_secs(10), async {
        loop {
            if !Command::new("docker")
                .args(["inspect", container])
                .output()
                .await
                .unwrap()
                .status
                .success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("container survived session cleanup");
}

#[tokio::test]
#[ignore = "requires Docker access and a local ubuntu:24.04 image"]
async fn docker_terminal_streams_real_traces_and_cleans_up() {
    let mut server = start_server().await;
    let ws_url = &server.ws_url;

    let bad_url = ws_url.split('?').next().unwrap().to_owned() + "?token=wrong";
    let unauthorized = connect_async(bad_url).await.unwrap_err();
    assert!(
        matches!(unauthorized, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 401)
    );
    let mut foreign = ws_url.into_client_request().unwrap();
    foreign
        .headers_mut()
        .insert("origin", "http://unrelated.example".parse().unwrap());
    assert!(
        matches!(connect_async(foreign).await.unwrap_err(), tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 403)
    );

    let (mut socket, _) = connect_async(ws_url).await.unwrap();
    let (container, _) = ready(&mut socket).await;
    socket
        .send(Message::Text(
            json!({"type":"resize","rows":33,"cols":101})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
        .send(Message::Binary(
            b"test -t 0 && printf 'PTY_OK\\n'; stty size; /bin/echo TRACE_VERIFIED\n"
                .to_vec()
                .into(),
        ))
        .await
        .unwrap();
    let mut terminal = Vec::new();
    let mut traced_echo = false;
    timeout(Duration::from_secs(10), async {
        while !traced_echo || !String::from_utf8_lossy(&terminal).contains("33 101\r\n") {
            match socket.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => terminal.extend(bytes),
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text).unwrap();
                    assert_ne!(value["type"], "error", "{value}");
                    let event = &value["event"];
                    traced_echo |= event["kind"] == "process.exec"
                        && event["data"]["argv"][0] == "/bin/echo"
                        && event["data"]["argv"][1] == "TRACE_VERIFIED";
                }
                _ => {}
            }
        }
    })
    .await
    .expect("terminal output or matching trace was missing");
    assert!(String::from_utf8_lossy(&terminal).contains("PTY_OK\r\n"));
    assert!(!container.is_empty());
    socket.close(None).await.unwrap();
    removed(&container).await;
    server.stop().await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and a local ubuntu:24.04 image"]
async fn completion_flushes_descendant_traces_after_shell_exit() {
    let mut server = start_server().await;
    let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut socket).await;
    socket
        .send(Message::Binary(
            b"trap '' HUP; (/bin/sleep 0.3; /bin/echo AFTER_ROOT) & disown; exit 7\n"
                .to_vec()
                .into(),
        ))
        .await
        .unwrap();
    let mut root_exited = false;
    let mut after_root_exec = false;
    let mut last_sequence = 0;
    timeout(Duration::from_secs(15), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(value["type"], "error", "{value}");
                match value["type"].as_str() {
                    Some("root_exit") => {
                        assert_eq!(value["code"], 7);
                        root_exited = true;
                    }
                    Some("trace") => {
                        let event = &value["event"];
                        last_sequence = event["sequence"].as_u64().unwrap();
                        if event["kind"] == "process.exec"
                            && event["data"]["argv"][1] == "AFTER_ROOT"
                        {
                            assert!(root_exited, "late descendant event preceded shell exit");
                            after_root_exec = true;
                        }
                    }
                    Some("complete") => {
                        assert_eq!(value["status"]["state"], "complete");
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("collector did not finish");
    assert!(root_exited && after_root_exec);
    let run = server.directory.join(id).join("run");
    let events = std::fs::read_to_string(run.join("events.jsonl")).unwrap();
    let last: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(
        last_sequence,
        last["sequence"].as_u64().unwrap(),
        "last event was lost before completion"
    );
    assert!(
        std::fs::read_to_string(run.join("stdout.log"))
            .unwrap()
            .contains("AFTER_ROOT\r\n")
    );
    removed(&container).await;
    server.stop().await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and a local ubuntu:24.04 image"]
async fn server_shutdown_removes_an_active_container() {
    let mut server = start_server().await;
    let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut socket).await;
    server.stop().await;
    removed(&container).await;
    let result: Value = serde_json::from_slice(
        &std::fs::read(server.directory.join(id).join("session-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(result["reason"], "server_shutdown");
    assert_eq!(result["container_removed"], true);
    let _ = std::fs::remove_dir_all(&server.directory);
}
