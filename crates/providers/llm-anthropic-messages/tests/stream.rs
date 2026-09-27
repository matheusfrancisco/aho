//! Adapter against a local mock server serving fixtures in the Messages streaming format.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

mod mock;

use std::sync::{Arc, Mutex};

use aho_cap::{Composition, Env, Os, PolicyInput, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, Llm, LlmError, LlmRoute, Request, Response, Sink, StreamEvent};
use aho_log::{CapState, TOOL_CALL_RUNNING};
use aho_types::{CallId, ConsequenceSet, ContentBlock, Message, Role, StopReason, Usage};
use llm_anthropic_messages::{AnthropicConfig, AnthropicMessages, MessagesRoute, http_client};
use mock::{Canned, Captured, serve_once};
use serde_json::json;

fn request(messages: Vec<Message>) -> Request {
    Request {
        model: "claude-sonnet-4-5".into(),
        system: vec![
            ("identity".into(), "You are aho.".into(), false),
            ("async-tools".into(), "Every call starts immediately.".into(), true),
            ("context".into(), "cwd: /work".into(), false),
        ],
        messages,
        tools: vec![],
        max_tokens: 1024,
        reasoning: None,
        cache_key: None,
    }
}

async fn run(canned: Canned, req: Request) -> (Result<Response, LlmError>, Vec<StreamEvent>, Captured) {
    let (base, server) = serve_once(canned).await;
    let route = MessagesRoute::new(AnthropicConfig::new(&format!("{base}/"), "sk-ant-test"), http_client().expect("client"));
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let sink: Sink<'_> = Box::new(move |e| seen.lock().expect("lock").push(e));
    let result = route.stream(req, sink).await;
    let captured = server.await.expect("server");
    let events = events.lock().expect("lock").clone();
    (result, events, captured)
}

#[tokio::test]
async fn text_stream_and_request_headers_and_cache_breakpoint() {
    let (res, events, seen) = run(Canned::sse(include_bytes!("../fixtures/anthropic-text.sse")), request(vec![Message::user("hi")])).await;
    let res = res.expect("response");
    assert_eq!(res.content, vec![ContentBlock::text("Hello from Claude — ünïcode.")]);
    assert_eq!(res.stop, StopReason::EndTurn);
    assert_eq!(res.usage, Usage { input_tokens: 25, output_tokens: 12, cache_read_tokens: 0, cache_write_tokens: 1800 });
    assert_eq!(events[..2], [StreamEvent::Text("Hello".into()), StreamEvent::Text(" from Claude — ünïcode.".into())]);
    assert_eq!(events.last(), Some(&StreamEvent::Usage(res.usage)));

    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/v1/messages"));
    assert_eq!(seen.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen.body["stream"], json!(true));
    assert_eq!(seen.body["max_tokens"], json!(1024));
    assert_eq!(
        seen.body["system"],
        json!([
            {"type": "text", "text": "You are aho."},
            {"type": "text", "text": "Every call starts immediately.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "cwd: /work"},
        ])
    );
    assert_eq!(seen.body["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]));
}

#[tokio::test]
async fn thinking_and_tool_use_stream_with_split_input_json() {
    let (res, events, _) =
        run(Canned::sse(include_bytes!("../fixtures/anthropic-thinking-tool-use.sse")), request(vec![Message::user("run the tests")]))
            .await;
    let res = res.expect("response");
    let id = CallId("toolu_01T1x1fJ34qAmk2tNTrN7Up6".into());
    let args = json!({"command": "cargo test", "timeout_ms": 60000});
    assert_eq!(
        res.content,
        vec![
            ContentBlock::Thinking { text: "The user wants the tests run. I'll call bash.".into() },
            ContentBlock::text("Running the suite."),
            ContentBlock::ToolUse { id: id.clone(), name: "bash".into(), args: args.clone() },
        ]
    );
    assert_eq!(res.stop, StopReason::ToolUse);
    assert_eq!(res.usage, Usage { input_tokens: 472, output_tokens: 89, cache_read_tokens: 2048, cache_write_tokens: 0 });
    assert_eq!(
        events,
        vec![
            StreamEvent::Thinking("The user wants the tests run. ".into()),
            StreamEvent::Thinking("I'll call bash.".into()),
            StreamEvent::Text("Running the suite.".into()),
            StreamEvent::ToolUseStart { id: id.clone(), name: "bash".into() },
            StreamEvent::ToolUseEnd { id, name: "bash".into(), args },
            StreamEvent::Usage(res.usage),
        ]
    );
}

#[tokio::test]
async fn late_result_becomes_a_user_block_and_results_follow_their_tool_use() {
    let tool_use = |id: &str| ContentBlock::ToolUse { id: CallId(id.into()), name: "bash".into(), args: json!({"command": id}) };
    let result =
        |id: &str, text: &str| ContentBlock::ToolResult { id: CallId(id.into()), content: vec![ContentBlock::text(text)], is_error: false };
    // derive_messages yields: placeholder for `b`, a deferred notice, later the late result
    let history = vec![
        Message::user("run both"),
        Message::assistant(vec![
            ContentBlock::Thinking { text: "unsigned".into() },
            ContentBlock::text("Starting."),
            tool_use("a"),
            tool_use("b"),
        ]),
        Message { role: Role::User, content: vec![result("a", "ok"), result("b", TOOL_CALL_RUNNING)] },
        Message::user("[notice] file changed"),
        Message::assistant(vec![ContentBlock::text("Reading code meanwhile.")]),
        Message::user("status?"),
        Message { role: Role::User, content: vec![result("b", "412 passed")] },
    ];
    let (res, _, seen) = run(Canned::sse(include_bytes!("../fixtures/anthropic-text.sse")), request(history)).await;
    res.expect("response");
    assert_eq!(
        seen.body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "run both"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Starting."},
                {"type": "tool_use", "id": "a", "name": "bash", "input": {"command": "a"}},
                {"type": "tool_use", "id": "b", "name": "bash", "input": {"command": "b"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "text", "text": "ok"}]},
                {"type": "tool_result", "tool_use_id": "b", "content": [{"type": "text", "text": TOOL_CALL_RUNNING}]},
                {"type": "text", "text": "[notice] file changed"},
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "Reading code meanwhile."}]},
            {"role": "user", "content": [
                {"type": "text", "text": "status?"},
                {"type": "text", "text": "[result for call b (bash)]\n412 passed"},
            ]},
        ])
    );
}

