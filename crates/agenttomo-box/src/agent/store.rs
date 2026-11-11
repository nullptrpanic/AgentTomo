//! Append complete semantic payloads, independently of network preview windows.
use super::MAX_CONTENT;
use serde_json::{Value, json};
use std::{io, path::Path};
use tokio::{fs::File, io::AsyncWriteExt, sync::Mutex};

pub const PAYLOAD_FILE: &str = "agent-payloads.bin";
const RUN_BUDGET: u64 = 512 * 1024 * 1024;
struct State {
    file: File,
    offset: u64,
    failed: bool,
}
pub struct Store(Mutex<State>);
impl Store {
    pub async fn create(directory: &Path) -> io::Result<Self> {
        Ok(Self(Mutex::new(State {
            file: crate::recording::create_log(directory, PAYLOAD_FILE).await?,
            offset: 0,
            failed: false,
        })))
    }
    pub async fn save(&self, value: &Value) -> io::Result<Value> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_CONTENT {
            return Err(io::Error::other("semantic payload exceeded 32 MiB"));
        }
        let mut inner = self.0.lock().await;
        let State {
            file,
            offset,
            failed,
        } = &mut *inner;
        if *failed {
            return Err(io::Error::other(
                "semantic payload storage previously failed",
            ));
        }
        if bytes.len() as u64 > RUN_BUDGET.saturating_sub(*offset) {
            return Err(io::Error::other(
                "512 MiB semantic payload budget exhausted",
            ));
        }
        let reference = json!({"offset":*offset,"captured_bytes":bytes.len(),"status":"complete","encoding":"json"});
        let result = async {
            file.write_all(&bytes).await?;
            file.flush().await
        }
        .await;
        if let Err(error) = result {
            *failed = true;
            return Err(error);
        }
        *offset += bytes.len() as u64;
        Ok(reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_storage_error_never_publishes_or_reuses_a_partial_range() {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .await
            .unwrap();
        let store = Store(Mutex::new(State {
            file,
            offset: 0,
            failed: false,
        }));
        assert!(store.save(&json!("payload")).await.is_err());
        assert!(
            store
                .save(&json!("next"))
                .await
                .unwrap_err()
                .to_string()
                .contains("previously failed")
        );
        assert_eq!(store.0.lock().await.offset, 0);
    }
}
