use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

pub async fn get(Path(path): Path<String>) -> Response {
    let (mime, content) = match path.as_str() {
        "app.js" => ("text/javascript", include_str!("assets/app.js")),
        "browser-view.mjs" => ("text/javascript", include_str!("assets/browser-view.mjs")),
        "browser.css" => ("text/css", include_str!("assets/browser.css")),
        "vendor/novnc.mjs" => ("text/javascript", include_str!("assets/vendor/novnc.mjs")),
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
        "stream-view.mjs" => ("text/javascript", include_str!("assets/stream-view.mjs")),
        "stream-model.mjs" => ("text/javascript", include_str!("assets/stream-model.mjs")),
        "traffic.css" => ("text/css", include_str!("assets/traffic.css")),
        "file-preview.mjs" => ("text/javascript", include_str!("assets/file-preview.mjs")),
        "copy-text.mjs" => ("text/javascript", include_str!("assets/copy-text.mjs")),
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
