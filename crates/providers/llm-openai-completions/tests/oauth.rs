//! OAuth reload and native Copilot routing against local streaming endpoints.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod mock;

use std::collections::BTreeMap;

use aho_llm::{Llm, Request};
use aho_types::{CallId, ContentBlock, Message, Role, StopReason};
use llm_openai_completions::{CompletionsRoute, Flavor, RouteConfig, http_client};
use mock::{Canned, serve_once, serve_sequence};
use provider_auth::{Credential, Store};
use serde_json::json;

fn credential(token: &str) -> Credential {
    Credential { access_token: token.into(), refresh_token: None, expires_at: None, metadata: BTreeMap::new() }
}

fn request(model: &str) -> Request {
    Request {
        model: model.into(),
        system: vec![("identity".into(), "You are aho.".into(), false)],
        messages: vec![Message::user("run a tool")],
        tools: vec![],
        max_tokens: 256,
        cache_key: None,
    }
}

#[tokio::test]
async fn openrouter_reloads_oauth_between_turns_and_preserves_tool_results() {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let path = root.join("auth.json");
    let store = Store::new(path.clone());
    let (base, server) = serve_sequence(vec![
        Canned::sse(include_bytes!("../fixtures/openai-tool-calls.sse")),
        Canned::sse(include_bytes!("../fixtures/openai-text.sse")),
    ])
    .await;
    let route = CompletionsRoute::new(
        RouteConfig::new("openrouter", &format!("{base}/api/v1"), Some(format!("oauth:{}", path.display())), Flavor::Compatible),
        http_client().expect("client"),
    );
    store.set("openrouter", credential("first-token")).await.expect("save");
    let mut req = request("anthropic/claude-sonnet-4.5");
    let response = route.stream(req.clone(), Box::new(|_| {})).await.expect("tool response");
    assert_eq!(response.stop, StopReason::ToolUse);
    req.messages.push(Message::assistant(response.content));
    req.messages.push(Message {
        role: Role::Tool,
        content: vec![
            ContentBlock::ToolResult {
                id: CallId("call_DdmO9pD3xa9XTPNJ32zg2hcA".into()),
                content: vec![ContentBlock::text("source")],
                is_error: false,
            },
            ContentBlock::ToolResult {
                id: CallId("call_q2Wm7e0nV8bXg4LkJ1s9TzRf".into()),
                content: vec![ContentBlock::text("passed")],
                is_error: false,
            },
        ],
    });
    store.set("openrouter", credential("second-token")).await.expect("replace");
    let response = route.stream(req, Box::new(|_| {})).await.expect("text response");
    assert_eq!(response.content, vec![ContentBlock::text("Hello from aho — ünïcode.")]);
    let captured = server.await.expect("server");
    assert_eq!(captured[0].path, "/api/v1/chat/completions");
    assert_eq!(captured[0].header("authorization"), Some("Bearer first-token"));
    assert_eq!(captured[1].header("authorization"), Some("Bearer second-token"));
    assert_eq!(captured[1].body["messages"][3], json!({"role":"tool","tool_call_id":"call_DdmO9pD3xa9XTPNJ32zg2hcA","content":"source"}));
    assert_eq!(captured[1].body["messages"][4], json!({"role":"tool","tool_call_id":"call_q2Wm7e0nV8bXg4LkJ1s9TzRf","content":"passed"}));
}

#[tokio::test]
async fn copilot_dispatches_models_to_native_protocols_with_oauth_and_tools() {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let path = root.join("auth.json");
    let store = Store::new(path.clone());
    store.set("github-copilot", credential("copilot-token;proxy-ep=proxy.production.invalid")).await.expect("save");
    let responses = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc1\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n";
    for (model, endpoint, fixture) in [
        (
            "claude-sonnet-4.5",
            "/v1/messages",
            include_bytes!("../../llm-anthropic-messages/fixtures/anthropic-thinking-tool-use.sse").as_slice(),
        ),
        ("gpt-5.4", "/responses", responses.as_slice()),
        ("grok-code-fast-1", "/responses", responses.as_slice()),
        ("gemini-2.5-pro", "/chat/completions", include_bytes!("../fixtures/openai-tool-calls.sse").as_slice()),
    ] {
        let (base, server) = serve_once(Canned::sse(fixture)).await;
        let route = CompletionsRoute::new(
            RouteConfig::new("github-copilot", &base, Some(format!("oauth:{}", path.display())), Flavor::Compatible),
            http_client().expect("client"),
        );
        let response = route.stream(request(model), Box::new(|_| {})).await.expect("native tool response");
        assert_eq!(response.stop, StopReason::ToolUse, "{model}");
        assert!(
            response
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { name, args, .. } if name == "bash" && args["command"].is_string())),
            "{model}"
        );
        let req = server.await.expect("server");
        assert_eq!(req.path, endpoint, "{model}");
        assert_eq!(req.header("authorization"), Some("Bearer copilot-token;proxy-ep=proxy.production.invalid"));
        assert_eq!(req.header("x-api-key"), None);
        assert_eq!(req.header("x-initiator"), Some("user"));
        assert_eq!(req.header("openai-intent"), Some("conversation-edits"));
        assert_eq!(req.header("copilot-integration-id"), Some("vscode-chat"));
        assert_ne!(req.body["system"][0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.");
    }
}

#[tokio::test]
async fn copilot_marks_tool_followups_and_vision_requests() {
    let (base, server) = serve_once(Canned::sse(include_bytes!("../fixtures/openai-text.sse"))).await;
    let route = CompletionsRoute::new(
        RouteConfig::new("github-copilot", &base, Some("key".into()), Flavor::Compatible),
        http_client().expect("client"),
    );
    let mut req = request("gemini-2.5-pro");
    req.messages.push(Message::assistant(vec![ContentBlock::ToolUse { id: CallId("c".into()), name: "read".into(), args: json!({}) }]));
    req.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            id: CallId("c".into()),
            content: vec![ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2, 3] }],
            is_error: false,
        }],
    });
    route.stream(req, Box::new(|_| {})).await.expect("response");
    let captured = server.await.expect("server");
    assert_eq!(captured.header("x-initiator"), Some("agent"));
    assert_eq!(captured.header("copilot-vision-request"), Some("true"));
    assert_eq!(captured.body["max_completion_tokens"], 256);
    assert!(captured.body.get("max_tokens").is_none());
}
