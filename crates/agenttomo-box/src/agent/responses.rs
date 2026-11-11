//! OpenAI Responses wire protocol, including function and custom tool calls.
use serde_json::{Value, json};
use std::collections::HashSet;

pub struct Event {
    pub kind: &'static str,
    pub data: Value,
    pub content: Option<Value>,
}

pub fn request(value: &Value) -> Vec<Event> {
    let mut events = vec![Event {
        kind: "agent.model.request",
        data: json!({"model":value["model"],"parallel_tool_calls":value["parallel_tool_calls"],"generate":value["generate"]}),
        content: Some(value.clone()),
    }];
    if let Some(input) = value["input"].as_array() {
        for item in input {
            if matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            ) {
                events.push(Event {
                    kind: "agent.tool.result",
                    data: json!({"call_id":item["call_id"],"visibility":"model_input"}),
                    content: Some(item["output"].clone()),
                });
            }
        }
    }
    events
}

#[derive(Default)]
pub struct Response {
    done: HashSet<String>,
    id: Option<String>,
}

impl Response {
    pub fn event(&mut self, value: Value) -> Vec<Event> {
        let mut events = Vec::new();
        match value["type"].as_str().unwrap_or("") {
            "response.created" => {
                self.done.clear();
                self.id = value["response"]["id"].as_str().map(str::to_owned);
                events.push(Event {
                kind: "agent.model.started",
                data: json!({"response_id":value["response"]["id"],"model":value["response"]["model"]}),
                content: None,
                });
            }
            "response.output_item.added" => {
                let item = &value["item"];
                if is_tool(item) {
                    events.push(tool(item, "agent.tool.started", None));
                }
            }
            "response.output_text.delta" => events.push(Event {
                kind: "agent.message.delta",
                data: json!({"item_id":value["item_id"],"delta":value["delta"]}),
                content: None,
            }),
            "response.output_item.done" => self.complete_item(&value["item"], &mut events),
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(items) = value["response"]["output"].as_array() {
                    for item in items {
                        self.complete_item(item, &mut events);
                    }
                }
                events.push(Event {
                    kind: "agent.model.complete",
                    data: json!({"response_id":value["response"]["id"],"status":value["response"]["status"],"usage":value["response"]["usage"],"error":value["response"]["error"]}),
                    content: Some(value["response"].clone()),
                });
            }
            "error" => events.push(Event {
                kind: "agent.capture.error",
                data: json!({"error":value}),
                content: None,
            }),
            _ => {}
        }
        if let Some(id) = &self.id {
            for event in &mut events {
                if event.data.get("response_id").is_none() {
                    event.data["response_id"] = id.clone().into();
                }
            }
        }
        events
    }

    fn complete_item(&mut self, item: &Value, events: &mut Vec<Event>) {
        let Some(id) = item["id"].as_str().or_else(|| item["call_id"].as_str()) else {
            return;
        };
        if !self.done.insert(id.to_owned()) {
            return;
        }
        if is_tool(item) {
            let content = match item["type"].as_str() {
                Some("custom_tool_call") => item["input"].clone(),
                Some("function_call") => item["arguments"].clone(),
                _ => item.clone(),
            };
            events.push(tool(item, "agent.tool.call", Some(content)));
            if item["type"] != "function_call"
                && item["type"] != "custom_tool_call"
                && let Some(output) = item
                    .get("output")
                    .or_else(|| item.get("outputs"))
                    .or_else(|| item.get("results"))
            {
                events.push(tool(item, "agent.tool.result", Some(output.clone())));
            }
        } else if item["type"] == "message" {
            let text = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "output_text")
                .filter_map(|part| part["text"].as_str())
                .collect::<String>();
            events.push(Event {
                kind: "agent.message",
                data: json!({"item_id":id,"role":item["role"],"phase":item["phase"]}),
                content: Some(Value::String(text)),
            });
        }
    }
}

