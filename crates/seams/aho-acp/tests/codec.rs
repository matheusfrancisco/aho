//! Framing, decode rules, the peer's request/notification semantics, and the schema against the
//! examples in the ACP v1 protocol docs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_acp::schema::*;
use aho_acp::{Frame, FrameReader, Handler, HandlerFuture, Message, RequestId, RpcError, code, connection, decode, methods};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ───────────────────────────── decode rules ─────────────────────────────

#[test]
fn messages_are_classified_by_method_and_id() {
    let r = decode(r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"s"}}"#).unwrap();
    assert_eq!(r, Message::request(RequestId::Number(7), "session/prompt", json!({"sessionId": "s"})));
    let n = decode(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#).unwrap();
    assert_eq!(n, Message::notification("session/cancel", json!({"sessionId": "s"})));
    let ok = decode(r#"{"jsonrpc":"2.0","id":"a","result":{"stopReason":"end_turn"}}"#).unwrap();
    assert_eq!(ok, Message::response(RequestId::Str("a".into()), Ok(json!({"stopReason": "end_turn"}))));
    let err = decode(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}}"#).unwrap();
    assert_eq!(err, Message::response(RequestId::Number(3), Err(RpcError::new(code::METHOD_NOT_FOUND, "nope"))));
    // params may be omitted
    let bare = decode(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
    assert_eq!(bare, Message::request(RequestId::Null, "x", Value::Null));
}

#[test]
fn malformed_frames_are_answered_with_the_right_error_and_responses_never_are() {
    let parse = decode("{not json").unwrap_err();
    assert_eq!((parse.reply_to, parse.error.code), (Some(RequestId::Null), code::PARSE_ERROR));

    let batch = decode(r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"}]"#).unwrap_err();
    assert_eq!((batch.reply_to, batch.error.code), (Some(RequestId::Null), code::INVALID_REQUEST));
    assert!(batch.error.message.contains("batches"));

    let version = decode(r#"{"jsonrpc":"1.0","id":4,"method":"initialize"}"#).unwrap_err();
    assert_eq!((version.reply_to, version.error.code), (Some(RequestId::Number(4)), code::INVALID_REQUEST));

    let scalar = decode(r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":3}"#).unwrap_err();
    assert_eq!(scalar.reply_to, Some(RequestId::Number(5)));

    let bad_id = decode(r#"{"jsonrpc":"2.0","id":1.5,"method":"initialize"}"#).unwrap_err();
    assert_eq!((bad_id.reply_to, bad_id.error.code), (Some(RequestId::Null), code::INVALID_REQUEST));

    // response-shaped garbage is dropped, never answered: two peers must not ping-pong errors
    let both = decode(r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"x"}}"#).unwrap_err();
    assert_eq!(both.reply_to, None);
    let bad_error = decode(r#"{"jsonrpc":"2.0","id":1,"error":"boom"}"#).unwrap_err();
    assert_eq!(bad_error.reply_to, None);
}

#[test]
fn encoded_frames_never_contain_a_newline() {
    let m = Message::notification("session/update", json!({"text": "line one\nline two\r\n", "id": 1}));
    let f = m.encode();
    assert!(!f.contains('\n') && !f.contains('\r'), "{f}");
    assert_eq!(decode(&f).unwrap(), m);
    let v: Value = serde_json::from_str(&Message::response(RequestId::Number(1), Err(RpcError::method_not_found("x/y"))).encode()).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["error"]["code"], -32601);
    assert_eq!(v["error"]["data"]["method"], "x/y");
    assert!(v.get("result").is_none());
}

// ───────────────────────────── framing ─────────────────────────────

async fn frames(input: Vec<u8>, limit: usize) -> Vec<Frame> {
    let (mut a, b) = tokio::io::duplex(8);
    tokio::spawn(async move {
        for chunk in input.chunks(5) {
            a.write_all(chunk).await.unwrap();
        }
    });
    let mut r = FrameReader::with_limit(BufReader::new(b), limit);
    let mut out = Vec::new();
    while let Some(f) = r.next().await.unwrap() {
        out.push(f);
    }
    out
}

#[tokio::test]
async fn frames_split_on_newlines_across_small_reads() {
    let got = frames(b"{\"a\":1}\n\n  \r\n{\"b\":2}\r\n{\"c\":3}".to_vec(), 1024).await;
    assert_eq!(got, vec![Frame::Line("{\"a\":1}".into()), Frame::Line("{\"b\":2}".into()), Frame::Line("{\"c\":3}".into())]);
}

#[tokio::test]
async fn an_oversized_line_is_discarded_whole_and_the_next_one_still_reads() {
    let mut input = vec![b'x'; 40];
    input.extend_from_slice(b"\n{\"ok\":true}\n");
    input.extend_from_slice(&[0xff, 0xfe, b'\n']);
    let got = frames(input, 16).await;
    assert_eq!(got, vec![Frame::TooLong(40), Frame::Line("{\"ok\":true}".into()), Frame::NotUtf8]);
    let e = Frame::TooLong(40).decode().unwrap_err();
    assert_eq!((e.reply_to, e.error.code), (Some(RequestId::Null), code::PARSE_ERROR));
}

// ───────────────────────────── peer ─────────────────────────────
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

fn seen(s: &Seen) -> std::sync::MutexGuard<'_, Vec<(String, Value)>> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

fn seen_list(s: &Seen) -> Vec<(String, Value)> {
    seen(s).clone()
}

/// Echoes `echo`, sleeps on `slow` until cancelled, knows nothing else; records notifications.
struct Echo {
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Handler for Echo {
    fn request(&self, method: String, params: Value) -> HandlerFuture {
        Box::pin(async move {
            match method.as_str() {
                "echo" => Ok(params),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(json!("late"))
                }
                other => Err(RpcError::method_not_found(other)),
            }
        })
    }
    fn notification(&self, method: String, params: Value) {
        seen(&self.seen).push((method, params));
    }
}

struct RawClient {
    w: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
}

impl RawClient {
    async fn send(&mut self, s: &str) {
        self.w.write_all(s.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }
    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line()).await.expect("a frame in time").unwrap().unwrap();
        assert!(!line.contains('\r'));
        serde_json::from_str(&line).unwrap()
    }
}

fn serve_echo() -> (RawClient, Seen, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (r, w) = tokio::io::split(b);
    let (_peer, driver) = connection(r, w);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(driver.run(Arc::new(Echo { seen: seen.clone() })));
    let (rd, w) = tokio::io::split(a);
    (RawClient { w, lines: BufReader::new(rd).lines() }, seen, task)
}

#[tokio::test]
async fn unknown_methods_get_method_not_found_and_bad_frames_get_errors_with_null_id() {
    let (mut c, seen, task) = serve_echo();
    c.send(r#"{"jsonrpc":"2.0","id":1,"method":"session/set_mode","params":{}}"#).await;
    let v = c.recv().await;
    assert_eq!((v["id"].clone(), v["error"]["code"].clone()), (json!(1), json!(code::METHOD_NOT_FOUND)));
    c.send("not json at all").await;
    let v = c.recv().await;
    assert_eq!((v["id"].clone(), v["error"]["code"].clone()), (Value::Null, json!(code::PARSE_ERROR)));
    // an unknown notification is not answered; the next request's answer is the next frame
    c.send(r#"{"jsonrpc":"2.0","method":"mystery/thing","params":{"x":1}}"#).await;
    c.send(r#"{"jsonrpc":"2.0","id":"e","method":"echo","params":{"hi":true}}"#).await;
    let v = c.recv().await;
    assert_eq!(v, json!({"jsonrpc": "2.0", "id": "e", "result": {"hi": true}}));
    assert_eq!(seen_list(&seen), vec![("mystery/thing".to_string(), json!({"x": 1}))]);
    drop(c);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancel_request_answers_the_request_once_with_request_cancelled() {
    let (mut c, _seen, task) = serve_echo();
    c.send(r#"{"jsonrpc":"2.0","id":9,"method":"slow"}"#).await;
    c.send(r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":9}}"#).await;
    let v = c.recv().await;
    assert_eq!((v["id"].clone(), v["error"]["code"].clone()), (json!(9), json!(code::REQUEST_CANCELLED)));
    // cancelling again (or an unknown id) is a no-op: the next frame answers the next request
    c.send(r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":9}}"#).await;
    c.send(r#"{"jsonrpc":"2.0","id":10,"method":"echo","params":[1]}"#).await;
    assert_eq!(c.recv().await["id"], 10);
    drop(c);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn two_peers_correlate_requests_and_keep_notifications_before_the_answer() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (client, client_driver) = connection(ar, aw);
    let (server, server_driver) = connection(br, bw);
    let client_seen = Arc::new(Mutex::new(Vec::new()));

    /// Server: on `work`, sends three notifications through its own peer, then answers.
    struct Worker {
        peer: aho_acp::Peer,
    }
    impl Handler for Worker {
        fn request(&self, method: String, params: Value) -> HandlerFuture {
            let peer = self.peer.clone();
            Box::pin(async move {
                if method != "work" {
                    return Err(RpcError::method_not_found(&method));
                }
                for i in 0..3 {
                    peer.notify("progress", json!({"i": i}))?;
                }
                Ok(json!({"done": params["n"]}))
            })
        }
        fn notification(&self, _: String, _: Value) {}
    }
    let s = tokio::spawn(server_driver.run(Arc::new(Worker { peer: server.clone() })));
    let seen = client_seen.clone();
    let recorder = move || {
        let seen = seen.clone();
        Echo { seen }
    };
    let c = tokio::spawn(client_driver.run(Arc::new(recorder())));

    let (r1, r2) = tokio::join!(client.request("work", json!({"n": 1})), client.request("work", json!({"n": 2})));
    assert_eq!((r1.unwrap(), r2.unwrap()), (json!({"done": 1}), json!({"done": 2})));
    // every notification sent before an answer was handled before that answer resolved
    assert_eq!(seen_list(&client_seen).len(), 6);
    let e = client.request("nope", Value::Null).await.unwrap_err();
    assert_eq!(e.code, code::METHOD_NOT_FOUND);
    let typed: Value = client.call("work", &json!({"n": "x"})).await.unwrap();
    assert_eq!(typed, json!({"done": "x"}));

    drop((client, server));
    s.abort();
    c.abort();
}

#[tokio::test]
async fn end_of_input_fails_waiting_requests_and_closes_the_peer() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (peer, driver) = connection(ar, aw);
    let task = tokio::spawn(driver.run(Arc::new(Echo { seen: Arc::new(Mutex::new(Vec::new())) })));
    let pending = peer.request("initialize", json!({"protocolVersion": 1}));
    // the other side reads the request, then goes away without answering
    let (br, _bw) = tokio::io::split(b);
    let mut lines = BufReader::new(br).lines();
    let sent: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!((sent["id"].clone(), sent["method"].clone()), (json!(1), json!("initialize")));
    drop(lines);
    drop(_bw);
    let e = pending.await.unwrap_err();
    assert!(e.message.contains("closed"), "{e}");
    task.await.unwrap().unwrap();
    assert!(peer.is_closed());
    assert!(peer.notify("session/cancel", json!({})).is_err());
}

// ───────────────────────────── schema ─────────────────────────────

#[test]
fn doc_examples_decode_into_the_typed_schema() {
    // prompt-turn §1
    let p: PromptRequest = serde_json::from_value(json!({
        "sessionId": "sess_abc123def456",
        "prompt": [
            {"type": "text", "text": "Can you analyze this code for potential issues?"},
            {"type": "resource", "resource": {"uri": "file:///home/user/project/main.py", "mimeType": "text/x-python", "text": "def f(): pass"}},
            {"type": "resource_link", "name": "spec", "uri": "file:///spec.md"}
        ]
    }))
    .unwrap();
    assert!(
        matches!(&p.prompt[1], ContentBlock::Resource(EmbeddedResource { resource: EmbeddedResourceResource::Text(t), .. }) if t.text == "def f(): pass")
    );
    assert!(matches!(&p.prompt[2], ContentBlock::ResourceLink(l) if l.uri == "file:///spec.md"));

    // tool_call, then a completed update with content
    let n: SessionNotification = serde_json::from_value(json!({
        "sessionId": "s", "update": {"sessionUpdate": "tool_call", "toolCallId": "call_001", "title": "Analyzing Python code", "kind": "other", "status": "pending"}
    }))
    .unwrap();
    let SessionUpdate::ToolCall(tc) = n.update else { panic!("tool_call") };
    assert_eq!((tc.tool_call_id.as_str(), tc.kind, tc.status), ("call_001", ToolKind::Other, ToolCallStatus::Pending));
    let u: SessionUpdate = serde_json::from_value(json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "call_001", "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": "Analysis complete"}}]
    }))
    .unwrap();
    assert!(
        matches!(&u, SessionUpdate::ToolCallUpdate(x) if x.status == Some(ToolCallStatus::Completed) && x.content.as_ref().map(Vec::len) == Some(1))
    );

    // plan
    let plan: SessionUpdate = serde_json::from_value(json!({
        "sessionUpdate": "plan", "entries": [{"content": "Check for syntax errors", "priority": "high", "status": "pending"}]
    }))
    .unwrap();
    assert!(matches!(&plan, SessionUpdate::Plan(pl) if pl.entries[0].priority == PlanEntryPriority::High));

    // permission request and both outcomes
    let req: RequestPermissionRequest = serde_json::from_value(json!({
        "sessionId": "s", "toolCall": {"toolCallId": "call_001"},
        "options": [{"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"}, {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}]
    }))
    .unwrap();
    assert_eq!(req.options[1].kind, PermissionOptionKind::RejectOnce);
    let sel: RequestPermissionResponse =
        serde_json::from_value(json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}})).unwrap();
    assert_eq!(sel.outcome, RequestPermissionOutcome::Selected { option_id: "allow-once".into() });
    let cancelled = serde_json::to_value(RequestPermissionResponse { outcome: RequestPermissionOutcome::Cancelled, meta: None }).unwrap();
    assert_eq!(cancelled, json!({"outcome": {"outcome": "cancelled"}}));
}

#[test]
fn chunks_serialize_with_the_discriminator_inline() {
    let u = SessionUpdate::AgentMessageChunk(ContentChunk::text("hi", Some("m1".into())));
    assert_eq!(
        serde_json::to_value(&u).unwrap(),
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}, "messageId": "m1"})
    );
    let r = serde_json::to_value(PromptResponse { stop_reason: StopReason::MaxTurnRequests, meta: None }).unwrap();
    assert_eq!(r, json!({"stopReason": "max_turn_requests"}));
}

