use super::*;
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;

const ID: &str = "0123456789abcdef0123456789abcdef";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agenttomo-monitor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.join(ID).join("run")).unwrap();
        std::fs::write(
            path.join(ID).join("session.json"),
            json!({"session_id": ID, "image": "test-image", "http_capture": true}).to_string(),
        )
        .unwrap();
        Self(path)
    }

    fn app(&self) -> App {
        App {
            settings: Arc::new(Settings {
                browser: false,
                image: String::new(),
                image_id: String::new(),
                binary: PathBuf::new(),
                output_dir: self.0.clone(),
                session_tmpfs_size_mib: 512,
                token: "monitor-token".into(),
                file_preview: Default::default(),
                http: Default::default(),
                rootfs: Default::default(),
                agent: None,
                proxy: None,
                credential_gateway: None,
            }),
            shutdown: CancellationToken::new(),
            sessions: TaskTracker::new(),
            desktops: Default::default(),
            running: Default::default(),
            proxy_peers: Default::default(),
            bridges: None,
        }
    }

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) {
        std::fs::write(self.0.join(ID).join(name), bytes).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn get(app: App, session: &str, query: &str) -> Response {
    router(app)
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{session}/events?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_response(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn archived_session_can_be_replayed_by_independent_cursors() {
    let fixture = Fixture::new();
    let bytes = b"{\"sequence\":1,\"kind\":\"process.exec\",\"data\":{\"pid\":35}}\n{\"sequence\":2,\"kind\":\"process.exit\",\"data\":{\"pid\":35}}\n";
    fixture.write("run/events.jsonl", bytes);
    fixture.write(
        "session-result.json",
        br#"{"reason":"completed","error":null,"root_pid":35}"#,
    );
    let app = fixture.app();
    for _ in 0..2 {
        let body = json_response(get(app.clone(), ID, "token=monitor-token&offset=0").await).await;
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["next_offset"], bytes.len());
        assert_eq!(body["has_more"], false);
        assert_eq!(body["active"], false);
        assert_eq!(body["root_pid"], 35);
        assert_eq!(body["session"]["http_capture"], true);
    }
    let body = json_response(
        get(
            app,
            ID,
            &format!("token=monitor-token&offset={}", bytes.len()),
        )
        .await,
    )
    .await;
    assert_eq!(body["events"], json!([]));
}

#[tokio::test]
async fn monitor_events_require_auth_and_reject_invalid_offsets_and_symlinks() {
    let fixture = Fixture::new();
    fixture.write("run/events.jsonl", b"{\"sequence\":1}\n");
    let app = fixture.app();
    assert_eq!(
        get(app.clone(), ID, "offset=0").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(app.clone(), "not-a-session", "token=monitor-token&offset=0")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    for offset in [1, 999] {
        assert_eq!(
            get(
                app.clone(),
                ID,
                &format!("token=monitor-token&offset={offset}")
            )
            .await
            .status(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
    }
    let trace = fixture.0.join(ID).join("run/events.jsonl");
    std::fs::remove_file(&trace).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &trace).unwrap();
    assert_eq!(
        get(app, ID, "token=monitor-token&offset=0").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn active_cursor_waits_for_partial_lines_and_keeps_the_final_tail() {
    use std::io::Write;
    let fixture = Fixture::new();
    let app = fixture.app();
    app.running.lock().unwrap().insert(ID.to_owned(), Some(35));
    let starting = json_response(get(app.clone(), ID, "token=monitor-token").await).await;
    assert_eq!(starting["events"], json!([]));
    assert_eq!(starting["active"], true);
    assert_eq!(starting["root_pid"], 35);

    let first = b"{\"sequence\":1}\n";
    fixture.write(
        "run/events.jsonl",
        [first.as_slice(), b"{\"sequence\":"].concat(),
    );
    let batch = json_response(get(app.clone(), ID, "token=monitor-token").await).await;
    assert_eq!(batch["events"], json!([{"sequence":1}]));
    assert_eq!(batch["next_offset"], first.len());
    assert_eq!(batch["incomplete"], false);
    let query = format!("token=monitor-token&offset={}", first.len());
    let waiting = json_response(get(app.clone(), ID, &query).await).await;
    assert_eq!(waiting["events"], json!([]));
    assert_eq!(waiting["next_offset"], first.len());

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.0.join(ID).join("run/events.jsonl"))
        .unwrap();
    file.write_all(b"2}\n{\"sequence\":3").unwrap();
    fixture.write(
        "session-result.json",
        br#"{"reason":"browser_disconnected","root_pid":35}"#,
    );
    app.running.lock().unwrap().remove(ID);
    let ended = json_response(get(app, ID, &query).await).await;
    assert_eq!(ended["events"], json!([{"sequence":2}]));
    assert_eq!(ended["next_offset"], first.len() * 2);
    assert_eq!(ended["active"], false);
    assert_eq!(ended["has_more"], false);
    assert_eq!(ended["incomplete"], true);
    assert_eq!(ended["root_pid"], 35);
}

#[tokio::test]
async fn replay_is_paginated_without_skips_or_duplicate_events() {
    let fixture = Fixture::new();
    let bytes: String = (0..600)
        .map(|sequence| format!("{{\"sequence\":{sequence}}}\n"))
        .collect();
    fixture.write("run/events.jsonl", &bytes);
    let app = fixture.app();
    let mut offset = 0;
    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    loop {
        let batch = json_response(
            get(
                app.clone(),
                ID,
                &format!("token=monitor-token&offset={offset}"),
            )
            .await,
        )
        .await;
        let events = batch["events"].as_array().unwrap();
        sizes.push(events.len());
        seen.extend(
            events
                .iter()
                .map(|event| event["sequence"].as_u64().unwrap()),
        );
        offset = batch["next_offset"].as_u64().unwrap();
        if batch["has_more"] == false {
            break;
        }
    }
    assert_eq!(sizes, vec![256, 256, 88]);
    assert_eq!(seen, (0..600).collect::<Vec<_>>());
    assert_eq!(offset as usize, bytes.len());
}

#[tokio::test]
async fn monitor_rejects_linked_metadata_directories_and_special_files() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let app = fixture.app();
    let run = fixture.0.join(ID).join("run");
    std::fs::remove_dir(&run).unwrap();
    symlink("/tmp", &run).unwrap();
    assert_eq!(
        get(app.clone(), ID, "token=monitor-token").await.status(),
        StatusCode::NOT_FOUND
    );
    std::fs::remove_file(&run).unwrap();
    std::fs::create_dir(&run).unwrap();
    nix::unistd::mkfifo(&run.join("events.jsonl"), nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert_eq!(
        get(app.clone(), ID, "token=monitor-token").await.status(),
        StatusCode::NOT_FOUND
    );
    std::fs::remove_file(run.join("events.jsonl")).unwrap();
    let metadata = fixture.0.join(ID).join("session.json");
    std::fs::remove_file(&metadata).unwrap();
    symlink("/etc/passwd", &metadata).unwrap();
    assert_eq!(
        get(app, ID, "token=monitor-token").await.status(),
        StatusCode::NOT_FOUND
    );
}
