//! Model-visible text, function calls and returns in streamed Chat Completions.
use super::protocol::{Event, MessageKind, ModelDecoder, ModelProtocol};
use axum::http::{Method, StatusCode, Uri};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    io,
};

pub struct ChatCompletions;
impl ModelProtocol for ChatCompletions {
    fn matches(&self, method: &Method, uri: &Uri) -> bool {
        method == Method::POST
            && uri
                .path()
                .trim_end_matches('/')
                .ends_with("/chat/completions")
    }
    fn decoder(&self) -> Box<dyn ModelDecoder> {
        Box::<Stream>::default()
    }
    fn expects_sse(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Choice {
    text: String,
    finish: Option<String>,
    tools: BTreeMap<u64, FunctionCall>,
}

#[derive(Default)]
struct FunctionCall {
    id: String,
    name: String,
    arguments: String,
    unsupported: bool,
}

fn append_fragment(target: &mut String, fragment: &str, bytes: &mut usize) -> io::Result<()> {
    if fragment.len() > super::MAX_CONTENT.saturating_sub(*bytes) {
        return Err(io::Error::other("model reply exceeded semantic limit"));
    }
    *bytes += fragment.len();
    target.push_str(fragment);
    Ok(())
}

impl FunctionCall {
    fn identified(&self) -> bool {
        !self.id.is_empty() && !self.name.is_empty()
    }

    fn update(&mut self, delta: &Value, bytes: &mut usize) -> io::Result<()> {
        self.unsupported |= delta["type"]
            .as_str()
            .is_some_and(|kind| kind != "function");
        if self.unsupported {
            return Ok(());
        }
        if let Some(id) = delta["id"].as_str().filter(|id| !id.is_empty()) {
            if self.id.is_empty() {
                append_fragment(&mut self.id, id, bytes)?;
            } else if self.id != id {
                return Err(io::Error::other(
                    "Chat Completions tool call ID changed at the same index",
                ));
            }
        }
        let function = &delta["function"];
        if let Some(name) = function["name"].as_str() {
            append_fragment(&mut self.name, name, bytes)?;
        }
        if let Some(args) = function["arguments"].as_str() {
            append_fragment(&mut self.arguments, args, bytes)?;
        }
        Ok(())
    }

    fn event(&self, kind: &'static str, response_id: &str, content: Option<Value>) -> Event {
        Event {
            kind,
            data: json!({"response_id":response_id, "call_id":self.id, "name":self.name,
                "tool_type":"function_call", "execution":"agent", "visibility":"model_output"}),
            content,
        }
    }
}

#[derive(Default)]
struct Stream {
    id: Option<String>,
    choices: BTreeMap<u64, Choice>,
    tool_ids: HashSet<String>,
    bytes: usize,
    usage: Value,
    ended: bool,
}

impl Stream {
    fn end(&mut self) -> Vec<Event> {
        self.ended = true;
        let mut events = Vec::new();
        let mut choices = Vec::with_capacity(self.choices.len());
        let mut status = "completed";
        for (index, choice) in std::mem::take(&mut self.choices) {
            let kind = match choice.finish.as_deref() {
                Some("stop") if choice.tools.is_empty() => MessageKind::Final,
                Some("tool_calls" | "stop") if !choice.tools.is_empty() => {
                    MessageKind::Intermediate
                }
                _ => {
                    status = "incomplete";
                    MessageKind::Unknown
                }
            };
            let mut message = json!({"role":"assistant", "content":choice.text});
            if !choice.tools.is_empty() {
                let mut tools = Vec::new();
                for call in choice.tools.into_values() {
                    if call.unsupported {
                        status = "incomplete";
                        continue;
                    }
                    if call.identified()
                        && matches!(choice.finish.as_deref(), Some("tool_calls" | "stop"))
                    {
                        events.push(call.event(
                            "agent.tool.call",
                            self.id.as_deref().unwrap_or_default(),
                            Some(call.arguments.clone().into()),
                        ));
                    } else {
                        status = "incomplete";
                    }
                    let mut captured = json!({"type":"function","function":{"name":call.name,"arguments":call.arguments}});
                    if !call.id.is_empty() {
                        captured["id"] = call.id.into();
                    }
                    tools.push(captured);
                }
                message["tool_calls"] = tools.into();
                if choice.text.is_empty() {
                    message["content"] = Value::Null;
                }
            }
            choices.push(json!({"index":index, "finish_reason":choice.finish, "message":message}));
            if !choice.text.is_empty() {
                events.push(Event {
                    kind: "agent.message",
                    data: json!({"response_id":self.id,"item_id":self.item_id(index),"role":"assistant","message_kind":kind,"finish_reason":choice.finish}),
                    content: Some(choice.text.into()),
                });
            }
        }
        if events.is_empty() {
            status = "incomplete";
        }
        events.push(Event {
            kind: "agent.model.complete",
            data: json!({"response_id":self.id,"status":status,"usage":self.usage}),
            content: Some(json!({"source":"assembled_from_chat_completions_stream","id":self.id,"choices":choices,"usage":self.usage})),
        });
        events
    }
    fn item_id(&self, index: u64) -> String {
        format!("{}-{index}", self.id.as_deref().unwrap_or_default())
    }
    fn failure(&mut self, error: String, status: Option<StatusCode>) -> Vec<Event> {
        self.ended = true;
        vec![
            Event {
                kind: "agent.capture.error",
                data: json!({"error":error}),
                content: None,
            },
            Event {
                kind: "agent.model.complete",
                data: json!({"response_id":self.id,"status":"failed","status_code":status.map(|s|s.as_u16())}),
                content: Some(json!({"status":"failed","error":error})),
            },
        ]
    }
}

impl ModelDecoder for Stream {
    fn request(&mut self, bytes: &[u8]) -> io::Result<Vec<Event>> {
        let request: Value = serde_json::from_slice(bytes)?;
        let mut events = Vec::new();
        if let Some(messages) = request["messages"].as_array() {
            // Full chat histories can include tool returns from earlier user turns.
            let turn = messages
                .rsplit(|m| m["role"] == "user")
                .next()
                .unwrap_or_default();
            for message in turn {
                if message["role"] == "tool"
                    && let Some(id) = message["tool_call_id"].as_str().filter(|id| !id.is_empty())
                    && let Some(content) = message.get("content")
                {
                    events.push(Event {
                        kind: "agent.tool.result",
                        data: json!({"call_id":id, "visibility":"model_input"}),
                        content: Some(content.clone()),
                    });
                }
            }
        }
        events.insert(
            0,
            Event {
                kind: "agent.model.request",
                data: json!({"model":request["model"],"stream":request["stream"]}),
                content: Some(request),
            },
        );
        Ok(events)
    }
    fn http_response(&mut self, status: StatusCode, bytes: &[u8]) -> io::Result<Vec<Event>> {
        if !status.is_success() {
            return Ok(self.failure(format!("HTTP {status}"), Some(status)));
        }
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(error) => return Ok(self.failure(error.to_string(), Some(status))),
        };
        let mut events = match self.update(&value, false) {
            Ok(events) => events,
            Err(error) => return Ok(self.failure(error.to_string(), Some(status))),
        };
        events.extend(self.end());
        if let Some(complete) = events.last_mut() {
            complete.content = Some(value);
        }
        Ok(events)
    }
    fn websocket_request(&mut self, _: &[u8]) -> io::Result<Vec<Event>> {
        Ok(Vec::new())
    }
    fn event(&mut self, data: &str) -> io::Result<Vec<Event>> {
        if self.ended {
            return Ok(Vec::new());
        }
        if data.trim() == "[DONE]" {
            return Ok(self.end());
        }
        Ok(
            match serde_json::from_str(data)
                .map_err(io::Error::from)
                .and_then(|value| self.update(&value, true))
            {
                Ok(events) => events,
                Err(error) => self.failure(error.to_string(), None),
            },
        )
    }
}

impl Stream {
    fn update(&mut self, chunk: &Value, stream: bool) -> io::Result<Vec<Event>> {
        if !chunk["error"].is_null() {
            return Err(io::Error::other("model stream returned an error"));
        }
        let mut events = Vec::new();
        let chunk_id = chunk["id"].as_str().filter(|id| !id.is_empty());
        match (self.id.as_deref(), chunk_id) {
            (None, Some(id)) => {
                self.id = Some(id.to_owned());
                events.push(Event {
                    kind: "agent.model.started",
                    data: json!({"response_id":id,"model":chunk["model"]}),
                    content: None,
                });
            }
            (Some(existing), Some(id)) if existing != id => {
                return Err(io::Error::other(
                    "Chat Completions ID changed during the stream",
                ));
            }
            (Some(_), Some(_)) => {}
            (Some(_), None)
                if stream && (chunk["id"].is_null() || chunk["id"].as_str() == Some("")) => {}
            _ => return Err(io::Error::other("missing Chat Completions ID")),
        }
        let id = self.id.as_deref().expect("validated response ID");
        if !chunk["usage"].is_null() {
            self.usage = chunk["usage"].clone();
        }
        if stream && chunk["choices"].is_null() && chunk["usage"].is_object() {
            return Ok(events);
        }
        for choice in chunk["choices"]
            .as_array()
            .ok_or_else(|| io::Error::other("missing Chat Completions choices"))?
        {
            let index = choice["index"]
                .as_u64()
                .ok_or_else(|| io::Error::other("missing choice index"))?;
            if index >= 128 {
                return Err(io::Error::other("too many Chat Completions choices"));
            }
            let item_id = self.item_id(index);
            let stored = self.choices.entry(index).or_default();
            if let Some(reason) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                stored.finish = Some(reason.to_owned());
            }
            let message = &choice[if stream { "delta" } else { "message" }];
            for delta in [&message["content"], &message["refusal"]]
                .into_iter()
                .filter_map(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                append_fragment(&mut stored.text, delta, &mut self.bytes)?;
                events.push(Event {kind:"agent.message.delta",data:json!({"response_id":id,"item_id":item_id,"role":"assistant","delta":delta}),content:None});
            }
            if let Some(deltas) = message["tool_calls"].as_array() {
                for (position, delta) in deltas.iter().enumerate() {
                    let index = delta["index"]
                        .as_u64()
                        .or_else(|| (!stream).then_some(position as u64))
                        .filter(|index| *index < 128)
                        .ok_or_else(|| io::Error::other("invalid Chat Completions tool index"))?;
                    let call = stored.tools.entry(index).or_default();
                    let identified = call.identified();
                    call.update(delta, &mut self.bytes)?;
                    if !identified && call.identified() && !call.unsupported {
                        if !self.tool_ids.insert(call.id.clone()) {
                            return Err(io::Error::other(
                                "duplicate Chat Completions tool call ID",
                            ));
                        }
                        events.push(call.event("agent.tool.started", id, None));
                    }
                }
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests;
