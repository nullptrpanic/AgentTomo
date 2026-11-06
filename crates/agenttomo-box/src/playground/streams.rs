//! Read the retained message window; IDs never alias reused payload offsets.
use super::{Access, App, payload::run_directory};
use crate::recording::streams;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nix::{
    fcntl::{OFlag, openat},
    sys::stat::Mode,
};
use serde::Deserialize;
use std::fs::File;

#[derive(Deserialize)]
pub(super) struct Selection {
    #[serde(flatten)]
    access: Access,
    request_id: u64,
    message_id: Option<u64>,
}

pub async fn read(
    State(app): State<App>,
    Path(session): Path<String>,
    Query(query): Query<Selection>,
) -> Response {
    if !query.access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session.len() != 32
        || !session.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !(1..=i64::MAX as u64).contains(&query.request_id)
        || query
            .message_id
            .is_some_and(|id| !(1..=i64::MAX as u64).contains(&id))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let dir =
            run_directory(&app.settings.output_dir, &session).map_err(|_| StatusCode::NOT_FOUND)?;
        let file = File::from(
            openat(
                &dir,
                streams::FILE,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| StatusCode::NOT_FOUND)?,
        );
        if !file
            .metadata()
            .map_err(|_| StatusCode::NOT_FOUND)?
            .is_file()
        {
            return Err(StatusCode::NOT_FOUND);
        }
        // SQLite's NOFOLLOW rejects symlinks in every component, including
        // /proc/self/fd. Use the validated absolute path with that flag.
        let path = app
            .settings
            .output_dir
            .join(&session)
            .join("run")
            .join(streams::FILE);
        let mut db = streams::reader(&path).map_err(|_| StatusCode::NOT_FOUND)?;
        let response = if let Some(id) = query.message_id {
            match streams::payload(&db, query.request_id, id)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            {
                Some(bytes) => {
                    ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
                }
                None => return Err(StatusCode::GONE),
            }
        } else {
            Json(
                streams::snapshot(&mut db, query.request_id)
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
            )
            .into_response()
        };
        Ok(response)
    })
    .await;
    let mut response = match result {
        Ok(Ok(response)) => response,
        Ok(Err(status)) => status.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
