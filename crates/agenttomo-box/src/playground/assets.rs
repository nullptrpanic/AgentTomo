use axum::{
    extract::Path,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};

pub async fn get(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let (mime, content) = match path.as_str() {
        "resume.mjs" => ("text/javascript", include_str!("assets/resume.mjs")),
        "vendor/xterm-LICENSE" => ("text/plain", include_str!("assets/vendor/xterm-LICENSE")),
        "vendor/addon-fit-LICENSE" => (
            "text/plain",
            include_str!("assets/vendor/addon-fit-LICENSE"),
        ),
        "app.js" => ("text/javascript", include_str!("assets/app.js")),
        "workspace.mjs" => ("text/javascript", include_str!("assets/workspace.mjs")),
        "agent-model.mjs" => ("text/javascript", include_str!("assets/agent-model.mjs")),
        "agent-chat.mjs" => ("text/javascript", include_str!("assets/agent-chat.mjs")),
        "agent-markdown.mjs" => ("text/javascript", include_str!("assets/agent-markdown.mjs")),
        "agent-view.mjs" => ("text/javascript", include_str!("assets/agent-view.mjs")),
        "agent.css" => ("text/css", include_str!("assets/agent.css")),
        "samples.json" => ("application/json", super::samples::CATALOG),
        "samples-model.mjs" => ("text/javascript", include_str!("assets/samples-model.mjs")),
        "samples-view.mjs" => ("text/javascript", include_str!("assets/samples-view.mjs")),
        "samples.css" => ("text/css", include_str!("assets/samples.css")),
        "observation.mjs" => ("text/javascript", include_str!("assets/observation.mjs")),
        "monitor.mjs" => ("text/javascript", include_str!("assets/monitor.mjs")),
        "panel-resize.mjs" => ("text/javascript", include_str!("assets/panel-resize.mjs")),
        "browser-view.mjs" => ("text/javascript", include_str!("assets/browser-view.mjs")),
        "browser.css" => ("text/css", include_str!("assets/browser.css")),
        "vendor/novnc.mjs" => ("text/javascript", include_str!("assets/vendor/novnc.mjs")),
        "vendor/marked.mjs" => ("text/javascript", include_str!("assets/vendor/marked.mjs")),
        "vendor/dompurify.mjs" => (
            "text/javascript",
            include_str!("assets/vendor/dompurify.mjs"),
        ),
        "vendor/marked-LICENSE.txt" => (
            "text/plain",
            include_str!("assets/vendor/marked-LICENSE.txt"),
        ),
        "vendor/dompurify-LICENSE.txt" => (
            "text/plain",
            include_str!("assets/vendor/dompurify-LICENSE.txt"),
        ),
        "vendor/dompurify-LICENSE-MPL.txt" => (
            "text/plain",
            include_str!("assets/vendor/dompurify-LICENSE-MPL.txt"),
        ),
        "vendor/novnc-LICENSE.txt" => (
            "text/plain",
            include_str!("assets/vendor/novnc-LICENSE.txt"),
        ),
        "vendor/novnc-NOTICES.txt" => (
            "text/plain",
            include_str!("assets/vendor/novnc-NOTICES.txt"),
        ),
        "traffic-view.mjs" => ("text/javascript", include_str!("assets/traffic-view.mjs")),
        "traffic-model.mjs" => ("text/javascript", include_str!("assets/traffic-model.mjs")),
        "http-preview.mjs" => ("text/javascript", include_str!("assets/http-preview.mjs")),
        "stream-view.mjs" => ("text/javascript", include_str!("assets/stream-view.mjs")),
        "stream-model.mjs" => ("text/javascript", include_str!("assets/stream-model.mjs")),
        "traffic.css" => ("text/css", include_str!("assets/traffic.css")),
        "file-preview.mjs" => ("text/javascript", include_str!("assets/file-preview.mjs")),
        "copy-text.mjs" => ("text/javascript", include_str!("assets/copy-text.mjs")),
        "icons.mjs" => ("text/javascript", include_str!("assets/icons.mjs")),
        "file-preview.css" => ("text/css", include_str!("assets/file-preview.css")),
        "process-tree.mjs" => ("text/javascript", include_str!("assets/process-tree.mjs")),
        "legacy-process-tree.mjs" => (
            "text/javascript",
            include_str!("assets/legacy-process-tree.mjs"),
        ),
        "event-description.mjs" => (
            "text/javascript",
            include_str!("assets/event-description.mjs"),
        ),
        "trace-view.mjs" => ("text/javascript", include_str!("assets/trace-view.mjs")),
        "activity-model.mjs" => ("text/javascript", include_str!("assets/activity-model.mjs")),
        "network-activity.mjs" => (
            "text/javascript",
            include_str!("assets/network-activity.mjs"),
        ),
        "event-evidence.mjs" => ("text/javascript", include_str!("assets/event-evidence.mjs")),
        "event-time.mjs" => ("text/javascript", include_str!("assets/event-time.mjs")),
        "event-filter.mjs" => ("text/javascript", include_str!("assets/event-filter.mjs")),
        "style.css" => ("text/css", include_str!("assets/style.css")),
        "vendor/xterm.js" => ("text/javascript", include_str!("assets/vendor/xterm.js")),
        "vendor/addon-fit.js" => (
            "text/javascript",
            include_str!("assets/vendor/addon-fit.js"),
        ),
        "vendor/xterm.css" => ("text/css", include_str!("assets/vendor/xterm.css")),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    use std::{
        collections::HashMap,
        hash::{DefaultHasher, Hash, Hasher},
        sync::{Mutex, OnceLock},
    };
    static TAGS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let etag = TAGS
        .get_or_init(Default::default)
        .lock()
        .expect("asset tags")
        .entry(path)
        .or_insert_with(|| {
            let mut hash = DefaultHasher::new();
            content.hash(&mut hash);
            format!(r#""{:016x}""#, hash.finish())
        })
        .clone();
    let unchanged = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/") == etag);
    let response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            content,
        )
            .into_response()
    };
    (
        [
            (header::ETAG, etag.as_str()),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        response,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_asset_supports_conditional_get() {
        let first = get(Path("app.js".into()), HeaderMap::new()).await;
        let etag = first.headers()[header::ETAG].clone();
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.clone());
        let cached = get(Path("app.js".into()), headers.clone()).await;
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(&cached.headers()[header::ETAG], &etag);
        assert_eq!(
            get(Path("style.css".into()), headers).await.status(),
            StatusCode::OK
        );
    }
}
