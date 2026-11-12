use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

pub async fn get(Path(path): Path<String>) -> Response {
    let (mime, content) = match path.as_str() {
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
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        content,
    )
        .into_response()
}
