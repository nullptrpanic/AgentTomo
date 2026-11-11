//! Owner WebSockets manage Docker sessions; monitor pages only read their journals.

mod agent_egress;
mod assets;
pub(crate) mod bridge;
mod desktop;
mod docker;
mod monitor;
#[cfg(test)]
mod monitor_tests;
mod payload;
pub(crate) mod sandbox;
mod session;
mod streams;
mod trace_feed;

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::{Query, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cli::PlaygroundArgs;

struct Settings {
    browser: bool,
    image: String,
    image_id: String,
    binary: PathBuf,
    output_dir: PathBuf,
    session_tmpfs_size_mib: u32,
    token: String,
    file_preview: crate::recording::file_preview::Limits,
    http: crate::proxy::http::Options,
    rootfs: crate::execution::rootfs::Options,
    codex: Option<crate::agent::codex::Installation>,
    proxy: Option<crate::proxy::outbound::Proxy>,
}

#[derive(Clone)]
struct App {
    settings: Arc<Settings>,
    shutdown: CancellationToken,
    sessions: TaskTracker,
    desktops: Arc<Mutex<HashMap<String, CancellationToken>>>,
    running: Arc<Mutex<HashMap<String, Option<i32>>>>,
    proxy_peers: agent_egress::Peers,
    // None only inside the container read service (and filesystem API tests).
    bridges: Option<Arc<Mutex<HashMap<String, bridge::Client>>>>,
}

#[derive(Deserialize)]
struct Access {
    token: Option<String>,
}

impl Access {
    fn authorized(&self, app: &App) -> bool {
        self.token.as_deref() == Some(app.settings.token.as_str())
    }
}

pub async fn run(options: PlaygroundArgs) -> io::Result<()> {
    let image_id = docker::check(&options.image).await?;
    let browser = docker::supports_browser(&image_id).await?;
    let shutdown = CancellationToken::new();
    let proxy_peers = agent_egress::Peers::default();
    let codex = crate::agent::codex::Installation::discover();
    let proxy = agent_egress::configure(
        options.proxy,
        options.no_proxy,
        proxy_peers.clone(),
        shutdown.clone(),
    )
    .await?;
    let mut http = options.http.options();
    if codex.is_some() {
        http.enabled = true;
    }
    let app = App {
        settings: Arc::new(Settings {
            browser,
            image: options.image,
            image_id,
            binary: std::fs::canonicalize(std::env::current_exe()?)?,
            output_dir: options.output_dir,
            session_tmpfs_size_mib: options.session_tmpfs_size_mib,
            token: uuid::Uuid::new_v4().simple().to_string(),
            file_preview: options.file_preview.limits(),
            http,
            rootfs: options.rootfs.options(),
            codex,
            proxy,
        }),
        shutdown,
        sessions: TaskTracker::new(),
        desktops: Default::default(),
        running: Default::default(),
        proxy_peers,
        bridges: Some(Default::default()),
    };
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    let address = listener.local_addr()?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    println!(
        "AgentTomo playground: http://{address}/?token={}",
        app.settings.token
    );
    io::stdout().flush()?;
    let shutdown = app.shutdown.clone();
    let result = axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(async move {
            tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
            shutdown.cancel();
        })
        .await;
    app.shutdown.cancel();
    app.sessions.close();
    app.sessions.wait().await;
    result
}

fn router(app: App) -> Router {
    records_router()
        .route_layer(axum::middleware::from_fn_with_state(
            app.clone(),
            bridge::forward,
        ))
        .route("/", get(index))
        .route("/ws", get(websocket))
        .route("/api/sessions/{session}/desktop", get(desktop::websocket))
        .route("/assets/{*path}", get(assets::get))
        .with_state(app)
}

fn records_router() -> Router<App> {
    Router::new()
        .route("/api/sessions/{session}/events", get(monitor::read))
        .route(
            "/api/sessions/{session}/file-preview",
            get(payload::file_preview),
        )
        .route("/api/sessions/{session}/http-body", get(payload::http_body))
        .route(
            "/api/sessions/{session}/agent-payload",
            get(payload::agent_payload),
        )
        .route(
            "/api/sessions/{session}/stream-messages",
            get(streams::read),
        )
}

fn valid_session(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn index(State(app): State<App>, Query(access): Query<Access>) -> Response {
    if !access.authorized(&app) {
        return (
            StatusCode::UNAUTHORIZED,
            "Open the complete access URL printed by agenttomo-box playground.",
        )
            .into_response();
    }
    ([
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ], Html(include_str!("assets/index.html"))).into_response()
}

async fn websocket(
    State(app): State<App>,
    Query(access): Query<Access>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Browser clients must originate from this HTTP server. Non-browser clients
    // may omit Origin but still need the unguessable access token.
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if app.shutdown.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // Reserve before upgrade so server shutdown waits for sessions that are
    // still completing their WebSocket handshake as well as active containers.
    let tracked = app.sessions.token();
    upgrade
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            let _tracked = tracked;
            session::serve(socket, app).await;
        })
}

