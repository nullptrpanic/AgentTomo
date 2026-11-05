use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

pub async fn get(Path(path): Path<String>) -> Response {
    let (mime, content) = match path.as_str() {
        "app.js" => ("text/javascript", include_str!("assets/app.js")),
        "process-tree.mjs" => ("text/javascript", include_str!("assets/process-tree.mjs")),
        "trace-view.mjs" => ("text/javascript", include_str!("assets/trace-view.mjs")),
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
