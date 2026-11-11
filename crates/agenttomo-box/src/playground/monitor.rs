//! Independent read cursors over the existing journal; no container ownership.

use super::{Access, App, payload};
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
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
};

const BATCH_EVENTS: usize = 256;
const BATCH_BYTES: u64 = 1024 * 1024;
const RECORD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct Cursor {
    #[serde(flatten)]
    access: Access,
    #[serde(default)]
    offset: u64,
}

pub async fn read(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(cursor): Query<Cursor>,
) -> Response {
    if !cursor.access.authorized(&app) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Take liveness BEFORE reading: an inactive session has finished writing.
    // A concurrent completion can require one more poll, but cannot hide a tail.
    let running = app
        .running
        .lock()
        .expect("session registry")
        .get(&id)
        .copied();
    let result = tokio::task::spawn_blocking(move || {
        let dir = payload::session_directory(&app.settings.output_dir, &id)?;
        let metadata = read_json(&dir, "session.json")?;
        let outcome = match read_json(&dir, "session-result.json") {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error),
        };
        let running = running.filter(|_| outcome.is_null());
        let root_pid = running
            .flatten()
            .map_or_else(|| outcome["root_pid"].clone(), |pid| json!(pid));
        let file = match payload::run_directory(&app.settings.output_dir, &id)
            .and_then(|run| regular_file(&run, "events.jsonl"))
        {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let (events, next_offset, has_more, partial) = batch(file, cursor.offset)?;
        Ok::<_, io::Error>(json!({
            "session":metadata, "root_pid":root_pid, "active":running.is_some(),
            "result":outcome, "events":events, "next_offset":next_offset,
            "has_more":has_more, "incomplete":partial && running.is_none()
        }))
    })
    .await;
    match result {
        Ok(Ok(body)) => (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            Json(body),
        )
            .into_response(),
        Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
            StatusCode::RANGE_NOT_SATISFIABLE.into_response()
        }
        Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
        Ok(Err(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn regular_file(dir: &File, name: &str) -> io::Result<File> {
    let file = File::from(openat(
        dir,
        name,
        OFlag::O_RDONLY
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_NONBLOCK
            | OFlag::O_NOCTTY,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::from_raw_os_error(nix::libc::EACCES));
    }
    Ok(file)
}

fn read_json(dir: &File, name: &str) -> io::Result<Value> {
    let mut bytes = Vec::new();
    regular_file(dir, name)?
        .take(BATCH_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > BATCH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session metadata is too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn batch(file: Option<File>, offset: u64) -> io::Result<(Vec<Value>, u64, bool, bool)> {
    let Some(mut file) = file else {
        return if offset == 0 {
            Ok((Vec::new(), 0, false, false))
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        };
    };
    if offset > file.metadata()?.len() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if offset != 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        if previous[0] != b'\n' {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut next = offset;
    loop {
        let mut line = Vec::new();
        // Limit the read itself, including a malicious or corrupt unterminated line.
        reader
            .by_ref()
            .take(RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if line.len() as u64 > RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trace record exceeds 16 MiB",
            ));
        }
        if line.is_empty() {
            return Ok((events, next, false, false));
        }
        if line.last() != Some(&b'\n') {
            return Ok((events, next, false, true));
        }
        let event = serde_json::from_slice(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        next += line.len() as u64;
        events.push(event);
        if events.len() >= BATCH_EVENTS || next - offset >= BATCH_BYTES {
            let more = next < reader.get_ref().metadata()?.len();
            return Ok((events, next, more, false));
        }
    }
}