#[tokio::test]
async fn http_errors_map_to_seam_errors() {
    let msgs = || request(vec![Message::user("hi")]);
    let (res, _, _) =
        run(Canned::json(429, include_bytes!("../fixtures/anthropic-rate-limited.json")).header("retry-after", "12"), msgs()).await;
    assert_eq!(res, Err(LlmError::RateLimited { retry_after_ms: 12_000 }));
    let (res, _, _) = run(Canned::json(529, include_bytes!("../fixtures/anthropic-overloaded.json")), msgs()).await;
    assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("HTTP 529") && m.contains("Overloaded")));
    let (res, _, _) = run(Canned::json(400, include_bytes!("../fixtures/anthropic-prompt-too-long.json")), msgs()).await;
    assert_eq!(res, Err(LlmError::ContextWindowExceeded { requested: 208_310, available: 200_000 }));
    let (res, _, _) =
        run(Canned::json(401, br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#), msgs()).await;
    assert!(matches!(res, Err(LlmError::Auth(m)) if m.contains("invalid x-api-key")));
}

#[tokio::test]
async fn mid_stream_overloaded_error_is_transient() {
    let (res, events, _) =
        run(Canned::sse(include_bytes!("../fixtures/anthropic-overloaded.sse")), request(vec![Message::user("hi")])).await;
    assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("mid-stream") && m.contains("Overloaded")));
    assert_eq!(events, vec![StreamEvent::Text("Par".into())]);
}

#[tokio::test]
async fn truncated_stream_is_transient() {
    let full = include_str!("../fixtures/anthropic-text.sse");
    let cut = &full[..full.find("event: message_delta").expect("fixture has message_delta")];
    let (res, _, _) = run(Canned::sse(cut.as_bytes()), request(vec![Message::user("hi")])).await;
    assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("closed the stream")));
}

fn start(sets: &[(&str, &str)]) -> (CapState, aho_cap::Registry, Vec<String>) {
    let comp = Composition::new("t").provide::<AnthropicMessages>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let inputs = ResolveInputs { sets: sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() };
    let loadout = resolve(&inputs, &schema, false).expect("resolve");
    let plan = plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL },
        vec![],
    )
    .expect("plan");
    let state = plan.row("llm-anthropic-messages").expect("row").state.clone();
    let (reg, broken) = comp.start(&plan, &loadout);
    (state, reg, broken.into_iter().map(|(_, e)| e).collect())
}

#[test]
fn plans_locked_without_a_key_and_registers_the_route_with_one() {
    let (state, _, _) = start(&[]);
    assert!(matches!(state, CapState::Locked { .. }));
    let (state, reg, broken) = start(&[("provider.anthropic.key", "sk-ant-1")]);
    assert_eq!(state, CapState::Active);
    assert!(broken.is_empty(), "{broken:?}");
    let route: LlmRoute = reg.service("llm:anthropic").expect("route");
    assert_eq!(route.0.info().late_results, LateResults::AsUserBlock);
}

#[test]
fn kimi_only_setup_is_active_without_an_anthropic_key() {
    let (state, reg, broken) = start(&[("provider.kimi-coding.key", "kimi-key")]);
    assert_eq!(state, CapState::Active);
    assert!(broken.is_empty(), "{broken:?}");
    let route: LlmRoute = reg.service("llm:kimi-coding").expect("Kimi route");
    assert_eq!(route.0.info().route, "kimi-coding");
    assert!(reg.service::<LlmRoute>("llm:anthropic").is_none());
}

#[test]
fn anthropic_and_kimi_credentials_register_both_routes() {
    let (state, reg, broken) = start(&[("provider.kimi-coding.key", "kimi-key"), ("provider.anthropic.key", "anthropic-key")]);
    assert_eq!(state, CapState::Active);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.service::<LlmRoute>("llm:anthropic").is_some());
    assert!(reg.service::<LlmRoute>("llm:kimi-coding").is_some());
}
