#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod mock;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aho_llm::{Llm, Request, StreamEvent};
use aho_types::{CallId, Consequence, ContentBlock, Message, Role, RootSchema, ToolSpec};
use llm_anthropic_messages::{AnthropicConfig, MessagesRoute, http_client};
use mock::{Canned, serve_once, serve_sequence};
use provider_auth::{Credential, Store};
use serde_json::json;

fn credential(token: &str) -> Credential {
    Credential { access_token: token.into(), refresh_token: None, expires_at: None, metadata: BTreeMap::new() }
}

fn request() -> Request {
    Request {
        model: "claude-sonnet-4-5".into(),
        system: vec![("identity".into(), "You are aho.".into(), true)],
        messages: vec![
            Message::user("continue"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: CallId("prior".into()),
                name: "bash".into(),
                args: json!({"command": "pwd"}),
            }]),
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    id: CallId("prior".into()),
                    content: vec![ContentBlock::text("/work")],
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "bash".into(),
            description: "Run a command".into(),
            input_schema: RootSchema(json!({"type": "object"})),
            class: Consequence::ReadOnly,
        }],
        max_tokens: 128,
        cache_key: None,
    }
}

#[tokio::test]
async fn oauth_is_resolved_each_turn_and_claude_tool_names_round_trip() {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let path = root.join("auth.json");
    let store = Store::new(path.clone());
    store.set("anthropic", credential("oauth-first")).await.expect("save");
    let fixture = include_str!("../fixtures/anthropic-thinking-tool-use.sse").replace("\"name\":\"bash\"", "\"name\":\"Bash\"");
    let (base, server) = serve_sequence(vec![Canned::sse(fixture.as_bytes()), Canned::sse(fixture.as_bytes())]).await;
    let route = MessagesRoute::new(AnthropicConfig::new(&base, &format!("oauth:{}", path.display())), http_client().expect("client"));
    let events = Arc::new(Mutex::new(Vec::new()));
    for token in ["oauth-first", "oauth-second"] {
        store.set("anthropic", credential(token)).await.expect("replace credential");
        let seen = events.clone();
        let response = route.stream(request(), Box::new(move |e| seen.lock().expect("lock").push(e))).await.expect("response");
        assert!(
            response
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { name, args, .. } if name == "bash" && args["command"] == "cargo test"))
        );
    }
    let captured = server.await.expect("server");
    assert_eq!(captured[0].header("authorization"), Some("Bearer oauth-first"));
    assert_eq!(captured[1].header("authorization"), Some("Bearer oauth-second"));
    for req in captured {
        assert_eq!(req.path, "/v1/messages?beta=true");
        assert_eq!(req.header("x-api-key"), None);
        assert_eq!(req.header("x-app"), Some("cli"));
        assert!(req.header("anthropic-beta").expect("OAuth beta").contains("oauth-2025-04-20"));
        assert_eq!(req.body["system"][0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.");
        assert_eq!(req.body["system"][1]["text"], "You are aho.");
        assert_eq!(req.body["tools"][0]["name"], "Bash");
        assert_eq!(req.body["messages"][1]["content"][0]["name"], "Bash");
        assert_eq!(req.body["messages"][2]["content"][0]["tool_use_id"], "prior");
    }
    assert!(events.lock().expect("events").iter().any(|e| matches!(e, StreamEvent::ToolUseEnd { name, .. } if name == "bash")));
}

#[tokio::test]
async fn kimi_oauth_uses_bearer_without_claude_identity_or_tool_renaming() {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let path = root.join("auth.json");
    let store = Store::new(path.clone());
    store.set("kimi-coding", credential("kimi-oauth")).await.expect("save");
    let (base, server) = serve_once(Canned::sse(include_bytes!("../fixtures/anthropic-text.sse"))).await;
    let route = MessagesRoute::new(
        AnthropicConfig::for_provider("kimi-coding", &format!("{base}/coding"), &format!("oauth:{}", path.display())),
        http_client().expect("client"),
    );
    route.stream(request(), Box::new(|_| {})).await.expect("response");
    let req = server.await.expect("server");
    assert_eq!(req.path, "/coding/v1/messages");
    assert_eq!(req.header("authorization"), Some("Bearer kimi-oauth"));
    assert_eq!(req.header("x-api-key"), None);
    assert_eq!(req.header("anthropic-beta"), None);
    assert_eq!(req.body["system"][0]["text"], "You are aho.");
    assert_eq!(req.body["tools"][0]["name"], "bash");
}
