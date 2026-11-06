//! Bounded body prefixes. Payload retention never determines forwarding length.
use super::create_log;
use serde_json::{Value, json};
use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{fs::File, io::AsyncWriteExt, sync::Mutex};

pub const PAYLOAD_FILE: &str = "http-bodies.bin";
pub const MAX_BODY_BYTES: u32 = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: u32,
    pub total_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 65536,
            total_bytes: 64 * 1024 * 1024,
        }
    }
}

struct Sink {
    file: File,
    offset: u64,
    failed: bool,
}
pub struct Store {
    sink: Mutex<Sink>,
    remaining: AtomicU64,
    limits: Limits,
}

impl Store {
    pub async fn create(directory: &Path, limits: Limits) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            sink: Mutex::new(Sink {
                file: create_log(directory, PAYLOAD_FILE).await?,
                offset: 0,
                failed: false,
            }),
            remaining: AtomicU64::new(limits.total_bytes),
            limits,
        }))
    }
    pub fn capture(self: &Arc<Self>) -> Capture {
        Capture {
            store: self.clone(),
            bytes: Vec::new(),
            seen: 0,
            budget_limited: false,
        }
    }
}

pub struct Capture {
    store: Arc<Store>,
    bytes: Vec<u8>,
    seen: u64,
    budget_limited: bool,
}
impl Capture {
    pub fn observe(&mut self, data: &[u8]) {
        self.seen = self.seen.saturating_add(data.len() as u64);
        let wanted =
            data.len()
                .min(self.store.limits.max_bytes as usize - self.bytes.len()) as u64;
        let before = self
            .store
            .remaining
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                Some(remaining.saturating_sub(wanted))
            })
            .unwrap();
        let keep = before.min(wanted) as usize;
        self.budget_limited |= keep < wanted as usize;
        self.bytes.extend_from_slice(&data[..keep]);
    }

    pub async fn save(self, complete: bool) -> Value {
        let truncated = self.seen > self.bytes.len() as u64;
        let disabled = self.store.limits.max_bytes == 0 || self.store.limits.total_bytes == 0;
        let mut data = json!({
            "status": if disabled { "disabled" } else if !complete { "incomplete" } else if truncated { "truncated" } else { "complete" },
            "complete":complete, "truncated":truncated, "observed_bytes":self.seen,
            "captured_bytes":self.bytes.len(), "limit_bytes":self.store.limits.max_bytes,
            "limit_reason":if disabled { Some("disabled") } else if self.budget_limited { Some("run_budget") } else if truncated { Some("body_limit") } else { None },
            "representation":"entity_bytes", "content_decoded":false,
        });
        let mut sink = self.store.sink.lock().await;
        let result = async {
            if sink.failed {
                return Err(io::Error::other("payload store previously failed"));
            }
            let offset = sink.offset;
            sink.file.write_all(&self.bytes).await?;
            sink.file.flush().await?;
            sink.offset += self.bytes.len() as u64;
            Ok::<_, io::Error>(offset)
        }
        .await;
        match result {
            Ok(offset) => data["offset"] = offset.into(),
            Err(error) => {
                sink.failed = true;
                data["status"] = "unavailable".into();
                data["error"] = error.to_string().into();
                data["captured_bytes"] = 0.into();
            }
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retention_bounds_are_shared_and_empty_is_not_missing() {
        let dir =
            std::env::temp_dir().join(format!("agenttomo-http-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let store = Store::create(
            &dir,
            Limits {
                max_bytes: 4,
                total_bytes: 5,
            },
        )
        .await
        .unwrap();
        let mut a = store.capture();
        let mut b = store.capture();
        a.observe(b"123456");
        b.observe(b"abcdef");
        let a = a.save(true).await;
        let b = b.save(false).await;
        assert_eq!(a["captured_bytes"], 4);
        assert_eq!(a["observed_bytes"], 6);
        assert_eq!(b["captured_bytes"], 1);
        assert_eq!(b["limit_reason"], "run_budget");
        assert_eq!(b["status"], "incomplete");
        let empty = store.capture().save(true).await;
        assert_eq!(empty["status"], "complete");
        assert_eq!(empty["captured_bytes"], 0);
        assert_eq!(std::fs::read(dir.join(PAYLOAD_FILE)).unwrap(), b"1234a");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