fn same_origin(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => {
            let host = headers
                .get(header::HOST)
                .and_then(|host| host.to_str().ok());
            matches!((origin.to_str().ok(), host), (Some(origin), Some(host)) if origin == format!("http://{host}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn file_preview_ranges_require_auth_and_cannot_follow_session_or_payload_symlinks() {
        check_payload_ranges("file-preview", "file-previews.bin").await;
    }

    #[tokio::test]
    async fn http_body_ranges_require_auth_and_cannot_follow_session_or_payload_symlinks() {
        check_payload_ranges("http-body", "http-bodies.bin").await;
    }

    #[tokio::test]
    async fn stream_api_auth_expiry_and_symlink_boundaries() {
        let directory =
            std::env::temp_dir().join(format!("agenttomo-stream-api-{}", uuid::Uuid::new_v4()));
        let id = "0123456789abcdef0123456789abcdef";
        let run = directory.join(id).join("run");
        std::fs::create_dir_all(&run).unwrap();
        let store = crate::recording::streams::Store::create(
            &run,
            crate::recording::streams::Limits {
                window_messages: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for payload in [b"first", b"hello"] {
            store.record(crate::recording::streams::Message {
                request_id: 7,
                data: serde_json::json!({"kind":"text"}),
                payload: payload.to_vec(),
            });
        }
        store.flush().await.unwrap();
        let app = App {
            settings: Arc::new(Settings {
                browser: false,
                image: String::new(),
                image_id: String::new(),
                binary: PathBuf::new(),
                output_dir: directory.clone(),
                session_tmpfs_size_mib: 512,
                token: "test-token".into(),
                file_preview: Default::default(),
                http: Default::default(),
                rootfs: Default::default(),
                codex: None,
                proxy: None,
            }),
            shutdown: CancellationToken::new(),
            sessions: TaskTracker::new(),
            desktops: Default::default(),
            running: Default::default(),
            proxy_peers: Default::default(),
            bridges: None,
        };
        let router = router(app);
        let base = format!("/api/sessions/{id}/stream-messages");
        for (query, status) in [
            ("request_id=7", StatusCode::UNAUTHORIZED),
            ("token=wrong&request_id=7", StatusCode::UNAUTHORIZED),
            ("token=test-token&request_id=7", StatusCode::OK),
            ("token=test-token&request_id=7&message_id=2", StatusCode::OK),
            (
                "token=test-token&request_id=7&message_id=1",
                StatusCode::GONE,
            ),
            (
                "token=test-token&request_id=8&message_id=2",
                StatusCode::GONE,
            ),
            ("token=test-token&request_id=0", StatusCode::BAD_REQUEST),
            (
                "token=test-token&request_id=18446744073709551615",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("{base}?{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{query}");
            if status == StatusCode::OK {
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let bytes = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                if query.contains("message_id") {
                    assert_eq!(bytes.as_ref(), b"hello");
                } else {
                    let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(data["messages"].as_array().unwrap().len(), 1);
                    assert_eq!(data["evicted"], 1);
                }
            }
        }
        drop(store);
        let path = run.join(crate::recording::streams::FILE);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{base}?token=test-token&request_id=7"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&run).unwrap();
        std::os::unix::fs::symlink("/tmp", &run).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("{base}?token=test-token&request_id=7"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(directory).unwrap();
    }

    async fn check_payload_ranges(endpoint: &str, filename: &str) {
        let directory =
            std::env::temp_dir().join(format!("agenttomo-preview-api-{}", uuid::Uuid::new_v4()));
        let id = "0123456789abcdef0123456789abcdef";
        let run = directory.join(id).join("run");
        std::fs::create_dir_all(&run).unwrap();
        let payload = run.join(filename);
        std::fs::write(&payload, b"abcdefgh").unwrap();
        let app = App {
            settings: Arc::new(Settings {
                browser: false,
                image: String::new(),
                image_id: String::new(),
                binary: PathBuf::new(),
                output_dir: directory.clone(),
                session_tmpfs_size_mib: 512,
                token: "test-token".into(),
                file_preview: Default::default(),
                http: Default::default(),
                rootfs: Default::default(),
                codex: None,
                proxy: None,
            }),
            shutdown: CancellationToken::new(),
            sessions: TaskTracker::new(),
            desktops: Default::default(),
            running: Default::default(),
            proxy_peers: Default::default(),
            bridges: None,
        };
        let router = router(app);
        let base = format!("/api/sessions/{id}/{endpoint}");
        for (query, expected) in [
            ("offset=2&length=4", StatusCode::UNAUTHORIZED),
            ("token=wrong&offset=2&length=4", StatusCode::UNAUTHORIZED),
            ("token=test-token&offset=2&length=4", StatusCode::OK),
            ("token=test-token&offset=8&length=0", StatusCode::OK),
            (
                "token=test-token&offset=7&length=2",
                StatusCode::RANGE_NOT_SATISFIABLE,
            ),
            (
                "token=test-token&offset=18446744073709551615&length=1",
                StatusCode::RANGE_NOT_SATISFIABLE,
            ),
            (
                "token=test-token&offset=0&length=1048577",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("{base}?{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{query}");
            if expected == StatusCode::OK {
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert_eq!(
                    response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                    "nosniff"
                );
                let bytes = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    &bytes[..],
                    if query.contains("length=0") {
                        &b""[..]
                    } else {
                        &b"cdef"[..]
                    }
                );
            }
        }
        if endpoint == "http-body" {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(b"<html>decoded preview</html>").unwrap();
            let original = encoder.finish().unwrap();
            std::fs::write(&payload, &original).unwrap();
            for (token, decode, status) in [
                ("wrong", "gzip", StatusCode::UNAUTHORIZED),
                ("test-token", "gzip", StatusCode::OK),
                ("test-token", "identity", StatusCode::OK),
            ] {
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!(
                                "{base}?token={token}&offset=0&length={}&decode={decode}",
                                original.len()
                            ))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
                if status == StatusCode::OK {
                    assert_eq!(
                        response.headers()["x-agenttomo-decoding"],
                        if decode == "gzip" {
                            "complete"
                        } else {
                            "identity"
                        }
                    );
                    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
                    let bytes = axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap();
                    assert_eq!(
                        &bytes[..],
                        if decode == "gzip" {
                            b"<html>decoded preview</html>".as_slice()
                        } else {
                            &original
                        }
                    );
                }
            }
            assert_eq!(std::fs::read(&payload).unwrap(), original);
        }
        let uri = format!("{base}?token=test-token&offset=0&length=4");
        std::fs::remove_file(&payload).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &payload).unwrap();
        let response = router
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        std::fs::remove_file(&payload).unwrap();
        std::fs::remove_dir(&run).unwrap();
        let outside = directory.join("outside");
        std::fs::create_dir_all(outside.join("run")).unwrap();
        std::fs::write(outside.join("run").join(filename), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("run"), &run).unwrap();
        let response = router
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        std::fs::remove_file(&run).unwrap();
        std::fs::remove_dir(directory.join(id)).unwrap();
        std::os::unix::fs::symlink(&outside, directory.join(id)).unwrap();
        let response = router
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/not-a-session/{endpoint}?token=test-token&offset=0&length=4"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn access_token_protects_the_page_and_assets_are_embedded() {
        let app = App {
            settings: Arc::new(Settings {
                browser: false,
                image: String::new(),
                image_id: String::new(),
                binary: PathBuf::new(),
                output_dir: PathBuf::new(),
                session_tmpfs_size_mib: 512,
                token: "test-token".into(),
                file_preview: Default::default(),
                http: Default::default(),
                rootfs: Default::default(),
                codex: None,
                proxy: None,
            }),
            shutdown: CancellationToken::new(),
            sessions: TaskTracker::new(),
            desktops: Default::default(),
            running: Default::default(),
            proxy_peers: Default::default(),
            bridges: None,
        };
        let router = router(app);
        for (path, expected) in [
            ("/", StatusCode::UNAUTHORIZED),
            ("/?token=wrong", StatusCode::UNAUTHORIZED),
            ("/?token=test-token", StatusCode::OK),
            ("/assets/vendor/xterm.js", StatusCode::OK),
            ("/assets/vendor/novnc.mjs", StatusCode::OK),
            ("/assets/browser-view.mjs", StatusCode::OK),
            ("/assets/workspace.mjs", StatusCode::OK),
            ("/assets/monitor.mjs", StatusCode::OK),
            ("/assets/observation.mjs", StatusCode::OK),
            ("/assets/process-tree.mjs", StatusCode::OK),
            ("/assets/trace-view.mjs", StatusCode::OK),
            ("/assets/activity-model.mjs", StatusCode::OK),
            ("/assets/network-activity.mjs", StatusCode::OK),
            ("/assets/event-evidence.mjs", StatusCode::OK),
            ("/assets/behavior-graph.mjs", StatusCode::OK),
            ("/assets/behavior-graph.css", StatusCode::OK),
            ("/assets/event-filter.mjs", StatusCode::OK),
            ("/assets/legacy-process-tree.mjs", StatusCode::OK),
            ("/assets/event-description.mjs", StatusCode::OK),
            ("/assets/../../Cargo.toml", StatusCode::NOT_FOUND),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
    }
}
