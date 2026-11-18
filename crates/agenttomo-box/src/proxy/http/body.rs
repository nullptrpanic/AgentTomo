//! Observe Hyper's body frames without buffering the forwarding stream.
use super::{MessageContext, headers};
use crate::recording::http::Capture;
use axum::body::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Format {
    Ordinary,
    Model,
    Sse,
}

impl Format {
    pub fn response(
        headers: &axum::http::HeaderMap,
        status: axum::http::StatusCode,
        protocol: Option<crate::agent::Protocol>,
        upgrade_requested: bool,
    ) -> Self {
        if status.is_informational()
            || status.is_redirection()
            || matches!(status.as_u16(), 204 | 205)
        {
            return Self::Ordinary;
        }
        if protocol.is_some() && (status.is_client_error() || status.is_server_error()) {
            return Self::Model;
        }
        let header = headers.get("content-type");
        let media = header
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media == "text/event-stream" {
            return Self::Sse;
        }
        let Some(protocol) = protocol else {
            return Self::Ordinary;
        };
        if media == "application/json" || media.ends_with("+json") {
            return Self::Model;
        }
        if header.is_none() && status.is_success() && !upgrade_requested && protocol.expects_sse() {
            return Self::Sse;
        }
        Self::Ordinary
    }
}

pub struct Observed {
    inner: Incoming,
    capture: Option<Capture>,
    sse: Option<super::streams::sse::Observer>,
    done: bool,
    message: MessageContext,
    trailers: serde_json::Value,
    model: Option<crate::agent::Observer>,
}

impl Observed {
    pub(super) fn new(inner: Incoming, message: MessageContext, format: Format) -> Self {
        let model = (format == Format::Model
            && (message.status.is_some() || !inner.is_end_stream()))
        .then(|| message.model_observer())
        .flatten();
        let capture = (format != Format::Sse).then(|| message.session.inspection.store.capture());
        let sse = (format == Format::Sse)
            .then(|| super::streams::sse::Observer::new(&message, &message.encoding));
        let mut body = Self {
            inner,
            capture,
            sse,
            done: false,
            message,
            trailers: serde_json::json!([]),
            model,
        };
        if body.inner.is_end_stream() {
            body.finish(true);
        }
        body
    }
    fn finish(&mut self, complete: bool) {
        if self.done {
            return;
        }
        self.done = true;
        if let Some(model) = &mut self.model {
            self.message.semantic(model.finish_body(
                &self.message.encoding,
                complete,
                self.message.status,
            ));
        }
        let capture = self.capture.take();
        let streaming = self.sse.is_some();
        if let Some(sse) = &mut self.sse {
            sse.finish(complete);
        }
        let message = self.message.clone();
        let trailers = self.trailers.take();
        let completed = super::now();
        let duration = message.start.elapsed().as_nanos();
        self.message.session.writes.spawn(async move {
            let body = match capture { Some(capture) => capture.save(complete).await, None => serde_json::json!({"status":"stream_window","captured_bytes":0,"complete":complete}) };
            let _ = message.clone().emit_body(body, trailers, completed, duration).await;
            if streaming { super::streams::closed(message, "sse", complete).await; }
        });
    }
}

impl Body for Observed {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref()
                    && let Some(model) = &mut self.model
                {
                    model.feed_body(bytes);
                }
                if let Some(bytes) = frame.data_ref()
                    && let Some(capture) = &mut self.capture
                {
                    capture.observe(bytes);
                }
                if let Some(bytes) = frame.data_ref()
                    && let Some(sse) = &mut self.sse
                {
                    sse.feed(bytes);
                }
                if let Some(trailers) = frame.trailers_ref() {
                    self.trailers = headers(trailers);
                }
                if self.inner.is_end_stream() {
                    self.finish(true);
                }
            }
            Poll::Ready(None) => self.finish(true),
            Poll::Ready(Some(Err(_))) => self.finish(false),
            _ => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Observed {
    fn drop(&mut self) {
        self.finish(self.inner.is_end_stream());
    }
}

#[cfg(test)]
mod tests {
    use super::Format;
    use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};

    #[test]
    fn model_stream_fallback_respects_response_headers_and_status() {
        let protocol =
            crate::agent::Protocol::detect(&Method::POST, &"/v1/responses".parse().unwrap());
        for (content_type, status, upgrade, expected) in [
            (None, StatusCode::OK, false, Format::Sse),
            (None, StatusCode::NO_CONTENT, false, Format::Ordinary),
            (
                Some("application/json"),
                StatusCode::NO_CONTENT,
                false,
                Format::Ordinary,
            ),
            (
                Some("text/event-stream"),
                StatusCode::UNAUTHORIZED,
                false,
                Format::Model,
            ),
            (None, StatusCode::OK, true, Format::Ordinary),
            (
                Some("application/json"),
                StatusCode::TEMPORARY_REDIRECT,
                false,
                Format::Ordinary,
            ),
            (None, StatusCode::UNAUTHORIZED, false, Format::Model),
            (
                Some("Application/Problem+JSON; charset=utf-8"),
                StatusCode::OK,
                false,
                Format::Model,
            ),
            (Some("text/plain"), StatusCode::OK, false, Format::Ordinary),
            (
                Some("text/event-stream"),
                StatusCode::OK,
                false,
                Format::Sse,
            ),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(value) = content_type {
                headers.insert("content-type", HeaderValue::from_static(value));
            }
            assert_eq!(
                Format::response(&headers, status, protocol, upgrade),
                expected
            );
        }
    }
}