#[test]
fn newer_variants_degrade_instead_of_failing() {
    let u: SessionUpdate = serde_json::from_value(json!({"sessionUpdate": "session_notice", "text": "x"})).unwrap();
    assert_eq!(u, SessionUpdate::Unknown);
    let t: ToolCall = serde_json::from_value(json!({"toolCallId": "a", "title": "t", "kind": "teleport"})).unwrap();
    assert_eq!((t.kind, t.status), (ToolKind::Other, ToolCallStatus::Pending));
    // omitted capabilities are unsupported
    let init: InitializeRequest = serde_json::from_value(json!({"protocolVersion": 1})).unwrap();
    assert!(!init.client_capabilities.fs.read_text_file && !init.client_capabilities.terminal);
    let resp: InitializeResponse = serde_json::from_value(json!({"protocolVersion": 1})).unwrap();
    assert!(!resp.agent_capabilities.load_session && resp.auth_methods.is_empty());
}

#[test]
fn mcp_servers_are_tagged_except_stdio() {
    let list: Vec<McpServer> = serde_json::from_value(json!([
        {"name": "fs", "command": "/bin/mcp-fs", "args": ["--ro"], "env": [{"name": "K", "value": "v"}]},
        {"type": "http", "name": "api", "url": "https://x.example/mcp", "headers": []},
        {"type": "sse", "name": "ev", "url": "https://x.example/sse", "headers": [{"name": "A", "value": "b"}]}
    ]))
    .unwrap();
    assert!(matches!(&list[0], McpServer::Stdio(s) if s.env[0].name == "K" && s.args == ["--ro"]));
    assert!(matches!(&list[1], McpServer::Http(h) if h.name == "api"));
    assert!(matches!(&list[2], McpServer::Sse(h) if h.headers.len() == 1));
    let back = serde_json::to_value(&list).unwrap();
    assert!(back[0].get("type").is_none());
    assert_eq!((back[1]["type"].clone(), back[2]["type"].clone()), (json!("http"), json!("sse")));
    assert!(serde_json::from_value::<McpServer>(json!({"type": "ws", "name": "x", "url": "u"})).is_err());
}

#[test]
fn negotiation_answers_the_request_when_supported_else_the_latest() {
    assert_eq!(aho_acp::negotiate(1), 1);
    assert_eq!(aho_acp::negotiate(2), 1);
    assert_eq!(aho_acp::negotiate(0), 1);
    assert_eq!(methods::SESSION_REQUEST_PERMISSION, "session/request_permission");
}
