//! Adapter against a local mock server serving fixtures in the chat-completions wire format.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod mock;

use std::sync::{Arc, Mutex};

use aho_llm::{Llm, LlmError, Request, Response, Sink, StreamEvent};
use aho_types::{CallId, ContentBlock, Message, StopReason, Usage};
use llm_openai_completions::{CompletionsRoute, Flavor, RouteConfig, http_client};
use mock::{Canned, Captured, serve_once};
use serde_json::json;

fn request() -> Request {
    Request {
        model: "gpt-4.1-mini".into(),
        system: vec![("identity".into(), "You are aho.".into(), true), ("async".into(), "Go wide.".into(), false)],
        messages: vec![Message::user("say hello")],
        tools: vec![],
        max_tokens: 256,
        cache_key: Some("sess-1".into()),
    }
}

async fn run(canned: Canned, flavor: Flavor, key: Option<&str>) -> (Result<Response, LlmError>, Vec<StreamEvent>, Captured) {
    let (base, server) = serve_once(canned).await;
    let route = CompletionsRoute::new(
        RouteConfig::new("t", &format!("{base}/v1/"), key.map(str::to_string), flavor),
        http_client().expect("client"),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let sink: Sink<'_> = Box::new(move |e| seen.lock().expect("lock").push(e));
    let result = route.stream(request(), sink).await;
    let captured = server.await.expect("server");
    let events = events.lock().expect("lock").clone();
    (result, events, captured)
}

#[tokio::test]
async fn text_stream_emits_deltas_usage_and_final_response() {
    let (res, events, seen) = run(Canned::sse(include_bytes!("../fixtures/openai-text.sse")), Flavor::OpenAi, Some("sk-test")).await;
    let res = res.expect("response");
    assert_eq!(res.content, vec![ContentBlock::text("Hello from aho — ünïcode.")]);
    assert_eq!(res.stop, StopReason::EndTurn);
    let usage = Usage { input_tokens: 176, output_tokens: 6, cache_read_tokens: 1024, cache_write_tokens: 0 };
    assert_eq!(res.usage, usage);
    let texts: Vec<&str> = events.iter().filter_map(|e| if let StreamEvent::Text(t) = e { Some(t.as_str()) } else { None }).collect();
    assert_eq!(texts, ["Hello", " from", " aho — ünïcode."]);
    assert_eq!(events.last(), Some(&StreamEvent::Usage(usage)));

    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/v1/chat/completions"));
    assert_eq!(seen.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(seen.body["stream"], json!(true));
    assert_eq!(seen.body["stream_options"]["include_usage"], json!(true));
    assert_eq!(seen.body["max_completion_tokens"], json!(256));
    assert_eq!(seen.body["prompt_cache_key"], json!("sess-1"));
    assert_eq!(seen.body["messages"][0], json!({"role": "system", "content": "You are aho.\n\nGo wide."}));
}

#[tokio::test]
async fn tool_call_stream_accumulates_split_argument_deltas() {
    let (res, events, _) = run(Canned::sse(include_bytes!("../fixtures/openai-tool-calls.sse")), Flavor::OpenAi, Some("k")).await;
    let res = res.expect("response");
    assert_eq!(res.stop, StopReason::ToolUse);
    let read = CallId("call_DdmO9pD3xa9XTPNJ32zg2hcA".into());
    let bash = CallId("call_q2Wm7e0nV8bXg4LkJ1s9TzRf".into());
    assert_eq!(
        res.content,
        vec![
            ContentBlock::ToolUse { id: read.clone(), name: "read".into(), args: json!({"path": "src/main.rs"}) },
            ContentBlock::ToolUse { id: bash.clone(), name: "bash".into(), args: json!({"command": "cargo test"}) },
        ]
    );
    let tool_events: Vec<&StreamEvent> = events.iter().filter(|e| !matches!(e, StreamEvent::Usage(_))).collect();
    assert_eq!(
        tool_events,
        vec![
            &StreamEvent::ToolUseStart { id: read.clone(), name: "read".into() },
            // the first call is complete as soon as the second one opens (16: start on block end)
            &StreamEvent::ToolUseEnd { id: read, name: "read".into(), args: json!({"path": "src/main.rs"}) },
            &StreamEvent::ToolUseStart { id: bash.clone(), name: "bash".into() },
            &StreamEvent::ToolUseEnd { id: bash, name: "bash".into(), args: json!({"command": "cargo test"}) },
        ]
    );
    assert_eq!(res.usage, Usage { input_tokens: 342, output_tokens: 51, cache_read_tokens: 0, cache_write_tokens: 0 });
}

#[tokio::test]
async fn ollama_whole_tool_call_with_stop_finish_is_tool_use() {
    let (res, events, seen) = run(Canned::sse(include_bytes!("../fixtures/ollama-tool-call.sse")), Flavor::Compatible, None).await;
    let res = res.expect("response");
    assert_eq!(res.stop, StopReason::ToolUse, "finish_reason `stop` with calls still ends in tool use");
    assert_eq!(res.content[0], ContentBlock::Thinking { text: "The user wants the file listing.".into() });
    assert_eq!(
        res.content[1],
        ContentBlock::ToolUse { id: CallId("call_8k2hq1zn".into()), name: "glob".into(), args: json!({"pattern": "**/*.rs"}) }
    );
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Thinking(_))));
    assert_eq!(seen.header("authorization"), None, "keyless route sends no bearer");
    assert_eq!(seen.body["max_tokens"], json!(256));
    assert!(seen.body.get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn rate_limit_maps_retry_after_header() {
    let body = include_bytes!("../fixtures/openai-rate-limited.json");
    let (res, _, _) = run(Canned::json(429, body).header("retry-after", "7"), Flavor::OpenAi, Some("k")).await;
    assert_eq!(res, Err(LlmError::RateLimited { retry_after_ms: 7000 }));
    let (res, _, _) =
        run(Canned::json(429, body).header("retry-after-ms", "210").header("retry-after", "1"), Flavor::OpenAi, Some("k")).await;
    assert_eq!(res, Err(LlmError::RateLimited { retry_after_ms: 210 }));
}

#[tokio::test]
async fn context_overflow_bodies_map_to_context_window_exceeded() {
    let cases: [(&[u8], u32, u32); 3] = [
        (include_bytes!("../fixtures/openai-context-overflow.json"), 130_546, 128_000),
        (include_bytes!("../fixtures/llamacpp-context-overflow.json"), 9312, 8192),
        (include_bytes!("../fixtures/llamacpp-context-overflow-legacy.json"), 8192, 4096),
    ];
    for (body, requested, available) in cases {
        let (res, _, _) = run(Canned::json(400, body), Flavor::Compatible, None).await;
        assert_eq!(res, Err(LlmError::ContextWindowExceeded { requested, available }));
    }
}

#[tokio::test]
async fn auth_and_server_failures_map_to_their_classes() {
    let (res, _, _) = run(
        Canned::json(401, br#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#),
        Flavor::OpenAi,
        Some("bad"),
    )
    .await;
    assert!(matches!(res, Err(LlmError::Auth(m)) if m.contains("provider.t.key")));
    let (res, _, _) = run(Canned::json(502, b"<html>bad gateway</html>"), Flavor::OpenAi, Some("k")).await;
    assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("HTTP 502")));
}

#[tokio::test]
async fn mid_stream_error_chunk_is_transient() {
    let (res, events, _) = run(Canned::sse(include_bytes!("../fixtures/openai-stream-error.sse")), Flavor::OpenAi, Some("k")).await;
    assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("mid-stream")));
    assert_eq!(events, vec![StreamEvent::Text("Par".into())]);
}

#[tokio::test]
async fn unreachable_server_is_transient_with_the_endpoint_named() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let route = CompletionsRoute::new(
        RouteConfig::new("ollama", &format!("http://{addr}/v1"), None, Flavor::Compatible),
        http_client().expect("client"),
    );
    let res = route.stream(request(), Box::new(|_| {})).await;
    assert!(
        matches!(res, Err(LlmError::Transient(m)) if m.contains("could not connect to route `ollama`") && m.contains("provider.ollama.base_url"))
    );
}
