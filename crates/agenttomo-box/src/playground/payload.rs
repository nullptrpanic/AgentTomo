//! Serve retained payload ranges, never arbitrary file paths from the browser.

use super::{Access, App};
use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nix::{
    fcntl::{OFlag, openat},
    sys::stat::Mode,
};
use serde::Deserialize;
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path as FsPath,
};

#[derive(Deserialize)]
pub(super) struct Range {
    #[serde(flatten)]
    access: Access,
    offset: u64,
    length: u32,
}

pub async fn file_preview(
    State(app): State<App>,
    Path(session): Path<String>,
    Query(range): Query<Range>,
) -> Response {
    read(
        app,
        session,
        range,
        crate::recording::file_preview::PAYLOAD_FILE,
        crate::recording::file_preview::MAX_PREVIEW_BYTES,
    )
    .await
}

pub async fn http_body(
    State(app): State<App>,
    Path(session): Path<String>,
    Query(range): Query<Range>,
) -> Response {
    read(
        app,
        session,
        range,
        crate::recording::http::PAYLOAD_FILE,
        crate::recording::http::MAX_BODY_BYTES,
    )
    .await
}

async fn read(
    app: App,
    session: String,
    range: Range,
    payload_file: &'static str,
    max_bytes: u32,
) -> Response {
    if !range.access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session.len() != 32
        || !session.bytes().all(|byte| byte.is_ascii_hexdigit())
        || range.length > max_bytes
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let dir = run_directory(&app.settings.output_dir, &session)?;
        let file = File::from(openat(
            &dir,
            payload_file,
            OFlag::O_RDONLY
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_NONBLOCK
                | OFlag::O_NOCTTY,
            Mode::empty(),
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let end = range.offset.checked_add(u64::from(range.length));
        if !end.is_some_and(|end| end <= metadata.len()) {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let mut bytes = vec![0; range.length as usize];
        file.read_exact_at(&mut bytes, range.offset)?;
        Ok::<_, io::Error>(bytes)
    })
    .await;
    match result {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
            StatusCode::RANGE_NOT_SATISFIABLE.into_response()
        }
        Ok(Err(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) fn run_directory(root: &FsPath, session: &str) -> io::Result<File> {
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(root)?;
    for part in [session, "run"] {
        dir = File::from(openat(
            &dir,
            part,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?);
    }
    Ok(dir)
}
