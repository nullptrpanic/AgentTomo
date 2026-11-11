//! Byte-bounded file snapshots. JSONL contains ranges, never inline file bodies.

use crate::collectors::file::Opened;
use moka::sync::Cache;
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
            total_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct Store {
    file: Option<File>,
    limits: Limits,
    used: u64,
    failed: bool,
    exclude_task_trust: bool,
    // Exact captured bytes are the key, so path reuse or in-place edits cannot
    // return stale content. Bound memory independently of the on-disk budget.
    snapshots: Cache<Vec<u8>, u64>,
}

impl Store {
    pub async fn create(
        directory: &Path,
        limits: Limits,
        exclude_task_trust: bool,
    ) -> io::Result<Self> {
        Ok(Self {
            file: if limits.enabled() {
                Some(super::create_log(directory, PAYLOAD_FILE).await?)
            } else {
                None
            },
            limits,
            used: 0,
            failed: false,
            exclude_task_trust,
            snapshots: Cache::builder()
                .max_capacity(16 * 1024 * 1024)
                .weigher(|bytes: &Vec<u8>, _: &u64| bytes.len() as u32 + 128)
                .build(),
        })
    }

    pub async fn capture(&mut self, opened: Opened) -> Value {
        let Opened { mut data, content } = opened;
        if data["resolved_path"].as_str().is_some_and(|path| {
            path.starts_with("/root/.agenttomo-codex/")
                || path.starts_with("/root/.agenttomo-config/")
                || path.starts_with("/records/codex/")
        }) {
            data["preview"] = json!({"status":"unavailable","source":"open_snapshot","reason":"agent_private_state"});
            return data;
        }
        if self.exclude_task_trust
            && self.limits.enabled()
            && data["resolved_path"].as_str().is_some_and(trust_material)
        {
            data["preview"] = json!({
                "status": "unavailable", "source": "open_snapshot", "reason": "task_trust_material"
            });
            return data;
        }
        let Some(file) = content else {
            return data;
        };
        let preview = &mut data["preview"];
        let remaining = self.limits.total_bytes.saturating_sub(self.used);
        let unavailable = if !self.limits.enabled() {
            Some("disabled")
        } else if self.failed {
            Some("storage_error")
        } else {
            None
        };
        if let Some(reason) = unavailable {
            preview["reason"] = reason.into();
            return data;
        }
        let mut limit = self.limits.max_bytes as usize;
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
        let mut truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let offset = if let Some(offset) = self.snapshots.get(bytes.as_slice()) {
            preview["reused"] = true.into();
            offset
        } else {
            if remaining == 0 && !bytes.is_empty() {
                preview["reason"] = "budget_exhausted".into();
                return data;
            }
            limit = remaining.min(limit as u64) as usize;
            truncated |= bytes.len() > limit;
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
            let offset = self.used;
            self.used += bytes.len() as u64;
            self.snapshots.insert(bytes.clone(), offset);
            offset
        };
        preview["status"] = if truncated { "truncated" } else { "complete" }.into();
        preview["offset"] = offset.into();
        preview["captured_bytes"] = bytes.len().into();
        preview["limit_bytes"] = limit.into();
        preview["captured_unix_ns"] = json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        if truncated {
            preview["limit_reason"] = if limit < self.limits.max_bytes as usize {
                "run_budget"
            } else {
                "file_limit"
            }
            .into();
        }
        data
    }
}

fn trust_material(path: &str) -> bool {
    if matches!(
        path,
        "/etc/ssl/certs/ca-certificates.crt" | "/etc/ssl/certs/agenttomo-task-ca.pem"
    ) {
        return true;
    }
    let Some((directory, filename)) = path.rsplit_once('/') else {
        return false;
    };
    let nss = directory.ends_with("/.pki/nssdb") || directory.ends_with("/.local/share/pki/nssdb");
    nss && matches!(
        filename,
        "cert9.db" | "cert9.db-wal" | "cert9.db-journal" | "cert9.db-shm" | "cert8.db"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("agenttomo-preview-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        async fn capture(&self, store: &mut Store, bytes: &[u8]) -> Value {
            let path = self.0.join("example");
            std::fs::write(&path, bytes).unwrap();
            store.capture(Opened {
                data: json!({"resolved_path": path, "preview": {"status": "unavailable", "source": "open_snapshot"}}),
                content: Some(std::fs::File::open(path).unwrap()),
            }).await["preview"].clone()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn repeated_content_preserves_budget_and_changed_content_gets_a_new_snapshot() {
        let dir = Directory::new();
        let mut store = Store::create(
            &dir.0,
            Limits {
                max_bytes: 4,
                total_bytes: 8,
            },
            false,
        )
        .await
        .unwrap();
        let first = dir.capture(&mut store, b"abcd").await;
        for _ in 0..30 {
            let again = dir.capture(&mut store, b"abcd").await;
            assert_eq!(again["status"], "complete");
            assert_eq!(again["offset"], first["offset"]);
        }
        let changed = dir.capture(&mut store, b"efgh").await;
        assert_eq!(changed["status"], "complete");
        assert_ne!(changed["offset"], first["offset"]);
        // Reuse still works when the budget is full; unseen content stays bounded.
        let again = dir.capture(&mut store, b"abcd-more").await;
        assert_eq!(again["offset"], first["offset"]);
        assert_eq!(again["status"], "truncated");
        assert_eq!(again["limit_reason"], "file_limit");
        let unavailable = dir.capture(&mut store, b"ijkl").await;
        assert_eq!(unavailable["reason"], "budget_exhausted");
        assert!(unavailable["offset"].is_null());
        assert_eq!(
            std::fs::read(dir.0.join(PAYLOAD_FILE)).unwrap(),
            b"abcdefgh"
        );
    }

    #[tokio::test]
    async fn partial_budget_and_empty_snapshots_keep_accurate_ranges() {
        let dir = Directory::new();
        let mut store = Store::create(
            &dir.0,
            Limits {
                max_bytes: 4,
                total_bytes: 6,
            },
            false,
        )
        .await
        .unwrap();
        dir.capture(&mut store, b"abcd").await;
        let partial = dir.capture(&mut store, b"efgh").await;
        assert_eq!(partial["captured_bytes"], 2);
        assert_eq!(partial["limit_reason"], "run_budget");
        let empty = dir.capture(&mut store, b"").await;
        assert_eq!(empty["status"], "complete");
        assert_eq!(empty["captured_bytes"], 0);
        assert_eq!(std::fs::read(dir.0.join(PAYLOAD_FILE)).unwrap(), b"abcdef");
    }
}
