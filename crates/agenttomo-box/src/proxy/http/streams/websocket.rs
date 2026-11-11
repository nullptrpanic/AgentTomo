use super::{Prefix, Recorder, inflate};
use crate::proxy::http::{MessageContext, io::BoxIo};
use flate2::Decompress;
use serde_json::json;
use std::{
    io::{self, Cursor},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tungstenite::protocol::frame::{
    FrameHeader,
    coding::{Control, Data, OpCode},
};

struct Message {
    kind: &'static str,
    body: Prefix,
    started: u128,
    frames: u64,
    compressed: bool,
    decoded: bool,
    semantic: Option<Prefix>,
}
impl Message {
    fn feed(&mut self, bytes: &[u8]) {
        self.body.feed(bytes);
        if let Some(semantic) = &mut self.semantic {
            semantic.feed(bytes);
        }
    }
}
struct Frame {
    header: FrameHeader,
    length: u64,
    received: u64,
    control: Option<Message>,
}
struct Observer {
    recorder: Recorder,
    header: Vec<u8>,
    frame: Option<Frame>,
    message: Option<Message>,
    inflater: Decompress,
    compression: bool,
    unknown_extension: bool,
    reset_context: bool,
    client: bool,
    failed: bool,
    started: u128,
    model: Option<crate::agent::Observer>,
    context: Option<MessageContext>,
}
impl Observer {
    fn new(recorder: Recorder, extensions: &str, client: bool) -> Self {
        let extensions: Vec<_> = extensions
            .split(',')
            .filter(|part| !part.trim().is_empty())
            .collect();
        let compression = extensions.len() == 1
            && extensions[0]
                .split(';')
                .next()
                .unwrap()
                .trim()
                .eq_ignore_ascii_case("permessage-deflate");
        let unknown_extension = !extensions.is_empty() && !compression;
        let reset_context = extensions
            .iter()
            .flat_map(|part| part.split(';'))
            .any(|part| {
                part.trim().eq_ignore_ascii_case(if client {
                    "client_no_context_takeover"
                } else {
                    "server_no_context_takeover"
                })
            });
        Self {
            recorder,
            header: Vec::with_capacity(14),
            frame: None,
            message: None,
            inflater: Decompress::new(false),
            compression,
            unknown_extension,
            reset_context,
            client,
            failed: false,
            started: 0,
            model: None,
            context: None,
        }
    }
    fn feed(&mut self, mut bytes: &[u8]) {
        if self.failed {
            return;
        }
        while !bytes.is_empty() {
            if self.frame.is_none() {
                if self.header.is_empty() {
                    self.started = crate::proxy::http::now();
                }
                self.header.push(bytes[0]);
                bytes = &bytes[1..];
                let parsed = FrameHeader::parse(&mut Cursor::new(&self.header));
                match parsed {
                    Ok(Some((header, length))) => {
                        self.header.clear();
                        if let Err(reason) = self.begin(header, length) {
                            self.fail(&reason);
                            return;
                        }
                        if length == 0 {
                            self.end_frame();
                        }
                    }
                    Ok(None) if self.header.len() < 14 => {}
                    _ => {
                        self.fail(
                            "invalid WebSocket header; observation stopped, forwarding continues",
                        );
                        return;
                    }
                }
                continue;
            }
            let frame = self.frame.as_mut().unwrap();
            let count = bytes
                .len()
                .min((frame.length - frame.received).min(8192) as usize);
            let mut plain = [0; 8192];
            plain[..count].copy_from_slice(&bytes[..count]);
            if let Some(mask) = frame.header.mask {
                // Apply the RFC's repeating XOR to the retained observation only.
                for (index, byte) in plain[..count].iter_mut().enumerate() {
                    *byte ^= mask[(frame.received as usize + index) % 4];
                }
            }
            frame.received += count as u64;
            let message = frame.control.as_mut().or(self.message.as_mut()).unwrap();
            if message.compressed && message.decoded {
                if let Err(error) =
                    inflate::feed(&mut self.inflater, &plain[..count], &mut |bytes| {
                        message.feed(bytes)
                    })
                {
                    self.fail(&error);
                    return;
                }
            } else {
                message.feed(&plain[..count]);
            }
            bytes = &bytes[count..];
            if frame.received == frame.length {
                self.end_frame();
            }
            if self.failed {
                return;
            }
        }
    }
    fn begin(&mut self, header: FrameHeader, length: u64) -> Result<(), String> {
        if length > i64::MAX as u64 || header.mask.is_some() != self.client {
            return Err(
                "invalid WebSocket length or masking; observation stopped, forwarding continues"
                    .into(),
            );
        }
        let kind = match header.opcode {
            OpCode::Data(Data::Text) => "text",
            OpCode::Data(Data::Binary) => "binary",
            OpCode::Data(Data::Continue) => "continuation",
            OpCode::Control(Control::Ping) => "ping",
            OpCode::Control(Control::Pong) => "pong",
            OpCode::Control(Control::Close) => "close",
            _ => return Err("unsupported WebSocket opcode".into()),
        };
        let control = matches!(header.opcode, OpCode::Control(_));
        if control
            && (!header.is_final || length > 125 || header.rsv1 || header.rsv2 || header.rsv3)
        {
            return Err("invalid WebSocket control frame".into());
        }
        let message = Message {
            kind,
            body: Prefix::new(self.recorder.store.limits.message_bytes as usize),
            started: self.started,
            frames: 1,
            compressed: header.rsv1,
            decoded: !self.unknown_extension
                && !header.rsv2
                && !header.rsv3
                && (!header.rsv1 || self.compression),
            semantic: (self.model.is_some() && kind == "text")
                .then(|| Prefix::new(crate::agent::MAX_CONTENT)),
        };
        let control = if control {
            Some(message)
        } else {
            if kind == "continuation" {
                let Some(message) = self.message.as_mut() else {
                    return Err("WebSocket continuation without a first fragment".into());
                };
                if header.rsv1 {
                    return Err("compressed flag on WebSocket continuation".into());
                }
                message.frames += 1;
            } else {
                if self.message.is_some() {
                    return Err("new WebSocket message before previous fragments finished".into());
                }
                self.message = Some(message);
            }
            None
        };
        self.frame = Some(Frame {
            header,
            length,
            received: 0,
            control,
        });
        Ok(())
    }
    fn end_frame(&mut self) {
        let Some(frame) = self.frame.take() else {
            return;
        };
        if let Some(message) = frame.control {
            self.emit(message, true);
        } else if frame.header.is_final
            && let Some(mut message) = self.message.take()
        {
            if message.compressed && message.decoded {
                if let Err(error) =
                    inflate::feed(&mut self.inflater, &[0, 0, 255, 255], &mut |bytes| {
                        message.feed(bytes)
                    })
                {
                    self.emit(message, false);
                    self.fail(&error);
                    return;
                }
                if self.reset_context {
                    self.inflater.reset(false);
                }
            }
            self.emit(message, true);
        }
    }
    fn emit(&mut self, message: Message, complete: bool) {
        if let (Some(model), Some(context), Some(semantic)) =
            (&mut self.model, &self.context, &message.semantic)
        {
            if complete && message.decoded && semantic.seen == semantic.bytes.len() as u64 {
                let mut events = if self.client {
                    model.websocket_request(&semantic.bytes)
                } else {
                    model.event(std::str::from_utf8(&semantic.bytes).unwrap_or(""))
                };
                for event in &mut events {
                    event.data["message_started_unix_ns"] = json!(message.started);
                }
                context.semantic(events);
            } else {
                context.semantic(vec![crate::agent::Event { kind:"agent.capture.error",data:json!({"task_id":context.agent.as_ref().map(|scope| &scope.task_id),"error":"incomplete or oversized model WebSocket message"}),content:None }]);
            }
        }
        let mut data = message.body.metadata();
        data["kind"] = message.kind.into();
        data["started_unix_ns"] = json!(message.started);
        data["completed_unix_ns"] = json!(crate::proxy::http::now());
        data["start_source"] = "proxy_frame_header".into();
        data["frames"] = message.frames.into();
        data["complete"] = complete.into();
        data["compressed"] = message.compressed.into();
        data["decoded"] = message.decoded.into();
        if message.kind == "close" && message.body.bytes.len() >= 2 {
            data["close_code"] =
                u16::from_be_bytes([message.body.bytes[0], message.body.bytes[1]]).into();
        }
        self.recorder.record(data, message.body.bytes);
    }
    fn fail(&mut self, error: &str) {
        self.recorder.error(error);
        self.failed = true;
        self.finish();
    }
    fn finish(&mut self) {
        if let Some(frame) = self.frame.take()
            && let Some(message) = frame.control
        {
            self.emit(message, false);
        }
        if let Some(message) = self.message.take() {
            self.emit(message, false);
        }
        if !self.header.is_empty() {
            self.recorder
                .error("connection ended in a partial WebSocket header");
            self.header.clear();
        }
    }
}

pub(in crate::proxy::http) fn observe(
    io: BoxIo,
    message: &MessageContext,
    extensions: &str,
    client: bool,
) -> BoxIo {
    let mut observer = Observer::new(
        Recorder::new(
            message,
            "websocket",
            if client { "upstream" } else { "downstream" },
        ),
        extensions,
        client,
    );
    observer.model = message.model_observer();
    observer.context = Some(message.clone());
    Box::new(Observed { io, observer })
}
struct Observed {
    io: BoxIo,
    observer: Observer,
}
impl AsyncRead for Observed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        self.observer.feed(&buf.filled()[before..]);
        result
    }
}
impl AsyncWrite for Observed {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
impl Drop for Observed {
    fn drop(&mut self) {
        self.observer.finish();
    }
}
