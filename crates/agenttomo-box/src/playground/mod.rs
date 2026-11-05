//! HTTP entry point. Each authenticated WebSocket owns a Docker session.

mod assets;
mod docker;
mod session;
mod trace_feed;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

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
    image: String,
    binary: PathBuf,
    output_dir: PathBuf,
    token: String,
}

#[derive(Clone)]
struct App {
    settings: Arc<Settings>,
    shutdown: CancellationToken,
    sessions: TaskTracker,
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
    docker::check(&options.image).await?;
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&options.output_dir)
        .await?;
    let app = App {
        settings: Arc::new(Settings {
            image: options.image,
            binary: std::fs::canonicalize(std::env::current_exe()?)?,
            output_dir: tokio::fs::canonicalize(options.output_dir).await?,
            token: uuid::Uuid::new_v4().simple().to_string(),
        }),
        shutdown: CancellationToken::new(),
        sessions: TaskTracker::new(),
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
    Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket))
        .route("/assets/{*path}", get(assets::get))
        .with_state(app)
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
    if let Some(origin) = headers.get(header::ORIGIN) {
        let host = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok());
        if !matches!((origin.to_str().ok(), host), (Some(origin), Some(host)) if origin == format!("http://{host}"))
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if app.shutdown.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // Reserve before upgrade so server shutdown waits for sessions that are
    // still completing their WebSocket handshake as well as active containers.
    let tracked = app.sessions.token();
    upgrade
        .max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| async move {
            let _tracked = tracked;
            session::serve(socket, app).await;
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn access_token_protects_the_page_and_assets_are_embedded() {
        let app = App {
            settings: Arc::new(Settings {
                image: String::new(),
                binary: PathBuf::new(),
                output_dir: PathBuf::new(),
                token: "test-token".into(),
            }),
            shutdown: CancellationToken::new(),
            sessions: TaskTracker::new(),
        };
        let router = router(app);
        for (path, expected) in [
            ("/", StatusCode::UNAUTHORIZED),
            ("/?token=wrong", StatusCode::UNAUTHORIZED),
            ("/?token=test-token", StatusCode::OK),
            ("/assets/vendor/xterm.js", StatusCode::OK),
            ("/assets/process-tree.mjs", StatusCode::OK),
            ("/assets/trace-view.mjs", StatusCode::OK),
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
