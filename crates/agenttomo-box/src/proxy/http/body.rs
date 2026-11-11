//! Observe Hyper's body frames without buffering the forwarding stream.
use super::{MessageContext, headers};
use crate::recording::http::Capture;
use axum::body::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

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
    pub fn new(inner: Incoming, message: MessageContext, encoding: Option<String>) -> Self {
        let model = (!message.response && !inner.is_end_stream())
            .then(|| message.model_observer())
            .flatten();
        let capture = encoding
            .is_none()
            .then(|| message.session.inspection.store.capture());
        let sse = encoding.map(|encoding| super::streams::sse::Observer::new(&message, &encoding));
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
            self.message
                .semantic(model.finish_request(&self.message.encoding, complete));
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
                    model.feed_request(bytes);
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
