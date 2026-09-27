#![allow(clippy::unwrap_used, clippy::expect_used)]

use aho_llm::{LlmError, Request, Sink, StreamEvent};
use aho_types::{CallId, Consequence, ContentBlock, Message, Role, RootSchema, StopReason, ToolSpec, Usage, Value};
use llm_pi_messages::{StreamDecoder, classify, encode_request};
use serde_json::json;

fn usage() -> Value {
    json!({
        "input": 11, "output": 8, "cacheRead": 23, "cacheWrite": 5, "cacheWrite1h": 2,
        "reasoning": 3, "totalTokens": 47,
        "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0.03, "cacheWrite": 0.04, "total": 0.37}
    })
}

fn feed(decoder: &mut StreamDecoder, event: Value, sink: &mut Sink<'_>) {
    decoder.on_event(&event.to_string(), sink).expect("valid native event");
}

#[test]
fn live_tools_complete_once_and_native_metadata_survives_history_replay() {
    let mut decoder = StreamDecoder::new("auto");
    let mut events = Vec::new();
    let mut sink: Sink<'_> = Box::new(|event| events.push(event));
    for event in [
        json!({"type": "start"}),
        json!({"type": "thinking_start", "contentIndex": 0}),
        json!({"type": "thinking_delta", "contentIndex": 0, "delta": "Plan"}),
        json!({"type": "thinking_end", "contentIndex": 0, "content": "Plan", "contentSignature": "opaque-thinking", "redacted": true}),
        json!({"type": "text_start", "contentIndex": 1}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "Read "}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "café"}),
        json!({"type": "text_end", "contentIndex": 1, "content": "Read café", "contentSignature": "opaque-text"}),
        json!({"type": "toolcall_start", "contentIndex": 2, "id": "call-1", "toolName": "read"}),
        json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "{\"path\":"}),
        json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "\"partial\"}"}),
        json!({"type": "toolcall_end", "contentIndex": 2, "toolCall": {
            "type": "toolCall", "id": "call-1", "name": "read", "arguments": {"path": "café.txt"},
            "thoughtSignature": "opaque-tool", "namespace": "fs"
        }}),
    ] {
        feed(&mut decoder, event, &mut sink);
    }
    assert!(!decoder.is_done());
    let rewrite = json!({"policyId": "p", "policyVersion": 2, "changed": true, "tokenCountChange": -3,
        "messageCountChange": 0, "systemPromptChanged": true});
    feed(
        &mut decoder,
        json!({"type": "done", "reason": "toolUse", "usage": usage(),
        "responseId": "resp-1", "providerThinkingLevel": "high", "rewrite": rewrite}),
        &mut sink,
    );
    let response = decoder.finish().expect("complete response");
    drop(sink);
    assert_eq!(response.stop, StopReason::ToolUse);
    assert_eq!(response.usage, Usage { input_tokens: 11, output_tokens: 8, cache_read_tokens: 23, cache_write_tokens: 5 });
    assert_eq!(
        events,
        vec![
            StreamEvent::Thinking("Plan".into()),
            StreamEvent::Text("Read ".into()),
            StreamEvent::Text("café".into()),
            StreamEvent::ToolUseStart { id: CallId("call-1".into()), name: "read".into() },
            StreamEvent::ToolUseEnd { id: CallId("call-1".into()), name: "read".into(), args: json!({"path": "café.txt"}) },
            StreamEvent::Usage(response.usage),
        ]
    );
    // Exercise durable serialization beyond in-memory forwarding of the metadata value.
    let stored = serde_json::to_string(&Message::assistant(response.content)).unwrap();
    let restored: Message = serde_json::from_str(&stored).unwrap();
    assert_eq!(restored.text(), "PlanRead café");
    let replay = encode_request(&Request { model: "new-routing-model".into(), messages: vec![restored], ..Default::default() });
    let previous = &replay["context"]["messages"][0];
    assert_eq!(previous["model"], "auto", "retain the source model when the next turn changes models");
    assert_eq!(previous["responseId"], "resp-1");
    assert_eq!(previous["providerThinkingLevel"], "high");
    assert_eq!(previous["usage"], usage(), "cache duration, reasoning and costs remain in native history");
    assert_eq!(previous["diagnostics"][0]["details"], rewrite);
    assert_eq!(
        previous["content"],
        json!([
            {"type": "thinking", "thinking": "Plan", "thinkingSignature": "opaque-thinking", "redacted": true},
            {"type": "text", "text": "Read café", "textSignature": "opaque-text"},
            {"type": "toolCall", "id": "call-1", "name": "read", "arguments": {"path": "café.txt"},
                "thoughtSignature": "opaque-tool", "namespace": "fs"}
        ])
    );
}

