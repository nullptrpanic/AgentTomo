use super::*;
use crate::agent::{Hub, Observer, Scope, protocol::Protocol};

fn observer(hub: &Hub, task: &str) -> Observer {
    let protocol =
        Protocol::detect(&Method::POST, &"/api/v3/chat/completions".parse().unwrap()).unwrap();
    Observer::new(
        Scope {
            agent_instance_id: "instance".into(),
            task_id: task.into(),
        },
        hub.clone(),
        protocol,
    )
}

#[test]
fn parallel_function_calls_and_replayed_results_keep_their_identity() {
    let hub = Hub::default();
    let mut wire = observer(&hub, "turn1");
    let start = wire.event(&json!({"id":"reply","choices":[{"index":0,"delta":{"tool_calls":[
        {"index":0,"id":"a","type":"function","function":{"name":"weather","arguments":"{\"city\":"}},
        {"index":1,"id":"b","type":"function","function":{"name":"clock","arguments":"{"}}
    ]}}]}).to_string());
    let started: Vec<_> = start
        .iter()
        .filter(|e| e.kind == "agent.tool.started")
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(started[0].data["name"], "weather");
    assert_eq!(started[1].data["name"], "clock");
    wire.event(
        &json!({"id":"reply","choices":[{"index":0,"delta":{"tool_calls":[
        {"index":1,"function":{"arguments":"}"}},
        {"index":0,"function":{"arguments":"\"北京\"}"}}
    ]},"finish_reason":"tool_calls"}]})
        .to_string(),
    );
    let end = wire.event("[DONE]");
    let calls: Vec<_> = end.iter().filter(|e| e.kind == "agent.tool.call").collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].content, Some(json!(r#"{"city":"北京"}"#)));
    assert_eq!(calls[1].content, Some(json!("{}")));
    for (start, call) in started.iter().zip(&calls) {
        assert_eq!(
            start.data["agenttomo_toolcall_id"],
            call.data["agenttomo_toolcall_id"]
        );
        assert_eq!(call.data["response_id"], "reply");
    }
    let complete = end.last().unwrap();
    assert_eq!(complete.data["status"], "completed");
    assert_eq!(
        complete.content.as_ref().unwrap()["choices"][0]["message"]["tool_calls"][1]["id"],
        "b"
    );
    assert!(wire.event("[DONE]").is_empty());

    let messages = json!({"messages":[
        {"role":"tool","tool_call_id":"old","content":"from another turn"},
        {"role":"user","content":"time and weather"},
        {"role":"assistant","tool_calls":complete.content.as_ref().unwrap()["choices"][0]["message"]["tool_calls"]},
        {"role":"tool","tool_call_id":"b","content":[{"type":"text","text":"12:00"}]},
        {"role":"tool","tool_call_id":"a","content":"sunny"}
    ]}).to_string();
    let mut next = observer(&hub, "turn1");
    next.feed_body(messages.as_bytes());
    let request = next.finish_body("", true, None);
    assert_eq!(request.len(), 3);
    assert_eq!(request[0].kind, "agent.model.request");
    assert_eq!(request[1].kind, "agent.tool.result");
    assert_eq!(
        request[1].data["agenttomo_toolcall_id"],
        calls[1].data["agenttomo_toolcall_id"]
    );
    assert_eq!(
        request[1].content,
        Some(json!([{"type":"text","text":"12:00"}]))
    );
    assert_eq!(
        request[2].data["agenttomo_toolcall_id"],
        calls[0].data["agenttomo_toolcall_id"]
    );
    assert_eq!(request[2].content, Some(json!("sunny")));
    let mut replay = observer(&hub, "turn2");
    replay.feed_body(messages.as_bytes());
    assert_eq!(replay.finish_body("", true, None).len(), 1);
}

#[test]
fn incomplete_functions_keep_the_observation_without_claiming_a_complete_call() {
    for (id, finish) in [(Some("a"), "length"), (None, "tool_calls")] {
        let mut wire = observer(&Hub::default(), "turn");
        wire.event(
            &json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":id,"type":"function","function":{"name":"clock","arguments":"{"}}
        ]},"finish_reason":finish}]})
            .to_string(),
        );
        let end = wire.event("[DONE]");
        assert!(end.iter().all(|e| e.kind != "agent.tool.call"));
        let complete = end.last().unwrap();
        assert_eq!(complete.data["status"], "incomplete");
        assert_eq!(
            complete.content.as_ref().unwrap()["choices"][0]["message"]["tool_calls"][0]["function"]
                ["arguments"],
            "{"
        );
    }
}

#[test]
fn tool_indices_are_local_to_each_choice_but_call_ids_must_identify_one_call() {
    let mut stream = Stream::default();
    let observation = |second_id: &str| {
        json!({
        "id":"r",
        "choices": [
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"clock","arguments":"{}"}}]},"finish_reason":"tool_calls"},
            {"index":1,"delta":{"tool_calls":[{"index":0,"id":second_id,"function":{"name":"weather","arguments":"{}"}}]},"finish_reason":"tool_calls"}
        ]
    }).to_string()
    };
    stream.event(&observation("b")).unwrap();
    let events = stream.event("[DONE]").unwrap();
    let model = events.last().unwrap().content.as_ref().unwrap();
    assert_eq!(model["choices"][0]["message"]["tool_calls"][0]["id"], "a");
    assert_eq!(model["choices"][1]["message"]["tool_calls"][0]["id"], "b");

    let mut ambiguous = Stream::default();
    let error = ambiguous.event(&observation("a")).unwrap();
    assert_eq!(error[0].kind, "agent.capture.error");
    assert!(ambiguous.event("[DONE]").unwrap().is_empty());
}

#[test]
fn function_arguments_share_the_stream_byte_limit_with_text_and_names() {
    let mut stream = Stream {
        bytes: crate::agent::MAX_CONTENT - 4,
        ..Default::default()
    };
    let first = stream.event(r#"{"id":"r","choices":[{"index":0,"delta":{"content":"t","tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{"}}]}}]}"#).unwrap();
    assert_eq!(first.last().unwrap().kind, "agent.tool.started");
    let error = stream.event(r#"{"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}"#).unwrap();
    assert_eq!(error[0].kind, "agent.capture.error");
    assert!(stream.event("[DONE]").unwrap().is_empty());
}
