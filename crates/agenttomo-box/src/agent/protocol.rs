//! Model protocols normalize wire data without knowing which agent sent it.
use super::responses;
use axum::http::{Method, Uri};
use serde_json::Value;
use std::io;

const PROTOCOLS: &[Protocol] = &[Protocol::new(&responses::Responses)];

pub struct Event {
    pub kind: &'static str,
    pub data: Value,
    pub content: Option<Value>,
}

/// Normalized `agent.message` semantics. Unknown leaves final-answer inference
/// to the task projection; it does not assert that the message is intermediate.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Intermediate,
    Final,
    Unknown,
}

/// Stateless recognition; mutable parsing state is allocated per observed stream.
pub trait ModelProtocol: Send + Sync {
    fn matches(&self, method: &Method, uri: &Uri) -> bool;
    fn decoder(&self) -> Box<dyn ModelDecoder>;
    fn expects_sse(&self) -> bool;
}

/// Receives decompressed bodies and framed payloads. Payload syntax and vendor
/// sentinels belong here; forwarding, limits and evidence correlation do not.
pub trait ModelDecoder: Send {
    fn request(&mut self, bytes: &[u8]) -> io::Result<Vec<Event>>;
    fn event(&mut self, data: &str) -> io::Result<Vec<Event>>;
    fn websocket_request(&mut self, bytes: &[u8]) -> io::Result<Vec<Event>>;
}

#[derive(Clone, Copy)]
pub struct Protocol(&'static dyn ModelProtocol);

impl Protocol {
    pub const fn new(plugin: &'static dyn ModelProtocol) -> Self {
        Self(plugin)
    }

    pub fn detect(method: &Method, uri: &Uri) -> Option<Self> {
        PROTOCOLS
            .iter()
            .copied()
            .find(|protocol| protocol.0.matches(method, uri))
    }
    pub fn decoder(self) -> Box<dyn ModelDecoder> {
        self.0.decoder()
    }
    pub fn expects_sse(self) -> bool {
        self.0.expects_sse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Hub, Observer, Scope};
    use serde_json::json;
    use std::io;

    struct PlainProtocol;
    struct PlainDecoder {
        sequence: usize,
    }

    impl ModelProtocol for PlainProtocol {
        fn matches(&self, method: &Method, uri: &Uri) -> bool {
            *method == Method::POST && uri.path() == "/plain"
        }
        fn decoder(&self) -> Box<dyn ModelDecoder> {
            Box::new(PlainDecoder { sequence: 0 })
        }
        fn expects_sse(&self) -> bool {
            false
        }
    }

    impl ModelDecoder for PlainDecoder {
        fn request(&mut self, bytes: &[u8]) -> io::Result<Vec<Event>> {
            Ok(vec![Event {
                kind: "agent.model.request",
                data: json!({}),
                content: Some(String::from_utf8_lossy(bytes).into_owned().into()),
            }])
        }
        fn event(&mut self, data: &str) -> io::Result<Vec<Event>> {
            self.sequence += 1;
            Ok(vec![Event {
                kind: if data == "finished" {
                    "agent.model.complete"
                } else {
                    "agent.message"
                },
                data: json!({"sequence": self.sequence}),
                content: Some(data.into()),
            }])
        }
        fn websocket_request(&mut self, bytes: &[u8]) -> io::Result<Vec<Event>> {
            self.request(bytes)
        }
    }

    fn observer(protocol: Protocol) -> Observer {
        Observer::new(
            Scope {
                agent_instance_id: "a".into(),
                task_id: "t".into(),
            },
            Hub::default(),
            protocol,
        )
    }

    #[test]
    fn selected_decoder_owns_payload_syntax_and_completion_sentinels() {
        let mut observer = observer(Protocol::new(&PlainProtocol));
        observer.feed_request(b"plain request, not JSON");
        let request = observer.finish_request("identity", true);
        assert_eq!(request[0].kind, "agent.model.request");
        assert_eq!(request[0].content, Some("plain request, not JSON".into()));
        assert_eq!(request[0].data["task_id"], "t");
        let event = observer.event("[DONE]");
        assert_eq!(event[0].content, Some("[DONE]".into()));
        assert_eq!(
            observer.websocket_request(b"plain frame")[0].content,
            Some("plain frame".into())
        );
        observer.event("finished");
        assert!(observer.finish_response(true).is_empty());
    }

    #[test]
    fn streams_get_independent_decoder_state() {
        let protocol = Protocol::new(&PlainProtocol);
        let mut first = observer(protocol);
        let mut second = observer(protocol);
        assert_eq!(first.event("first")[0].data["sequence"], 1);
        assert_eq!(first.event("second")[0].data["sequence"], 2);
        assert_eq!(second.event("first")[0].data["sequence"], 1);

        let protocol = Protocol::detect(&Method::POST, &"/v1/responses".parse().unwrap()).unwrap();
        let mut first = observer(protocol);
        let mut second = observer(protocol);
        let data = r#"{"type":"response.output_item.done","item":{"id":"same-id","type":"message","content":[{"type":"output_text","text":"answer"}]}}"#;
        assert_eq!(first.event(data).len(), 1);
        assert!(first.event(data).is_empty());
        assert_eq!(second.event(data).len(), 1);
        assert!(second.event("[DONE]").is_empty());
    }
    #[test]
    fn responses_detection_is_protocol_based_and_does_not_claim_other_operations() {
        for path in [
            "/v1/responses",
            "/backend-api/codex/responses",
            "/custom/responses/",
        ] {
            for method in [Method::GET, Method::POST] {
                assert!(Protocol::detect(&method, &path.parse().unwrap()).is_some());
            }
        }
        for path in [
            "/v1/responses/compact",
            "/v1/messages",
            "/v1/not-responses",
            "/weather",
        ] {
            assert!(Protocol::detect(&Method::POST, &path.parse().unwrap()).is_none());
        }
        assert!(Protocol::detect(&Method::DELETE, &"/v1/responses".parse().unwrap()).is_none());
    }
}
