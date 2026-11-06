//! Observers of forwarded stream bytes; never protocol endpoints.
mod inflate;
pub(super) mod sse;
pub(super) mod websocket;

use super::MessageContext;
use crate::recording::streams::{Message, Store};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Recorder {
    store: Arc<Store>,
    request_id: u64,
    base: Value,
}
impl Recorder {
    fn new(message: &MessageContext, protocol: &str, direction: &str) -> Self {
        let mut base = message
            .session
            .context
            .event("network.stream.message", json!({}))
            .data;
        base["request_id"] = message.request_id.into();
        base["protocol"] = protocol.into();
        base["direction"] = direction.into();
        Self {
            store: message.session.inspection.streams.clone(),
            request_id: message.request_id,
            base,
        }
    }
    fn record(&self, mut data: Value, payload: Vec<u8>) {
        data.as_object_mut()
            .unwrap()
            .extend(self.base.as_object().unwrap().clone());
        self.store.record(Message {
            request_id: self.request_id,
            data,
            payload,
        });
    }
    fn error(&self, reason: &str) {
        self.record(json!({"kind":"inspection_error","error":reason,"started_unix_ns":super::now(),"start_source":"proxy_observer","captured_bytes":0}), Vec::new());
    }
}

struct Prefix {
    bytes: Vec<u8>,
    seen: u64,
    limit: usize,
}
impl Prefix {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            seen: 0,
            limit,
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        let take = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..take]);
    }
    fn metadata(&self) -> Value {
        json!({"observed_bytes":self.seen,"captured_bytes":self.bytes.len(),"truncated":self.seen>self.bytes.len() as u64})
    }
}

pub(super) async fn closed(message: MessageContext, protocol: &str, complete: bool) {
    let error = message
        .session
        .inspection
        .streams
        .flush()
        .await
        .err()
        .map(|error| error.to_string());
    let _ = message.emit("network.stream.closed", json!({"protocol":protocol,"complete":complete,"error":error,"completed_unix_ns":super::now(),"duration_ns":message.start.elapsed().as_nanos()})).await;
}
