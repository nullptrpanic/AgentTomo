//! Bounded semantic observation independent of HTTP preview retention.
use super::{
    Hub, MAX_CONTENT, Scope,
    protocol::{Event, ModelDecoder, Protocol},
};
use crate::content_encoding::ContentEncoding;
use serde_json::json;
use std::io;

pub struct Observer {
    scope: Scope,
    hub: Hub,
    body: Vec<u8>,
    oversized: bool,
    response: Box<dyn ModelDecoder>,
    response_complete: bool,
}
impl Observer {
    pub fn new(scope: Scope, hub: Hub, protocol: Protocol) -> Self {
        Self {
            scope,
            hub,
            body: Vec::new(),
            oversized: false,
            response: protocol.decoder(),
            response_complete: false,
        }
    }
    pub fn feed_body(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_CONTENT.saturating_sub(self.body.len()) {
            self.oversized = true;
            self.body.clear();
        } else if !self.oversized {
            self.body.extend_from_slice(bytes);
        }
    }
    pub fn finish_body(
        &mut self,
        encoding: &str,
        complete: bool,
        status: Option<axum::http::StatusCode>,
    ) -> Vec<Event> {
        let body = std::mem::take(&mut self.body);
        let result = if std::mem::take(&mut self.oversized) {
            Err(io::Error::other(
                "model body exceeded 32 MiB semantic limit",
            ))
        } else if !complete {
            Err(io::Error::other("model body was interrupted"))
        } else {
            decode(body, encoding).and_then(|bytes| match status {
                Some(status) => self.response.http_response(status, &bytes),
                None => self.response.request(&bytes),
            })
        };
        let events = match result {
            Ok(events) => events,
            Err(error) => vec![self.error(error.to_string())],
        };
        self.decorate(events)
    }
    pub fn event(&mut self, data: &str) -> Vec<Event> {
        let events = match self.response.event(data) {
            Ok(events) => events,
            Err(error) => vec![self.error(error.to_string())],
        };
        if events
            .iter()
            .any(|event| event.kind == "agent.model.started")
        {
            self.response_complete = false;
        }
        if events
            .iter()
            .any(|event| event.kind == "agent.model.complete")
        {
            self.response_complete = true;
        }
        self.decorate(events)
    }
    pub fn finish_response(&self, complete: bool) -> Vec<Event> {
        if complete && self.response_complete {
            Vec::new()
        } else {
            self.failure("model SSE ended without a complete response")
        }
    }
    pub fn websocket_request(&mut self, bytes: &[u8]) -> Vec<Event> {
        let events = match self.response.websocket_request(bytes) {
            Ok(events) => events,
            Err(error) => vec![self.error(error.to_string())],
        };
        self.decorate(events)
    }
    pub fn failure(&self, error: &str) -> Vec<Event> {
        self.decorate(vec![self.error(error.to_owned())])
    }
    fn error(&self, error: String) -> Event {
        Event {
            kind: "agent.capture.error",
            data: json!({"error":error}),
            content: None,
        }
    }
    fn decorate(&self, events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter_map(|mut event| {
                if event.kind == "agent.message.delta"
                    && let Some(delta) = event.data["delta"].as_str()
                {
                    let prefix = delta.chars().take(8192).collect::<String>();
                    event.data["delta_truncated"] = (prefix.len() < delta.len()).into();
                    event.data["delta"] = prefix.into();
                }
                let mut scope = self.scope.clone();
                if let Some(external) = event.data["call_id"].as_str().map(str::to_owned) {
                    if event.kind == "agent.tool.result"
                        && !self.hub.first_result(&scope, &external)
                    {
                        return None;
                    }
                    let (id, original_scope) = self.hub.call(
                        &scope,
                        &external,
                        matches!(event.kind, "agent.tool.started" | "agent.tool.call")
                            && event.data["execution"] != "provider_or_native",
                    );
                    if event.kind == "agent.tool.result" {
                        self.hub.returned(&original_scope, &id);
                    }
                    event.data["agenttomo_toolcall_id"] = id.into();
                    scope = original_scope;
                }
                event.data["task_id"] = scope.task_id.into();
                event.data["agent_instance_id"] = scope.agent_instance_id.into();
                event.data["captured_unix_ns"] = json!(super::now());
                Some(event)
            })
            .collect()
    }
}

fn decode(bytes: Vec<u8>, encoding: &str) -> io::Result<Vec<u8>> {
    let encoding = ContentEncoding::parse(encoding)
        .ok_or_else(|| io::Error::other("unsupported model body Content-Encoding"))?;
    if encoding == ContentEncoding::Identity {
        return Ok(bytes); // feed_body already enforces MAX_CONTENT.
    }
    let mut output = Vec::new();
    encoding.read(&bytes, MAX_CONTENT + 1, &mut output)?;
    if output.len() > MAX_CONTENT {
        return Err(io::Error::other(
            "decoded model body exceeded 32 MiB semantic limit",
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn responses() -> Protocol {
        Protocol::detect(&axum::http::Method::POST, &"/v1/responses".parse().unwrap()).unwrap()
    }
    #[test]
    fn model_body_accepts_the_gzip_alias_used_by_traffic_preview() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(b"model request").unwrap();
        let compressed = gzip.finish().unwrap();
        assert_eq!(decode(compressed, " X-GZip ").unwrap(), b"model request");
    }

    #[test]
    fn identity_decoding_reuses_the_captured_buffer() {
        let body = vec![1; 65536];
        let original = body.as_ptr();
        let decoded = decode(body, "identity").unwrap();
        assert_eq!(decoded.as_ptr(), original);
    }
    #[test]
    fn returning_a_tool_releases_the_active_fallback_for_later_unrelated_children() {
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let hub = Hub::default();
        hub.exec(
            1,
            &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"}),
            true,
        );
        let mut observer = Observer::new(scope, hub.clone(), responses());
        observer.event(r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"item1","call_id":"call1","name":"exec","input":""}}"#);
        observer.feed_body(
            br#"{"input":[{"type":"custom_tool_call_output","call_id":"call1","output":"done"}]}"#,
        );
        observer.finish_body("", true, None);
        hub.fork(1, 2);
        assert_eq!(hub.binding(2).unwrap().agenttomo_toolcall_id, None);
    }
    #[test]
    fn zstd_requests_preserve_large_complete_tool_outputs_and_deduplicate_history() {
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let hub = Hub::default();
        let mut observer = Observer::new(scope, hub, responses());
        let output = "result\n".repeat(20000);
        let raw = serde_json::to_vec(&json!({"input":[{"type":"custom_tool_call_output","call_id":"call1","output":output}]})).unwrap();
        let compressed = zstd::stream::encode_all(&raw[..], 1).unwrap();
        for chunk in compressed.chunks(13) {
            observer.feed_body(chunk);
        }
        let events = observer.finish_body("zstd", true, None);
        assert_eq!(events[1].content.as_ref().unwrap(), &output);
        for chunk in compressed.chunks(11) {
            observer.feed_body(chunk);
        }
        assert_eq!(observer.finish_body("zstd", true, None).len(), 1);
    }
    #[test]
    fn malformed_and_interrupted_content_reports_capture_errors() {
        let mut observer = Observer::new(
            Scope {
                agent_instance_id: "a".into(),
                task_id: "t".into(),
            },
            Hub::default(),
            responses(),
        );
        observer.feed_body(b"bad");
        assert_eq!(
            observer.finish_body("zstd", true, None)[0].kind,
            "agent.capture.error"
        );
        assert_eq!(
            observer.finish_body("", false, None)[0].kind,
            "agent.capture.error"
        );
    }
}
