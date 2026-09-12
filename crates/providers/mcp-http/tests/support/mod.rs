//! A Streamable HTTP MCP server for the tests: axum on its own thread and runtime (so a build that
//! blocks the test's thread cannot starve it), recording every request it sees.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Debug)]
pub struct Seen {
    pub http: &'static str,
    pub rpc: Option<String>,
    pub session: Option<String>,
    pub version: Option<String>,
    pub auth: Option<String>,
    pub accept: Option<String>,
    pub body: Value,
}

pub struct St {
    pub sessions: Mutex<BTreeSet<String>>,
    next_session: AtomicU64,
    pub seen: Mutex<Vec<Seen>>,
    pub tools: Mutex<Vec<Value>>,
    bearer: Option<String>,
    get_supported: bool,
    /// Every request carrying a session answers 404, even right after `initialize`.
    pub amnesia: AtomicBool,
    streams: Mutex<Vec<mpsc::UnboundedSender<Bytes>>>,
}

impl St {
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.seen).clone()
    }
    pub fn rpc(&self, method: &str) -> Vec<Seen> {
        self.seen().into_iter().filter(|s| s.rpc.as_deref() == Some(method)).collect()
    }
    pub fn http(&self, verb: &str) -> Vec<Seen> {
        self.seen().into_iter().filter(|s| s.http == verb).collect()
    }
    pub fn expire_sessions(&self) {
        lock(&self.sessions).clear();
    }
    /// Open GET notification streams.
    pub fn listeners(&self) -> usize {
        let mut s = lock(&self.streams);
        s.retain(|tx| !tx.is_closed());
        s.len()
    }
    /// Sends `msg` on every open GET stream; returns how many got it.
    pub fn push(&self, msg: Value) -> usize {
        let mut s = lock(&self.streams);
        s.retain(|tx| tx.send(Bytes::from(event(&msg))).is_ok());
        s.len()
    }
}

pub struct Mock {
    pub url: String,
    pub st: Arc<St>,
}

pub fn tool(name: &str, description: &str) -> Value {
    json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}})
}

pub fn default_tools() -> Vec<Value> {
    vec![
        json!({"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        tool("progress", "Streams progress, then answers"),
        tool("boom", "Fails with a JSON-RPC error"),
        tool("fail", "Reports isError"),
        tool("http500", "Answers HTTP 500"),
        tool("sleep", "Answers after three seconds"),
    ]
}

pub fn start(bearer: Option<&str>, get_supported: bool) -> Mock {
    let st = Arc::new(St {
        sessions: Mutex::new(BTreeSet::new()),
        next_session: AtomicU64::new(0),
        seen: Mutex::new(Vec::new()),
        tools: Mutex::new(default_tools()),
        bearer: bearer.map(str::to_string),
        get_supported,
        amnesia: AtomicBool::new(false),
        streams: Mutex::new(Vec::new()),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let state = st.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let app = Router::new().route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp)).with_state(state);
            axum::serve(listener, app).await.unwrap();
        });
    });
    let addr = rx.recv().unwrap();
    Mock { url: format!("http://{addr}/mcp"), st }
}

fn event(v: &Value) -> String {
    format!("event: message\ndata: {v}\n\n")
}

fn result(id: Value, r: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": r})
}

fn json_body(v: Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn sse_body(events: Vec<String>) -> Response {
    let stream = futures::stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(Bytes::from(e))));
    Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from_stream(stream)).unwrap()
}

fn record(st: &St, http: &'static str, headers: &HeaderMap, body: &Value) {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    lock(&st.seen).push(Seen {
        http,
        rpc: body.get("method").and_then(Value::as_str).map(str::to_string),
        session: h("mcp-session-id"),
        version: h("mcp-protocol-version"),
        auth: h("authorization"),
        accept: h("accept"),
        body: body.clone(),
    });
}

