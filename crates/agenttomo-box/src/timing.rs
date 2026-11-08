//! Source observation times and duration, captured before content persistence.
use serde_json::{Value, json};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct OperationTiming {
    started: Instant,
    unix_ns: Option<u128>,
    source: &'static str,
}

fn unix_ns() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| time.as_nanos())
}

impl OperationTiming {
    pub fn begin(source: &'static str) -> Self {
        Self {
            started: Instant::now(),
            unix_ns: unix_ns(),
            source,
        }
    }

    pub fn complete(self, data: &mut Value) {
        data["started_unix_ns"] = json!(self.unix_ns);
        data["completed_unix_ns"] = json!(unix_ns());
        data["duration_ns"] = json!(self.started.elapsed().as_nanos());
        data["start_source"] = self.source.into();
    }
}
