#![cfg(target_os = "linux")]

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
    start_server_with_docker_wrapper(None).await
}

async fn start_server_with_docker_wrapper(wrapper: Option<&str>) -> Server {
    start_server_with_options(wrapper, &["--image", "agenttomo-playground:24.04"]).await
}

async fn start_server_with_options(wrapper: Option<&str>, options: &[&str]) -> Server {
    let directory = std::env::temp_dir().join(format!(
        "agenttomo-playground-test-{}",
        uuid::Uuid::new_v4()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"));
    command
        .args(["playground", "--listen", "127.0.0.1:0", "--output-dir"])
        .arg(&directory)
        .args(options)
        // Tests send terminal-style signals to this group, never to the test runner.
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    if let Some(wrapper) = wrapper {
        let path = std::env::var_os("PATH").unwrap();
        let real = std::env::split_paths(&path)
            .map(|path| path.join("docker"))
            .find(|path| path.is_file())
            .expect("Docker CLI is required");
        let bin = directory.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("docker");
        // These faults target live sessions, after the startup image probe.
        let wrapper = wrapper.replacen(
            "#!/bin/sh\n",
            "#!/bin/sh\ncase \"$*\" in *agenttomo-probe-*) exec \"$AGENTTOMO_TEST_REAL_DOCKER\" \"$@\" ;; esac\n",
            1,
        );
        std::fs::write(&script, wrapper).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        command
            .env("AGENTTOMO_TEST_REAL_DOCKER", real)
            .env("AGENTTOMO_TEST_GATE_DIR", &directory)
            .env(
                "PATH",
                std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
                    .unwrap(),
            );
    }
    let mut server = command.spawn().unwrap();
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

#[tokio::test]
#[ignore = "requires Docker access and the local agenttomo-playground-chrome:24.04 image"]
async fn browser_bridge_requires_auth_and_a_live_owned_session() {
    let mut server = start_server_with_options(None, &["--capture-http"]).await;
    let (mut terminal, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut terminal).await;
    let _guard = ContainerGuard(container.clone());
    let url = server
        .ws_url
        .replace("/ws?", &format!("/api/sessions/{id}/desktop?"));
    let unauthorized = url.split('?').next().unwrap();
    assert!(
        matches!(connect_async(unauthorized).await.unwrap_err(), tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 401)
    );
    let mut foreign = url.as_str().into_client_request().unwrap();
    foreign
        .headers_mut()
        .insert("Origin", "https://other.example".parse().unwrap());
    assert!(
        matches!(connect_async(foreign).await.unwrap_err(), tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 403)
    );
    let missing = url.replace(&id, "00000000000000000000000000000000");
    assert!(
        matches!(connect_async(missing).await.unwrap_err(), tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 404)
    );
    let (mut display, _) = connect_async(&url).await.unwrap();
    let greeting = timeout(Duration::from_secs(10), display.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(greeting, Message::Binary(bytes) if bytes.starts_with(b"RFB ")));
    display.close(None).await.unwrap();
    let (mut display, _) = connect_async(&url).await.unwrap();
    let greeting = timeout(Duration::from_secs(10), display.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(greeting, Message::Binary(bytes) if bytes.starts_with(b"RFB ")));
    timeout(Duration::from_secs(3), async {
        loop {
            let output = Command::new("docker")
                .args(["exec", &container, "pgrep", "--count", "--exact", "nc"])
                .output()
                .await
                .unwrap();
            if output.stdout == b"1\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("reconnecting leaked a VNC relay process");
    terminal.close(None).await.unwrap();
    timeout(Duration::from_secs(10), async {
        while let Some(Ok(message)) = display.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .expect("desktop bridge survived terminal disconnect");
    removed(&container).await;
    assert!(
        matches!(connect_async(&url).await.unwrap_err(), tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 404)
    );
    server.stop().await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and the local agenttomo-playground-chrome:24.04 image"]
async fn server_shutdown_closes_an_active_desktop_without_http_capture() {
    let image = format!(
        "agenttomo-browser-test-{}:local",
        uuid::Uuid::new_v4().simple()
    );
    assert!(
        Command::new("docker")
            .args(["tag", "agenttomo-playground-chrome:24.04", &image])
            .status()
            .await
            .unwrap()
            .success()
    );
    let _image = ImageGuard(image.clone());
    let mut server = start_server_with_options(None, &["--image", &image]).await;
    let (mut terminal, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut terminal).await;
    let _guard = ContainerGuard(container.clone());
    let url = server
        .ws_url
        .replace("/ws?", &format!("/api/sessions/{id}/desktop?"));
    let (mut display, _) = connect_async(&url).await.unwrap();
    let greeting = timeout(Duration::from_secs(10), display.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(greeting, Message::Binary(bytes) if bytes.starts_with(b"RFB ")));
    server.stop().await;
    removed(&container).await;
    timeout(Duration::from_secs(3), async {
        while let Some(Ok(message)) = display.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .expect("desktop survived server shutdown");
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and the local agenttomo-playground-chrome:24.04 image"]
async fn browser_mode_reports_missing_image_helpers_and_removes_the_container() {
    // The image passes detection, then a desktop launch failure must still clean up.
    let wrapper = r#"#!/bin/sh
if [ "$1" = exec ] && [ "$3" = /usr/local/bin/agenttomo-desktop ]; then
    echo 'desktop helper unavailable' >&2
    exit 42
fi
exec "$AGENTTOMO_TEST_REAL_DOCKER" "$@"
"#;
    let mut server = start_server_with_options(Some(wrapper), &[]).await;
    let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, message) = timeout(Duration::from_secs(15), async {
        let mut container = String::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "session" {
                container = value["container"].as_str().unwrap().to_owned();
            }
            if value["type"] == "error" {
                return (container, value["message"].as_str().unwrap().to_owned());
            }
        }
        panic!("missing desktop setup error");
    })
    .await
    .unwrap();
    assert!(message.contains("desktop helper unavailable"), "{message}");
    removed(&container).await;
    server.stop().await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

// Failure-path tests intentionally leave a container behind before asserting.
// Always clean up that specific fixture, including after a panic.
struct ContainerGuard(String);
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.0])
            .output();
    }
}

struct ImageGuard(String);
impl Drop for ImageGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["image", "rm", &self.0])
            .output();
    }
}

#[tokio::test]
#[ignore = "requires Docker access and the local agenttomo-playground-chrome:24.04 image"]
async fn incomplete_browser_image_fails_detection_and_cleans_up_the_probe() {
    let image = format!(
        "agenttomo-broken-browser-{}:local",
        uuid::Uuid::new_v4().simple()
    );
    let _image = ImageGuard(image.clone());
    let mut build = Command::new("docker")
        .args([
            "build",
            "--pull=false",
            "--network=none",
            "--tag",
            &image,
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    build.stdin.take().unwrap().write_all(
        b"FROM agenttomo-playground-chrome:24.04\nRUN chmod -x /usr/local/bin/agenttomo-desktop\n",
    ).await.unwrap();
    let built = timeout(Duration::from_secs(30), build.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = timeout(
        Duration::from_secs(40),
        Command::new(env!("CARGO_BIN_EXE_agenttomo-box"))
            .args(["playground", "--image", &image, "--listen", "127.0.0.1:0"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot detect browser support"), "{error}");
    assert!(
        error.contains("/usr/local/bin/agenttomo-desktop"),
        "{error}"
    );
    let remaining = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--filter",
            &format!("ancestor={image}"),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await
        .unwrap();
    assert!(remaining.status.success());
    assert!(
        remaining.stdout.is_empty(),
        "image probe survived startup failure"
    );
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
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
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
    let (container, id) = ready(&mut socket).await;
    let session: Value = serde_json::from_slice(
        &std::fs::read(server.directory.join(id).join("session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(session["browser"], false);
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
            b"test -t 0 && printf 'PTY_OK\\n'; stty size; if ifconfig lo >/dev/null && ip link show lo >/dev/null && curl --version >/dev/null && dig -v >/dev/null && jq -n -e '1 + 1 == 2' >/dev/null && python3 -c 'import socket'; then printf 'TOOLS_OK\\n'; else printf 'TOOLS_MISSING\\n'; fi; /bin/echo TRACE_VERIFIED\n"
                .to_vec()
                .into(),
        ))
        .await
        .unwrap();
    let mut terminal = Vec::new();
    let mut traced_echo = false;
    timeout(Duration::from_secs(10), async {
        while !traced_echo
            || !String::from_utf8_lossy(&terminal).contains("33 101\r\n")
            || !(String::from_utf8_lossy(&terminal).contains("TOOLS_OK\r\n")
                || String::from_utf8_lossy(&terminal).contains("TOOLS_MISSING\r\n"))
        {
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
    assert!(
        String::from_utf8_lossy(&terminal).contains("TOOLS_OK\r\n"),
        "{}",
        String::from_utf8_lossy(&terminal)
    );
    assert!(!container.is_empty());
    socket.close(None).await.unwrap();
    removed(&container).await;
    server.stop().await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
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
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
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

#[tokio::test]
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
async fn repeated_terminal_interrupts_do_not_interrupt_container_cleanup() {
    // Gate the real rm command so the second Ctrl-C deterministically occurs
    // while cleanup is running, rather than relying on Docker's timing.
    let wrapper = r#"#!/bin/sh
if [ "$1" = rm ]; then
    echo ready > "$AGENTTOMO_TEST_GATE_DIR/removing"
    while [ ! -f "$AGENTTOMO_TEST_GATE_DIR/release" ]; do sleep 0.01; done
fi
exec "$AGENTTOMO_TEST_REAL_DOCKER" "$@"
"#;
    let mut server = start_server_with_docker_wrapper(Some(wrapper)).await;
    let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut socket).await;
    let _container = ContainerGuard(container.clone());
    let group = nix::unistd::Pid::from_raw(server.child.id().unwrap() as i32);
    nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGINT).unwrap();
    timeout(Duration::from_secs(5), async {
        while !server.directory.join("removing").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cleanup did not start");
    nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGINT).unwrap();
    std::fs::write(server.directory.join("release"), b"").unwrap();
    assert!(
        timeout(Duration::from_secs(15), server.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let result: Value = serde_json::from_slice(
        &std::fs::read(server.directory.join(id).join("session-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(result["container_removed"], true, "{result}");
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(result["reason"], "server_shutdown");
    removed(&container).await;
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
async fn cleanup_verifies_daemon_state_after_the_rm_client_reports_failure() {
    // The daemon completes the real deletion, but the CLI fails before the
    // server can receive a successful result, as with an interrupted client.
    let wrapper = r#"#!/bin/sh
if [ "$1" = rm ]; then
    "$AGENTTOMO_TEST_REAL_DOCKER" "$@" || exit "$?"
    exit 42
fi
exec "$AGENTTOMO_TEST_REAL_DOCKER" "$@"
"#;
    let mut server = start_server_with_docker_wrapper(Some(wrapper)).await;
    let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
    let (container, id) = ready(&mut socket).await;
    let _container = ContainerGuard(container.clone());
    server.stop().await;
    removed(&container).await;
    let result: Value = serde_json::from_slice(
        &std::fs::read(server.directory.join(id).join("session-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(result["container_removed"], true, "{result}");
    assert!(result["error"].is_null(), "{result}");
    let _ = std::fs::remove_dir_all(&server.directory);
}

#[tokio::test]
#[ignore = "requires Docker access and a local agenttomo-playground:24.04 image"]
async fn cleanup_reports_real_failures_instead_of_claiming_the_container_is_gone() {
    for (failure, verification, expected) in [
        ("exit 42", "", "exit status: 42"),
        ("kill -KILL $$", "", "signal: 9"),
        (
            "exit 42",
            "if [ \"$1\" = container ] && [ \"$2\" = ls ]; then echo 'verification denied' >&2; exit 43; fi",
            "verification denied",
        ),
    ] {
        let wrapper = format!(
            "#!/bin/sh\nif [ \"$1\" = rm ]; then {failure}; fi\n{verification}\nexec \"$AGENTTOMO_TEST_REAL_DOCKER\" \"$@\"\n"
        );
        let mut server = start_server_with_docker_wrapper(Some(&wrapper)).await;
        let (mut socket, _) = connect_async(&server.ws_url).await.unwrap();
        let (container, id) = ready(&mut socket).await;
        let _container = ContainerGuard(container.clone());
        server.stop().await;
        let result: Value = serde_json::from_slice(
            &std::fs::read(server.directory.join(id).join("session-result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(result["container_removed"], false, "{result}");
        assert!(
            result["error"].as_str().unwrap().contains(expected),
            "{result}"
        );
        assert!(
            Command::new("docker")
                .args(["container", "inspect", &container])
                .output()
                .await
                .unwrap()
                .status
                .success(),
            "the fixture must really still exist"
        );
        let _ = std::fs::remove_dir_all(&server.directory);
    }
}