#[test]
fn normalized_system_tools_and_mixed_result_history_use_native_message_roles() {
    let call = CallId("c".into());
    let result = |text: &str| ContentBlock::ToolResult {
        id: call.clone(),
        content: vec![ContentBlock::text(text), ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2, 3] }],
        is_error: true,
    };
    let request = Request {
        model: "auto".into(),
        system: vec![("identity".into(), "First".into(), true), ("local".into(), "Second".into(), false)],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read file".into(),
            input_schema: RootSchema(json!({"type": "object", "properties": {"path": {"type": "string"}}})),
            class: Consequence::ReadOnly,
        }],
        messages: vec![
            Message::assistant(vec![ContentBlock::ToolUse { id: call.clone(), name: "read".into(), args: json!({"path": "x"}) }]),
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::text("before"), result(aho_log::TOOL_CALL_RUNNING), ContentBlock::text("after")],
            },
            Message::assistant(vec![ContentBlock::text("Continuing")]),
            Message { role: Role::Tool, content: vec![result("late result")] },
        ],
        max_tokens: 321,
        reasoning: None,
        cache_key: Some("session-stable".into()),
    };
    let body = encode_request(&request);
    assert_eq!(body["options"], json!({"maxTokens": 321, "sessionId": "session-stable"}));
    let messages = body["context"]["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": [
        {"type": "text", "text": "First"}, {"type": "text", "text": "Second"}
    ], "toolsAdded": [{"name": "read", "description": "Read file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}], "timestamp": 0})
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["text"], "before");
    assert_eq!(
        messages[3],
        json!({"role": "toolResult", "toolCallId": "c", "toolName": "read", "isError": true, "timestamp": 0,
        "content": [{"type": "text", "text": aho_log::TOOL_CALL_RUNNING}, {"type": "image", "mimeType": "image/png", "data": "AQID"}]})
    );
    assert_eq!(messages[4]["content"][0]["text"], "after");
    assert_eq!(messages[6]["role"], "user", "late result must remain valid when Radius routes to Anthropic");
    assert_eq!(messages[6]["content"][0]["text"], "[result for call c (read), failed]\nlate result");
    assert_eq!(messages[6]["content"][1], json!({"type": "image", "mimeType": "image/png", "data": "AQID"}));
}

#[test]
fn unknown_protocol_metadata_is_never_model_visible() {
    let request = Request {
        messages: vec![Message::assistant(vec![
            ContentBlock::text("answer"),
            ContentBlock::ProviderMetadata { protocol: "openai-responses".into(), data: json!({"private": "opaque"}) },
        ])],
        ..Default::default()
    };
    assert_eq!(encode_request(&request)["context"]["messages"][0]["content"], json!([{"type": "text", "text": "answer"}]));
}

#[test]
fn incomplete_and_identity_changing_tool_calls_never_complete() {
    let mut events = Vec::new();
    let mut sink: Sink<'_> = Box::new(|event| events.push(event));
    let mut decoder = StreamDecoder::new("auto");
    feed(&mut decoder, json!({"type": "toolcall_start", "contentIndex": 0, "id": "one", "toolName": "read"}), &mut sink);
    let changed = json!({"type": "toolcall_end", "contentIndex": 0,
        "toolCall": {"id": "different", "name": "read", "arguments": {}}});
    assert!(matches!(decoder.on_event(&changed.to_string(), &mut sink), Err(LlmError::Transient(_))));
    decoder.on_event("[DONE]", &mut sink).unwrap();
    assert!(!decoder.is_done(), "the generic SSE sentinel cannot replace pi's terminal usage and reason");
    assert!(matches!(decoder.finish(), Err(LlmError::Transient(_))));
    drop(sink);
    assert_eq!(events, vec![StreamEvent::ToolUseStart { id: CallId("one".into()), name: "read".into() }]);

    let mut decoder = StreamDecoder::new("auto");
    let mut sink: Sink<'_> = Box::new(|_| {});
    feed(&mut decoder, json!({"type": "text_start", "contentIndex": 0}), &mut sink);
    feed(&mut decoder, json!({"type": "done", "reason": "stop", "usage": usage()}), &mut sink);
    assert!(matches!(decoder.finish(), Err(LlmError::Transient(_))), "terminal events cannot hide incomplete content blocks");
}

#[test]
fn terminal_reason_controls_stop_and_error_usage_is_observable() {
    let mut sink: Sink<'_> = Box::new(|_| {});
    for (reason, expected) in [("stop", StopReason::EndTurn), ("length", StopReason::MaxTokens), ("toolUse", StopReason::ToolUse)] {
        let mut decoder = StreamDecoder::new("auto");
        feed(&mut decoder, json!({"type": "done", "reason": reason, "usage": usage()}), &mut sink);
        assert_eq!(decoder.finish().unwrap().stop, expected);
    }
    let mut seen = Vec::new();
    let mut sink: Sink<'_> = Box::new(|event| seen.push(event));
    let mut decoder = StreamDecoder::new("auto");
    let error = json!({"type": "error", "reason": "error", "usage": usage(), "errorMessage": "upstream overloaded; bearer secret"});
    let error = decoder.on_event(&error.to_string(), &mut sink).unwrap_err();
    assert!(matches!(&error, LlmError::Transient(_)));
    assert!(!error.to_string().contains("secret"));
    drop(sink);
    assert_eq!(seen, vec![StreamEvent::Usage(Usage { input_tokens: 11, output_tokens: 8, cache_read_tokens: 23, cache_write_tokens: 5 })]);
}

#[test]
fn http_envelopes_map_actionable_failures_without_exposing_raw_diagnostics() {
    assert_eq!(classify(Some(429), Some(4200), "{}", false), LlmError::RateLimited { retry_after_ms: 4200 });
    assert_eq!(
        classify(Some(400), None, r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#, false),
        LlmError::ContextWindowExceeded { requested: 0, available: 0 }
    );
    assert_eq!(classify(Some(413), None, "{}", true), LlmError::ImageOffloadRequired);
    let error = classify(Some(401), None, r#"{"error":{"message":"authorization: Bearer credential"}}"#, false);
    assert!(matches!(&error, LlmError::Auth(_)));
    assert!(!error.to_string().contains("Bearer credential"));
    assert!(matches!(classify(Some(502), None, "not-json secret", false), LlmError::Transient(_)));
    let mut decoder = StreamDecoder::new("auto");
    let mut sink: Sink<'_> = Box::new(|_| {});
    let malformed = decoder.on_event("not-json secret", &mut sink).unwrap_err();
    assert!(!malformed.to_string().contains("secret"));
}