/// `None` when the request may proceed.
fn gate(st: &St, headers: &HeaderMap, needs_session: bool) -> Option<Response> {
    if let Some(tok) = &st.bearer {
        let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
        if auth != Some(format!("Bearer {tok}").as_str()) {
            return Some((StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "missing or wrong token").into_response());
        }
    }
    if !needs_session {
        return None;
    }
    match headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        None => Some((StatusCode::BAD_REQUEST, "missing session").into_response()),
        Some(s) if st.amnesia.load(Ordering::SeqCst) || !lock(&st.sessions).contains(s) => {
            Some((StatusCode::NOT_FOUND, "unknown session").into_response())
        }
        Some(_) => None,
    }
}

async fn post_mcp(State(st): State<Arc<St>>, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    record(&st, "POST", &headers, &msg);
    let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
    let initialize = method.as_deref() == Some("initialize");
    if let Some(refused) = gate(&st, &headers, !initialize) {
        return refused;
    }
    let id = msg.get("id").cloned();
    if initialize {
        let sid = format!("s-{}", st.next_session.fetch_add(1, Ordering::SeqCst) + 1);
        lock(&st.sessions).insert(sid.clone());
        let r = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": true}, "resources": {}},
            "serverInfo": {"name": "mock", "version": "1.0"},
            "instructions": "Use echo."
        });
        return ([("mcp-session-id", sid)], json_body(result(id.unwrap_or(Value::Null), r))).into_response();
    }
    // Notifications and responses to the server's requests.
    let (Some(id), Some(method)) = (id, method) else { return StatusCode::ACCEPTED.into_response() };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "tools/list" => {
            let tools = lock(&st.tools).clone();
            let from: usize = params.get("cursor").and_then(Value::as_str).and_then(|c| c.parse().ok()).unwrap_or(0);
            let mut r = json!({"tools": tools.iter().skip(from).take(4).cloned().collect::<Vec<_>>()});
            if from + 4 < tools.len() {
                r["nextCursor"] = json!((from + 4).to_string());
            }
            json_body(result(id, r))
        }
        "tools/call" => call(id, params).await,
        "resources/list" => {
            json_body(result(id, json!({"resources": [{"uri": "mem://readme", "name": "readme", "mimeType": "text/plain"}]})))
        }
        "resources/read" => {
            json_body(result(id, json!({"contents": [{"uri": params["uri"], "mimeType": "text/plain", "text": "hello from http"}]})))
        }
        _ => json_body(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}})),
    }
}

async fn call(id: Value, params: Value) -> Response {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match params.get("name").and_then(Value::as_str).unwrap_or("") {
        "echo" => json_body(result(id, json!({"content": [{"type": "text", "text": args["text"]}]}))),
        "progress" => {
            let token = params.pointer("/_meta/progressToken").cloned().unwrap_or(Value::Null);
            sse_body(vec![
                ": stream open\n\n".into(),
                "id: e1\ndata:\n\n".into(),
                event(
                    &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": token, "progress": 1, "total": 2, "message": "half"}}),
                ),
                event(&json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})),
                format!("data: {}\n\n", result(id, json!({"content": [{"type": "text", "text": "done"}]}))),
            ])
        }
        "boom" => json_body(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "kaboom"}})),
        "fail" => json_body(result(id, json!({"isError": true, "content": [{"type": "text", "text": "it broke"}]}))),
        "http500" => (StatusCode::INTERNAL_SERVER_ERROR, "database on fire").into_response(),
        "sleep" => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            json_body(result(id, json!({"content": []})))
        }
        other => json_body(result(id, json!({"isError": true, "content": [{"type": "text", "text": format!("unknown tool {other}")}]}))),
    }
}

async fn get_mcp(State(st): State<Arc<St>>, headers: HeaderMap) -> Response {
    record(&st, "GET", &headers, &Value::Null);
    if !st.get_supported {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if let Some(refused) = gate(&st, &headers, true) {
        return refused;
    }
    let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
    tx.send(Bytes::from_static(b": listening\n\n")).unwrap();
    lock(&st.streams).push(tx);
    let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx)) });
    Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from_stream(stream)).unwrap()
}

async fn delete_mcp(State(st): State<Arc<St>>, headers: HeaderMap) -> StatusCode {
    record(&st, "DELETE", &headers, &Value::Null);
    if let Some(s) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        lock(&st.sessions).remove(s);
    }
    StatusCode::OK
}
