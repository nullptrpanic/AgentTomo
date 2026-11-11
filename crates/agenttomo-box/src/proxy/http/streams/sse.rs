use super::{Prefix, Recorder, inflate};
use crate::proxy::http::MessageContext;
use flate2::Decompress;
use serde_json::json;
use sse_parse::SseParser;

pub(in crate::proxy::http) struct Observer {
    parser: SseParser,
    recorder: Recorder,
    decoder: Option<Decompress>,
    failed: bool,
    start: Option<u128>,
    line_has_bytes: bool,
    skip_lf: bool,
    model: Option<crate::agent::Observer>,
    context: MessageContext,
}
impl Observer {
    pub fn new(message: &MessageContext, encoding: &str) -> Self {
        let recorder = Recorder::new(message, "sse", "downstream");
        let (decoder, failed) = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => (None, false),
            "gzip" => (Some(Decompress::new_gzip(15)), false),
            "deflate" => (Some(Decompress::new(true)), false),
            _ => {
                recorder.error("unsupported SSE Content-Encoding; forwarding continues");
                (None, true)
            }
        };
        Self {
            parser: SseParser::new().max_buffer(if message.agent.is_some() {
                crate::agent::MAX_CONTENT
            } else {
                1024 * 1024
            }),
            recorder,
            decoder,
            failed,
            start: None,
            line_has_bytes: false,
            skip_lf: false,
            model: message.model_observer(),
            context: message.clone(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.failed {
            return;
        }
        let mut decoder = self.decoder.take();
        for chunk in bytes.chunks(8192) {
            if let Some(decoder) = &mut decoder {
                if let Err(error) = inflate::feed(decoder, chunk, &mut |bytes| self.parsed(bytes)) {
                    if let Some(model) = &self.model {
                        self.context.semantic(model.failure(&error));
                    }
                    self.recorder.error(&error);
                    self.failed = true;
                    break;
                }
            } else {
                self.parsed(chunk);
            }
        }
        self.decoder = decoder;
    }
    fn parsed(&mut self, bytes: &[u8]) {
        let now = crate::proxy::http::now();
        // Feed the library at line boundaries solely to timestamp each block's
        // first bytes. The library still owns SSE fields, UTF-8 and dispatch.
        for piece in bytes.split_inclusive(|byte| matches!(byte, b'\r' | b'\n')) {
            if self.skip_lf && piece == b"\n" {
                self.parser.feed(piece);
                self.skip_lf = false;
                continue;
            }
            self.skip_lf = false;
            let terminated = piece
                .last()
                .is_some_and(|byte| matches!(byte, b'\r' | b'\n'));
            let empty_line = terminated && piece.len() == 1 && !self.line_has_bytes;
            self.parse_piece(piece, now);
            if terminated {
                self.line_has_bytes = false;
                self.skip_lf = piece.last() == Some(&b'\r');
                if empty_line {
                    self.start = None;
                }
            } else {
                self.line_has_bytes = true;
            }
        }
    }
    fn parse_piece(&mut self, bytes: &[u8], now: u128) {
        let started = *self.start.get_or_insert(now);
        let before = self.parser.overflows();
        self.parser.feed(bytes);
        if self.parser.overflows() != before {
            if let Some(model) = &self.model {
                self.context.semantic(model.failure(
                    "model SSE block exceeded 32 MiB; complete tool evidence is unavailable",
                ));
            }
            self.recorder.error(
                "SSE block exceeded the configured parser limit; skipped until the next event",
            );
            self.start = None;
        }
        while let Some(event) = self.parser.next_event() {
            if let Some(model) = &mut self.model {
                self.context.semantic(model.event(&event.data));
            }
            let mut prefix = Prefix::new(self.recorder.store.limits.message_bytes as usize);
            prefix.feed(event.data.as_bytes());
            let mut metadata = prefix.metadata();
            metadata["kind"] = "event".into();
            metadata["event"] = event.event.chars().take(1024).collect::<String>().into();
            metadata["event_id"] = event.id.chars().take(1024).collect::<String>().into();
            metadata["metadata_truncated"] =
                (event.event.chars().count() > 1024 || event.id.chars().count() > 1024).into();
            metadata["retry_ms"] = json!(event.retry.map(|duration| duration.as_millis()));
            metadata["started_unix_ns"] = json!(started);
            metadata["completed_unix_ns"] = json!(now);
            metadata["start_source"] = "proxy_body_chunk".into();
            metadata["complete"] = true.into();
            self.recorder.record(metadata, prefix.bytes);
        }
    }
    pub fn finish(&mut self, complete: bool) {
        if let Some(model) = &self.model {
            self.context.semantic(model.finish_response(complete));
        }
        self.parser.finish();
    }
}
