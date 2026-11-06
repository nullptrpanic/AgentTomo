//! Byte-bounded file snapshots. JSONL contains ranges, never inline file bodies.

use crate::collectors::file::Opened;
use serde_json::{Value, json};
use std::{
    io,
    os::unix::fs::FileExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{fs::File, io::AsyncWriteExt};

pub const MAX_PREVIEW_BYTES: u32 = 1024 * 1024;
pub const PAYLOAD_FILE: &str = "file-previews.bin";

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_bytes: u32,
    pub total_bytes: u64,
}

impl Limits {
    pub fn enabled(self) -> bool {
        self.max_bytes != 0 && self.total_bytes != 0
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024,
            total_bytes: 16 * 1024 * 1024,
        }
    }
}

pub struct Store {
    file: Option<File>,
    limits: Limits,
    used: u64,
    failed: bool,
}

impl Store {
    pub async fn create(directory: &Path, limits: Limits) -> io::Result<Self> {
        Ok(Self {
            file: if limits.enabled() {
                Some(super::create_log(directory, PAYLOAD_FILE).await?)
            } else {
                None
            },
            limits,
            used: 0,
            failed: false,
        })
    }

    pub async fn capture(&mut self, opened: Opened) -> Value {
        let Opened { mut data, content } = opened;
        let Some(file) = content else {
            return data;
        };
        let preview = &mut data["preview"];
        let remaining = self.limits.total_bytes.saturating_sub(self.used);
        let unavailable = if !self.limits.enabled() {
            Some("disabled")
        } else if self.failed {
            Some("storage_error")
        } else if remaining == 0 {
            Some("budget_exhausted")
        } else {
            None
        };
        if let Some(reason) = unavailable {
            preview["reason"] = reason.into();
            return data;
        }
        let limit = remaining.min(u64::from(self.limits.max_bytes)) as usize;
        // This independent handle does not share the application's seek offset.
        // The blocking filesystem read runs outside the tracer/runtime thread.
        let read = tokio::task::spawn_blocking(move || {
            let mut bytes = vec![0; limit + 1];
            let mut filled = 0;
            while filled < bytes.len() {
                match file.read_at(&mut bytes[filled..], filled as u64) {
                    Ok(0) => break,
                    Ok(count) => filled += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            bytes.truncate(filled);
            Ok(bytes)
        })
        .await
        .map_err(io::Error::other)
        .and_then(|result| result);
        let mut bytes = match read {
            Ok(bytes) => bytes,
            Err(error) => {
                preview["reason"] = "read_error".into();
                preview["error"] = error.to_string().into();
                return data;
            }
        };
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let file = self.file.as_mut().expect("enabled snapshot store");
        let result = async {
            file.write_all(&bytes).await?;
            file.flush().await
        }
        .await;
        if let Err(error) = result {
            self.failed = true;
            preview["reason"] = "storage_error".into();
            preview["error"] = error.to_string().into();
            return data;
        }
        preview["status"] = if truncated { "truncated" } else { "complete" }.into();
        preview["offset"] = self.used.into();
        preview["captured_bytes"] = bytes.len().into();
        preview["limit_bytes"] = limit.into();
        preview["captured_unix_ns"] = json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        if truncated {
            preview["limit_reason"] = if remaining < u64::from(self.limits.max_bytes) {
                "run_budget"
            } else {
                "file_limit"
            }
            .into();
        }
        self.used += bytes.len() as u64;
        data
    }
}
