#![allow(clippy::unwrap_used, clippy::expect_used)]

use aho_llm::{LlmError, Request, Response, Sink, StreamEvent};
use aho_types::{CallId, ContentBlock, Message, Role, StopReason, Usage, Value};
use llm_openai_responses::{Flavor, StreamDecoder, classify, encode_request};
use serde_json::json;

fn decode(events: Vec<Value>) -> (Result<Response, LlmError>, Vec<StreamEvent>) {
    let mut observed = Vec::new();
    let result;
    {
        let mut sink: Sink<'_> = Box::new(|event| observed.push(event));
        let mut decoder = StreamDecoder::with_model("openai-codex", "gpt-5");
        result = events
            .into_iter()
            .try_for_each(|event| decoder.on_data(&event.to_string(), &mut sink))
            .and_then(|()| decoder.finish(&mut sink));
    }
    (result, observed)
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": name, "arguments": arguments})
}

fn item_event(kind: &str, index: u64, item: Value) -> Value {
    json!({"type": format!("response.output_item.{kind}"), "output_index": index, "item": item})
}

fn completed(output: Vec<Value>) -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": output}})
}

#[test]
fn interleaved_tools_finish_independently_and_keep_output_order() {
    let first = call("a", "read", r#"{"path":"a"}"#);
    let second = call("b", "read", r#"{"path":"b"}"#);
    let terminal = json!({"type": "response.completed", "response": {
        "status": "completed", "output": [first, second],
        "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 10}, "output_tokens": 8}
    }});
    let (result, events) = decode(vec![
        item_event("added", 0, call("a", "read", "")),
        json!({"type":"response.function_call_arguments.delta", "output_index":0, "delta":"{\"path\":"}),
        item_event("added", 1, call("b", "read", "")),
        item_event("done", 1, second),
        json!({"type":"response.function_call_arguments.delta", "output_index":0, "delta":"\"a\"}"}),
        item_event("done", 0, first),
        terminal,
    ]);
    let result = result.unwrap();
    assert_eq!(result.stop, StopReason::ToolUse);
    assert_eq!(result.usage, Usage { input_tokens: 30, output_tokens: 8, cache_read_tokens: 60, cache_write_tokens: 10 });
    assert_eq!(
        &result.content[..2],
        &[
            ContentBlock::ToolUse { id: CallId("a".into()), name: "read".into(), args: json!({"path":"a"}) },
            ContentBlock::ToolUse { id: CallId("b".into()), name: "read".into(), args: json!({"path":"b"}) },
        ]
    );
    let ends: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolUseEnd { id, .. } => Some(id.0.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ends, ["b", "a"]);
    assert!(matches!(&events[1], StreamEvent::ToolUseStart { id, .. } if id.0 == "b"));
}

#[test]
fn terminal_snapshots_backfill_reasoning_and_replay_only_to_the_same_route_and_model() {
    let early = json!({"type":"reasoning", "id":"rs_1", "summary":[{"type":"summary_text","text":"Consider."}]});
    let complete = json!({"type":"reasoning", "id":"rs_1", "summary":[{"type":"summary_text","text":"Consider."}], "encrypted_content":"opaque-signature"});
    let tool = call("a", "read", "{}");
    let (result, events) = decode(vec![
        item_event("added", 0, json!({"type":"reasoning", "id":"rs_1"})),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"Consider."}),
        item_event("done", 0, early),
        completed(vec![complete, tool]),
    ]);
    let content = result.unwrap().content;
    assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::Thinking(_))).count(), 1);
    let mut req = Request { model: "gpt-5".into(), messages: vec![Message::assistant(content)], ..Default::default() };
    let same = encode_request(&req, Flavor::Codex, "openai-codex");
    assert_eq!(same["input"][0]["encrypted_content"], "opaque-signature");
    assert_eq!(same["input"][1]["id"], "fc_a");
    assert_eq!(same["input"][1]["call_id"], "a");
    let other = encode_request(&req, Flavor::Generic, "meta");
    assert_eq!(other["input"], json!([{"type":"function_call", "call_id":"a", "name":"read", "arguments":"{}"}]));
    req.model = "gpt-5.2".into();
    assert_eq!(encode_request(&req, Flavor::Codex, "openai-codex")["input"], other["input"]);
}

#[test]
fn final_only_items_emit_live_content_once_and_codex_done_commits() {
    let text = json!({"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"héllo"}]});
    let (result, events) = decode(vec![
        item_event("done", 0, text.clone()),
        json!({"type":"response.done","response":{"output":[text],"status":"completed"}}),
    ]);
    assert_eq!(result.unwrap().content[0], ContentBlock::text("héllo"));
    assert_eq!(events, [StreamEvent::Text("héllo".into())]);
}

#[test]
fn a_done_argument_snapshot_replaces_partial_bytes_and_preserves_invalid_json_for_schema_errors() {
    let (result, events) = decode(vec![
        item_event("added", 0, call("a", "read", "")),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"x\":"}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"not JSON"}),
        item_event("done", 0, call("a", "read", "not JSON")),
        completed(vec![call("a", "read", "not JSON")]),
    ]);
    let result = result.unwrap();
    assert!(matches!(&result.content[0], ContentBlock::ToolUse { args: Value::String(raw), .. } if raw == "not JSON"));
    assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolUseEnd { .. })).count(), 1);
}