fn is_tool(item: &Value) -> bool {
    item["type"]
        .as_str()
        .is_some_and(|kind| kind.ends_with("_call"))
}
fn tool(item: &Value, kind: &'static str, content: Option<Value>) -> Event {
    Event {
        kind,
        data: json!({"call_id":item.get("call_id").unwrap_or(&item["id"]),"item_id":item["id"],"name":item.get("name").unwrap_or(&item["type"]),"tool_type":item["type"],"execution":if matches!(item["type"].as_str(),Some("function_call"|"custom_tool_call")) {"agent"} else {"provider_or_native"},"visibility":"model_output"}),
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn terminal_events_keep_the_complete_model_response() {
        for (kind, status) in [
            ("response.completed", "completed"),
            ("response.incomplete", "incomplete"),
            ("response.failed", "failed"),
        ] {
            let value = json!({"id":"resp1","model":"test-model","status":status,
                "output":[{"id":"msg1","type":"message","content":[{"type":"output_text","text":"Full answer"}]}],
                "usage":{"input_tokens":12,"output_tokens":3},"error":null});
            let events = Response::default().event(json!({"type":kind,"response":value}));
            let complete = events.last().unwrap();
            assert_eq!(complete.kind, "agent.model.complete");
            assert_eq!(complete.content.as_ref(), Some(&value));
        }
    }

    #[test]
    fn provider_tools_keep_their_visible_record_and_returned_fields() {
        let mut response = Response::default();
        let item = json!({"type":"file_search_call","id":"fs1","queries":["weather"],"results":[{"text":"sunny"}]});
        let events = response.event(json!({"type":"response.output_item.done","item":item}));
        assert_eq!(events[0].data["call_id"], "fs1");
        assert_eq!(events[0].data["execution"], "provider_or_native");
        assert_eq!(events[0].content, Some(item));
        assert_eq!(events[1].content, Some(json!([{"text":"sunny"}])));
    }

    #[test]
    fn real_code_mode_sse_keeps_complete_arguments_once() {
        let fixture = include_str!("../../tests/fixtures/codex-code-mode-shell.sse");
        let mut parser = sse_parse::SseParser::new();
        let mut response = Response::default();
        let mut calls = Vec::new();
        for chunk in fixture.as_bytes().chunks(7) {
            parser.feed(chunk);
            while let Some(event) = parser.next_event() {
                calls.extend(response.event(serde_json::from_str(&event.data).unwrap()));
            }
        }
        let tools = calls
            .iter()
            .filter(|event| event.kind == "agent.tool.call")
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].data["name"], "exec");
        assert!(
            tools[0]
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("tools.exec_command")
        );
    }

    #[test]
    fn function_arguments_and_results_remain_lossless() {
        let mut response = Response::default();
        response.event(json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc1","call_id":"call1","name":"shell","arguments":""}}));
        response.event(json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"{\"cmd\":"}));
        response.event(json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"\"echo hello\"}"}));
        let events = response.event(json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc1","call_id":"call1","name":"shell","arguments":"{\"cmd\":\"echo hello\"}"}}));
        assert_eq!(events[0].content, Some(json!("{\"cmd\":\"echo hello\"}")));
        let output = json!([{"type":"input_text","text":"a".repeat(100_000)},{"type":"input_text","text":"complete result"}]);
        let events = request(
            &json!({"model":"example","input":[{"type":"function_call_output","call_id":"call1","output":output}]}),
        );
        assert_eq!(events[1].kind, "agent.tool.result");
        assert_eq!(events[1].content, Some(output));
    }

    #[test]
    fn completion_recovers_tools_and_text_without_duplicate_dispatch() {
        let tool = json!({"type":"custom_tool_call","id":"ctc","call_id":"call1","name":"exec","input":"text(42)"});
        let mut response = Response::default();
        assert_eq!(
            response
                .event(json!({"type":"response.output_item.done","item":tool}))
                .len(),
            1
        );
        let events = response.event(json!({"type":"response.completed","response":{"id":"resp1","output":[tool,{"type":"message","id":"msg1","content":[{"type":"output_text","text":"done"}]}]}}));
        assert!(!events.iter().any(|event| event.kind == "agent.tool.call"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "agent.message" && event.content == Some(json!("done")))
        );
        assert_eq!(events.last().unwrap().kind, "agent.model.complete");
    }
}