#[test]
fn premature_done_markers_and_malformed_transitions_cannot_commit_partial_output() {
    let mut sink: Sink<'_> = Box::new(|_| {});
    let mut decoder = StreamDecoder::new("meta");
    decoder.on_data(r#"{"type":"response.output_text.delta","output_index":0,"delta":"partial"}"#, &mut sink).unwrap();
    decoder.on_data("[DONE]", &mut sink).unwrap();
    assert!(matches!(decoder.finish(&mut sink), Err(LlmError::Transient(_))));
    for malformed in [
        "not JSON",
        r#"{"type":"response.output_text.delta","delta":"unindexed"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"read","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"status":"in_progress"}}"#,
    ] {
        assert!(matches!(StreamDecoder::new("meta").on_data(malformed, &mut sink), Err(LlmError::Transient(_))));
    }
    let (result, _) = decode(vec![item_event("added", 0, call("a", "read", "")), item_event("done", 0, call("b", "read", "{}"))]);
    assert!(matches!(result, Err(LlmError::Transient(_))));
}

#[test]
fn incomplete_responses_distinguish_length_refusal_and_unknown_failures() {
    for (reason, expected) in
        [("max_output_tokens", Some(StopReason::MaxTokens)), ("content_filter", Some(StopReason::Refusal)), ("unexpected", None)]
    {
        let (result, _) = decode(vec![json!({"type":"response.incomplete","response":{
            "status":"incomplete", "incomplete_details":{"reason":reason}, "output":[]
        }})]);
        match expected {
            Some(stop) => assert_eq!(result.unwrap().stop, stop),
            None => assert!(matches!(result, Err(LlmError::Transient(_)))),
        }
    }
}

#[test]
fn late_tool_outputs_keep_call_identity_and_images_are_not_discarded() {
    let result = |body, error| ContentBlock::ToolResult { id: CallId("call|fc_native".into()), content: body, is_error: error };
    let req = Request {
        model: "gpt-5".into(),
        max_tokens: 8,
        system: vec![("identity".into(), "You are aho.".into(), false)],
        messages: vec![
            Message::assistant(vec![ContentBlock::ToolUse { id: CallId("call|fc_native".into()), name: "read".into(), args: json!({}) }]),
            Message { role: Role::Tool, content: vec![result(vec![ContentBlock::text("running")], false)] },
            Message::user("continue"),
            Message {
                role: Role::Tool,
                content: vec![result(vec![ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2, 3] }], true)],
            },
        ],
        ..Default::default()
    };
    let codex = encode_request(&req, Flavor::Codex, "openai-codex");
    assert!(codex.get("max_output_tokens").is_none());
    assert_eq!(codex["instructions"], "You are aho.");
    assert_eq!(codex["store"], false);
    assert_eq!(codex["input"][0]["call_id"], "call");
    assert_eq!(codex["input"][1]["call_id"], "call");
    assert_eq!(codex["input"][3]["call_id"], "call");
    assert_eq!(
        codex["input"][3]["output"],
        json!([
            {"type":"input_text", "text":"[tool error]\n"},
            {"type":"input_image", "detail":"auto", "image_url":"data:image/png;base64,AQID"}
        ])
    );
    let generic = encode_request(&req, Flavor::Generic, "meta");
    assert_eq!(generic["max_output_tokens"], 16);
    assert_eq!(generic["input"][0]["role"], "system");
}

#[test]
fn provider_failures_are_classified_without_echoing_server_secrets() {
    let secret = "secret-access-token";
    let body = json!({"type":"response.failed","response":{"error":{"code":"server_error","message":secret}}}).to_string();
    let (result, _) = decode(vec![serde_json::from_str(&body).unwrap()]);
    assert!(matches!(&result, Err(LlmError::Transient(_))));
    assert!(!result.unwrap_err().to_string().contains(secret));
    assert_eq!(classify(Some(429), Some(2500), "{}", false, "meta"), LlmError::RateLimited { retry_after_ms: 2500 });
    assert_eq!(
        classify(Some(400), None, r#"{"error":{"code":"context_length_exceeded"}}"#, false, "xai"),
        LlmError::ContextWindowExceeded { requested: 0, available: 0 }
    );
    assert!(matches!(classify(Some(401), None, secret, false, "xai"), LlmError::Auth(message) if !message.contains(secret)));
    assert_eq!(classify(Some(413), None, "{}", true, "meta"), LlmError::ImageOffloadRequired);
}
