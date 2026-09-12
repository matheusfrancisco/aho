//! MCP Streamable HTTP (spec revision `2025-06-18`, `basic/transports`) over one endpoint.
//!
//! * Every client message is a POST with `Accept: application/json, text/event-stream`. A request
//!   is answered by one JSON body or by an SSE stream that may carry server notifications and
//!   requests before the response. Notifications and responses the client sends expect `202`.
//! * The `Mcp-Session-Id` the server returns with `InitializeResult` rides on every later request,
//!   with `MCP-Protocol-Version: <negotiated>`. A 404 on a request that carried the session means
//!   the server dropped it: aho re-initializes once (concurrent callers share one renewal) and
//!   retries the request once.
//! * A GET opens the server's notification stream (405 = the server has none); it reconnects with
//!   backoff. `notifications/tools/list_changed` — on any stream — re-lists the tools and swaps the
//!   server's generation two-phase through the capability's [`ToolTable`] (`10`).
//! * `notifications/progress` for a call reach the tool's progress sink as `AHO_PROGRESS {json}`.
//!   Server requests are answered: `ping` → `{}`, anything else → `-32601`.
//! * A timed-out or cancelled request sends `notifications/cancelled`. Dropping the last handle
//!   sends DELETE with the session id (waiting briefly, never blocking the I/O thread).
//!
//! All I/O runs on [`IoRuntime`], the provider's own single-thread tokio runtime, so callers may be
//! on any executor — or none, at build time.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use aho_cap::CancelToken;
use aho_cap::tokio_like::Sender;
use aho_llm::sse::{SseDecoder, SseEvent};
use aho_mcp::*;
use aho_proc::PROGRESS_MARKER;
use aho_types::*;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Response, StatusCode};
use serde_json::json;
use tokio::time::Instant;

use crate::HttpServerSpec;
use crate::runtime::{IoRuntime, ToolTable, lock};
use crate::wire::{self, Incoming, ServerInfo};

pub const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
pub const VERSION_HEADER: HeaderName = HeaderName::from_static("mcp-protocol-version");
const ACCEPT_POST: &str = "application/json, text/event-stream";
const ACCEPT_GET: &str = "text/event-stream";
/// How often a waiting request checks its cancellation token.
const POLL: Duration = Duration::from_millis(20);
/// Upper bound on pages for one paginated list (a looping cursor is a server bug).
const MAX_PAGES: usize = 1000;
/// How long dropping the last handle waits for the DELETE.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
/// Largest response aho reads: one JSON body, or the sum of one POST stream's bytes.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Characters of an HTTP error body quoted in the message.
const ERROR_EXCERPT: usize = 400;
const LISTEN_RETRY_MIN: Duration = Duration::from_millis(250);
const LISTEN_RETRY_MAX: Duration = Duration::from_secs(30);

// ───────────────────────────────── server ─────────────────────────────────

/// State of the GET notification stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Listening {
    /// Not admitted yet (quarantined probes never listen).
    Off,
    Connecting,
    Connected,
    /// The server answered the GET with 405: it offers no notification stream.
    Unsupported,
    /// The last attempt failed or the stream closed; aho reconnects with backoff.
    Retrying(String),
}

/// Why [`HttpServer::connect`] failed, and the operator's fix.
#[derive(Clone, Debug)]
pub struct ConnectError {
    pub error: McpError,
    pub fix: String,
}

enum Failure {
    /// 404 on a request that carried the session id.
    Expired,
    Error(McpError),
}

impl From<McpError> for Failure {
    fn from(e: McpError) -> Failure {
        Failure::Error(e)
    }
}

enum Opened {
    Stream(Response),
    Unsupported,
    Expired(u64),
    Failed,
}

struct Session {
    id: Option<String>,
    info: ServerInfo,
    epoch: u64,
}

struct Inner {
    name: String,
    class: Option<Consequence>,
    url: reqwest::Url,
    /// The URL without user info, query, or fragment (they may hold secrets).
    shown: String,
    /// Operator headers and `Authorization`, sent on every request.
    headers: HeaderMap,
    credential: Option<String>,
    /// A token or `Authorization` header is configured (with or without a credential name).
    sends_auth: bool,
    http: reqwest::Client,
    rt: Arc<IoRuntime>,
    table: Arc<ToolTable>,
    timeout: Duration,
    handshake: Duration,
    next_id: AtomicU64,
    session: Mutex<Session>,
    renewing: tokio::sync::Mutex<()>,
    sinks: Mutex<HashMap<u64, Sender>>,
    /// Admitted: re-lists install into the table and the GET stream is open.
    live: AtomicBool,
    /// `tools/list_changed` notifications seen; `relisted` is how many a re-list has covered.
    changes: AtomicU64,
    relisted: tokio::sync::Mutex<u64>,
    relist_error: Mutex<Option<String>>,
    listener: Mutex<Option<tokio::task::AbortHandle>>,
    listening: Mutex<Listening>,
    /// Status of the last HTTP error answer (for the fix on a failed connect).
    last_status: AtomicU16,
}

/// A connected Streamable HTTP MCP server. Cheap to clone; the session is deleted when the last
/// clone drops.
#[derive(Clone)]
pub struct HttpServer {
    inner: Arc<Inner>,
    instructions: Option<Arc<str>>,
}

/// URL without user info, query, or fragment.
pub fn shown_url(u: &reqwest::Url) -> String {
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{}://{}{port}{}", u.scheme(), u.host_str().unwrap_or(""), u.path())
}

fn header_text(h: &HeaderMap, name: &HeaderName) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn content_type(r: &Response) -> String {
    header_text(r.headers(), &CONTENT_TYPE).unwrap_or_default().split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// A reqwest error and its causes, without the URL.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut parts = vec![e.to_string()];
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        let t = s.to_string();
        if !parts.iter().any(|p| p.contains(&t)) {
            parts.push(t);
        }
        source = s.source();
    }
    parts.join(": ")
}

fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn with_progress_token(params: Value, id: u64) -> Value {
    let mut params = if params.is_object() { params } else { json!({}) };
    if let Some(m) = params.as_object_mut()
        && let Some(meta) = m.entry("_meta").or_insert_with(|| json!({})).as_object_mut()
    {
        meta.insert("progressToken".into(), json!(id));
    }
    params
}

/// Removes a request's progress sink when the exchange ends, however it ends.
struct SinkGuard<'a> {
    sinks: &'a Mutex<HashMap<u64, Sender>>,
    id: u64,
}

impl Drop for SinkGuard<'_> {
    fn drop(&mut self) {
        lock(self.sinks).remove(&self.id);
    }
}

async fn cancelled(token: Option<&CancelToken>) {
    match token {
        Some(t) => {
            while !t.is_cancelled() {
                tokio::time::sleep(POLL).await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

impl HttpServer {
    /// POST `initialize` (protocol [`PROTOCOL_VERSION`]), keep the session id, POST
    /// `notifications/initialized`. Must run on `rt` (see [`IoRuntime::block`]).
    pub async fn connect(
        spec: &HttpServerSpec,
        http: reqwest::Client,
        rt: Arc<IoRuntime>,
        table: Arc<ToolTable>,
        handshake: Duration,
    ) -> Result<HttpServer, ConnectError> {
        let url_fix = format!("aho explain mcp.servers.{}.url", spec.name);
        let url = reqwest::Url::parse(&spec.url).map_err(|e| ConnectError {
            error: McpError::Handshake(format!("`mcp.servers.{}.url` is not a URL ({e}); aho did not connect to it", spec.name)),
            fix: url_fix.clone(),
        })?;
        let headers = spec.header_map().map_err(|(msg, fix)| ConnectError { error: McpError::Handshake(msg), fix })?;
        let inner = Arc::new(Inner {
            name: spec.name.clone(),
            class: spec.class,
            shown: shown_url(&url),
            url,
            headers,
            credential: spec.credential.clone(),
            sends_auth: spec.sends_auth(),
            http,
            rt,
            table,
            timeout: Duration::from_millis(spec.timeout_ms.max(1)),
            handshake,
            next_id: AtomicU64::new(1),
            session: Mutex::new(Session { id: None, info: ServerInfo::default(), epoch: 0 }),
            renewing: tokio::sync::Mutex::new(()),
            sinks: Mutex::new(HashMap::new()),
            live: AtomicBool::new(false),
            changes: AtomicU64::new(0),
            relisted: tokio::sync::Mutex::new(0),
            relist_error: Mutex::new(None),
            listener: Mutex::new(None),
            listening: Mutex::new(Listening::Off),
            last_status: AtomicU16::new(0),
        });
        match inner.initialize().await {
            Ok((id, info)) => {
                let instructions = info.instructions.as_deref().map(Arc::from);
                *lock(&inner.session) = Session { id, info, epoch: 1 };
                Ok(HttpServer { inner, instructions })
            }
            Err(error) => {
                let fix = match (inner.last_status.load(Ordering::SeqCst), &inner.credential) {
                    (401 | 403, Some(c)) => format!("aho credential set {c}"),
                    (401 | 403, None) => format!("aho set mcp.servers.{0}.bearer cred:{0}", spec.name),
                    _ if matches!(error, McpError::Timeout { .. }) => "aho set mcp.handshake_timeout_ms 60000".to_string(),
                    _ => url_fix,
                };
                Err(ConnectError { error, fix })
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// What the latest `initialize` answered (a renewed session may differ from the first).
    pub fn info(&self) -> ServerInfo {
        lock(&self.inner.session).info.clone()
    }

    pub fn session_id(&self) -> Option<String> {
        lock(&self.inner.session).id.clone()
    }

    /// This server's current tool generation in the capability's table.
    pub fn generation(&self) -> Option<Generation> {
        self.inner.table.current(&self.inner.name)
    }

    /// `notifications/tools/list_changed` seen so far.
    pub fn tool_list_changes(&self) -> u64 {
        self.inner.changes.load(Ordering::SeqCst)
    }

    /// Why the last re-list did not install (fetch failure keeps the old generation; a conflict
    /// installs none).
    pub fn relist_error(&self) -> Option<String> {
        lock(&self.inner.relist_error).clone()
    }

    pub fn listening(&self) -> Listening {
        lock(&self.inner.listening).clone()
    }

    /// Admits the server: re-lists install from now on, and the GET notification stream opens.
    pub fn go_live(&self) {
        if self.inner.live.swap(true, Ordering::SeqCst) {
            return;
        }
        let task = self.inner.rt.handle.spawn(listen(Arc::downgrade(&self.inner)));
        *lock(&self.inner.listener) = Some(task.abort_handle());
        if self.inner.changes.load(Ordering::SeqCst) > 0 {
            self.inner.spawn_relist();
        }
    }

    /// Every page of `tools/list` (empty when the server has no `tools` capability).
    pub fn list_tools_now(&self) -> BoxFuture<'static, Result<Vec<RemoteTool>, McpError>> {
        let inner = self.inner.clone();
        self.inner.rt.run(async move { inner.list_tools().await })
    }

    /// The raw tool name behind a registered name, or why the call cannot be sent.
    pub fn raw_name(&self, normalized: &str) -> Result<String, String> {
        let (table, name) = (&self.inner.table, &self.inner.name);
        table.raw_for(name, normalized).ok_or_else(|| {
            let n = table.current(name).map(|g| g.number).unwrap_or(0);
            format!(
                "`{normalized}` is no longer offered by MCP server `{name}`: its tool list changed (now generation {n}) and no longer includes it, so the call was not sent. Tools the server added appear after the session restarts."
            )
        })
    }

    /// `tools/call`; with a sink, the request carries a progress token and the server's
    /// `notifications/progress` arrive as `AHO_PROGRESS {"progress", "total", "message"}` lines.
    pub fn call_with_progress(
        &self,
        raw: &str,
        args: Value,
        cancel: CancelToken,
        progress: Option<Sender>,
    ) -> BoxFuture<'static, Result<CallResult, McpError>> {
        let inner = self.inner.clone();
        let arguments = if args.is_null() { json!({}) } else { args };
        let params = json!({"name": raw, "arguments": arguments});
        self.inner.rt.run(async move { inner.request("tools/call", params, Some(&cancel), progress).await.map(|r| wire::parse_call(&r)) })
    }
}

impl Inner {
    fn session_snapshot(&self) -> (Option<String>, Option<String>, u64) {
        let s = lock(&self.session);
        let version = (!s.info.protocol_version.is_empty()).then(|| s.info.protocol_version.clone());
        (s.id.clone(), version, s.epoch)
    }

    fn info(&self) -> ServerInfo {
        lock(&self.session).info.clone()
    }

    fn set_listening(&self, l: Listening) {
        *lock(&self.listening) = l;
    }

    fn session_headers(&self, sid: Option<&str>, version: Option<&str>) -> HeaderMap {
        let mut h = self.headers.clone();
        if let Some(v) = sid.and_then(|s| HeaderValue::from_str(s).ok()) {
            h.insert(SESSION_HEADER, v);
        }
        if let Some(v) = version.and_then(|s| HeaderValue::from_str(s).ok()) {
            h.insert(VERSION_HEADER, v);
        }
        h
    }

    fn unreachable(&self, e: reqwest::Error) -> McpError {
        McpError::Transport(format!(
            "could not reach MCP server `{}` at {} ({}); the request did not complete. Check `mcp.servers.{}.url` and that the server is running.",
            self.name,
            self.shown,
            describe(e),
            self.name
        ))
    }

    #[allow(clippy::disallowed_methods, reason = "authority: mcp-http-post")]
    async fn post(&self, body: &Value, sid: Option<&str>, version: Option<&str>) -> Result<Response, McpError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| McpError::Transport(format!("could not encode a message for MCP server `{}` ({e})", self.name)))?;
        self.http
            .post(self.url.clone())
            .headers(self.session_headers(sid, version))
            .header(ACCEPT, ACCEPT_POST)
            .header(CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|e| self.unreachable(e))
    }

    /// A notification or a response to a server request: any 2xx (the spec says 202) accepts it.
    async fn send_message(&self, msg: &Value, sid: Option<&str>, version: Option<&str>) -> Result<(), McpError> {
        let resp = self.post(msg, sid, version).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let what = msg.get("method").and_then(Value::as_str).unwrap_or("a response").to_string();
        Err(self.http_error(&what, resp).await)
    }

    /// Reads at most `limit` bytes of an error body.
    async fn excerpt(mut resp: Response) -> String {
        let mut out = Vec::new();
        while out.len() < 4096 {
            match resp.chunk().await {
                Ok(Some(b)) => out.extend_from_slice(&b),
                Ok(None) | Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).trim().to_string()
    }

    async fn http_error(&self, method: &str, resp: Response) -> McpError {
        let status = resp.status();
        self.last_status.store(status.as_u16(), Ordering::SeqCst);
        let body = Self::excerpt(resp).await;
        let detail = if body.is_empty() { String::new() } else { format!(": {}", clip(&body, ERROR_EXCERPT)) };
        let name = &self.name;
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return McpError::Transport(match (&self.credential, self.sends_auth) {
                (Some(c), _) => format!(
                    "MCP server `{name}` refused `{method}` with HTTP {status}{detail}. aho sent credential `{c}`; if it is wrong or expired, replace it with `aho credential set {c}`."
                ),
                (None, true) => format!(
                    "MCP server `{name}` refused `{method}` with HTTP {status}{detail}. aho sent the token written in `mcp.servers.{name}`; check it, or store it as a credential: `aho set mcp.servers.{name}.bearer cred:{name}` and `aho credential set {name}`."
                ),
                (None, false) => format!(
                    "MCP server `{name}` refused `{method}` with HTTP {status}{detail}. aho sent no credential; add one with `aho set mcp.servers.{name}.bearer cred:<name>` and `aho credential set <name>` (OAuth sign-in is not supported)."
                ),
            });
        }
        let rpc = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
            let e = v.get("error")?;
            Some((e.get("code")?.as_i64()?, e.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string()))
        });
        if let Some((code, message)) = rpc {
            return McpError::Rpc { code, message: format!("{message} (HTTP {status})") };
        }
        let hint = if method == "initialize" && matches!(status, StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED) {
            " The URL may be wrong, or the server may speak the older HTTP+SSE transport (2024-11-05), which aho does not support."
        } else {
            ""
        };
        McpError::Transport(format!("MCP server `{name}` answered `{method}` with HTTP {status}{detail}.{hint}"))
    }

    /// One POST exchange for request `id`; returns the result and any `Mcp-Session-Id` header.
    async fn exchange(
        self: &Arc<Self>,
        id: u64,
        method: &str,
        params: Value,
        sid: Option<&str>,
        version: Option<&str>,
        progress: Option<Sender>,
    ) -> Result<(Value, Option<String>), Failure> {
        let (params, _sink) = match progress {
            Some(p) => {
                lock(&self.sinks).insert(id, p);
                (with_progress_token(params, id), Some(SinkGuard { sinks: &self.sinks, id }))
            }
            None => (params, None),
        };
        let mut body = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if !params.is_null() {
            body["params"] = params;
        }
        let resp = self.post(&body, sid, version).await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND && sid.is_some() {
            return Err(Failure::Expired);
        }
        if !status.is_success() {
            return Err(self.http_error(method, resp).await.into());
        }
        let session = header_text(resp.headers(), &SESSION_HEADER);
        let value = match content_type(&resp).as_str() {
            "text/event-stream" => self.read_stream(resp, id, method).await?,
            "application/json" => self.read_json(resp, id, method).await?,
            _ if status == StatusCode::ACCEPTED => {
                return Err(McpError::Transport(format!(
                    "MCP server `{}` answered the request `{method}` with 202 Accepted and no response; the request was abandoned",
                    self.name
                ))
                .into());
            }
            other => {
                return Err(McpError::Transport(format!(
                    "MCP server `{}` answered `{method}` with content type `{other}`; aho reads only application/json and text/event-stream",
                    self.name
                ))
                .into());
            }
        };
        Ok((value, session))
    }

    fn too_large(&self, method: &str) -> McpError {
        McpError::Transport(format!(
            "MCP server `{}` sent more than {} MiB answering `{method}`; aho stopped reading and abandoned the request",
            self.name,
            MAX_RESPONSE_BYTES / (1024 * 1024)
        ))
    }

    fn broken(&self, method: &str, e: reqwest::Error) -> McpError {
        McpError::Transport(format!(
            "the connection to MCP server `{}` broke while it answered `{method}` ({}); the request may or may not have run",
            self.name,
            describe(e)
        ))
    }

    async fn read_json(self: &Arc<Self>, mut resp: Response, id: u64, method: &str) -> Result<Value, McpError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| self.broken(method, e))? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(self.too_large(method));
            }
            bytes.extend_from_slice(&chunk);
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
            McpError::Transport(format!("MCP server `{}` answered `{method}` with JSON aho could not read ({e})", self.name))
        })?;
        let mut answer = None;
        for m in wire::classify(v) {
            if let Some(r) = self.dispatch(m, Some(id)) {
                answer = Some(r);
            }
        }
        answer.unwrap_or_else(|| {
            Err(McpError::Transport(format!("MCP server `{}` answered `{method}` without a JSON-RPC response to it", self.name)))
        })
    }

    /// The JSON-RPC messages in one SSE event (`message` events only; empty data is a keep-alive).
    fn messages(&self, ev: SseEvent, context: &str) -> Result<Vec<Incoming>, McpError> {
        if !matches!(ev.event.as_deref(), None | Some("message")) || ev.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let v = serde_json::from_str(&ev.data)
            .map_err(|e| McpError::Transport(format!("MCP server `{}` sent an event that is not JSON ({e}) on {context}", self.name)))?;
        Ok(wire::classify(v))
    }

    async fn read_stream(self: &Arc<Self>, mut resp: Response, id: u64, method: &str) -> Result<Value, McpError> {
        let mut sse = SseDecoder::new();
        let mut read = 0usize;
        let context = format!("the stream answering `{method}`");
        loop {
            let chunk = resp.chunk().await.map_err(|e| self.broken(method, e))?;
            let done = chunk.is_none();
            let events = match chunk {
                Some(bytes) => {
                    read += bytes.len();
                    if read > MAX_RESPONSE_BYTES {
                        return Err(self.too_large(method));
                    }
                    sse.push(&bytes)
                }
                None => sse.finish().into_iter().collect(),
            };
            for ev in events {
                for m in self.messages(ev, &context)? {
                    if let Some(answer) = self.dispatch(m, Some(id)) {
                        return answer;
                    }
                }
            }
            if done {
                return Err(McpError::Transport(format!(
                    "MCP server `{}` closed the event stream for `{method}` before answering; the request may or may not have run. Retry it, or check the server's logs.",
                    self.name
                )));
            }
        }
    }

    /// Routes one server message; `Some` is the response to the request being awaited.
    fn dispatch(self: &Arc<Self>, m: Incoming, awaiting: Option<u64>) -> Option<Result<Value, McpError>> {
        match m {
            Incoming::Response { id, result } => {
                (awaiting.is_some() && id.as_u64() == awaiting).then(|| result.map_err(|(code, message)| McpError::Rpc { code, message }))
            }
            Incoming::Request { id, method, .. } => {
                self.answer(id, &method);
                None
            }
            Incoming::Notification { method, params } => {
                match method.as_str() {
                    "notifications/progress" => self.progress(&params),
                    "notifications/tools/list_changed" => self.tools_changed(),
                    _ => {}
                }
                None
            }
        }
    }

    /// Server → client requests: `ping` → `{}`; aho offers no other client capability.
    fn answer(self: &Arc<Self>, id: Value, method: &str) {
        let reply = if method == "ping" {
            json!({"jsonrpc": "2.0", "id": id, "result": {}})
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("aho does not handle `{method}`")}})
        };
        let inner = self.clone();
        self.rt.handle.spawn(async move {
            let (sid, version, _) = inner.session_snapshot();
            inner.send_message(&reply, sid.as_deref(), version.as_deref()).await
        });
    }

    fn progress(&self, params: &Value) {
        let Some(token) = params.get("progressToken").and_then(Value::as_u64) else { return };
        let Some(sink) = lock(&self.sinks).get(&token).cloned() else { return };
        let mut line = serde_json::Map::new();
        for k in ["progress", "total", "message"] {
            if let Some(v) = params.get(k) {
                line.insert(k.to_string(), v.clone());
            }
        }
        sink.send(format!("{PROGRESS_MARKER}{}", Value::Object(line)));
    }

    fn tools_changed(self: &Arc<Self>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        if self.live.load(Ordering::SeqCst) {
            self.spawn_relist();
        }
    }

    fn spawn_relist(self: &Arc<Self>) {
        let inner = self.clone();
        self.rt.handle.spawn(async move { inner.relist().await });
    }

    /// Re-lists once for every batch of `list_changed` notifications and installs the result.
    async fn relist(self: &Arc<Self>) {
        let mut handled = self.relisted.lock().await;
        let target = self.changes.load(Ordering::SeqCst);
        if *handled >= target {
            return;
        }
        let fetched = self.list_tools().await;
        *lock(&self.relist_error) = self.table.install(&self.name, fetched).err().map(|e| e.to_string());
        *handled = target;
    }

    fn cancel_remote(self: &Arc<Self>, id: u64, reason: &'static str) {
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": reason}});
        let inner = self.clone();
        self.rt.handle.spawn(async move {
            let (sid, version, _) = inner.session_snapshot();
            inner.send_message(&msg, sid.as_deref(), version.as_deref()).await
        });
    }

    /// `initialize` + `notifications/initialized`, within the handshake timeout. Never cancelled
    /// remotely (the spec forbids cancelling `initialize`).
    async fn initialize(self: &Arc<Self>) -> Result<(Option<String>, ServerInfo), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "aho", "version": env!("CARGO_PKG_VERSION")}
        });
        let deadline = Instant::now() + self.handshake;
        let ms = self.handshake.as_millis() as u64;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let wrap = |e: McpError| match e {
            McpError::Handshake(_) | McpError::Timeout { .. } => e,
            other => {
                McpError::Handshake(format!("MCP server `{}` did not complete `initialize`: {other} aho did not connect to it.", self.name))
            }
        };
        let (result, sid) = match tokio::time::timeout_at(deadline, self.exchange(id, "initialize", params, None, None, None)).await {
            Err(_) => return Err(McpError::Timeout { method: "initialize".into(), ms }),
            Ok(Err(Failure::Expired)) => return Err(wrap(McpError::Transport("the server answered 404".into()))),
            Ok(Err(Failure::Error(e))) => return Err(wrap(e)),
            Ok(Ok(v)) => v,
        };
        let info = ServerInfo::from_initialize(&self.name, &result)?;
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        match tokio::time::timeout_at(deadline, self.send_message(&note, sid.as_deref(), Some(&info.protocol_version))).await {
            Err(_) => Err(McpError::Timeout { method: "notifications/initialized".into(), ms }),
            Ok(Err(e)) => Err(wrap(e)),
            Ok(Ok(())) => Ok((sid, info)),
        }
    }

    /// Starts a new session after a 404, unless another caller already did since `seen`.
    async fn renew(self: &Arc<Self>, seen: u64) -> Result<(), McpError> {
        let _turn = self.renewing.lock().await;
        if lock(&self.session).epoch != seen {
            return Ok(());
        }
        let (id, info) = self.initialize().await.map_err(|e| {
            McpError::Transport(format!("MCP server `{}` dropped aho's session (HTTP 404) and starting a new one failed: {e}", self.name))
        })?;
        *lock(&self.session) = Session { id, info, epoch: seen + 1 };
        Ok(())
    }

    /// One JSON-RPC request within the server's timeout; a 404 re-initializes once and retries once.
    async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        cancel: Option<&CancelToken>,
        progress: Option<Sender>,
    ) -> Result<Value, McpError> {
        let deadline = Instant::now() + self.timeout;
        let mut renewed = false;
        loop {
            let (sid, version, epoch) = self.session_snapshot();
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let outcome = tokio::select! {
                r = self.exchange(id, method, params.clone(), sid.as_deref(), version.as_deref(), progress.clone()) => r,
                _ = tokio::time::sleep_until(deadline) => {
                    self.cancel_remote(id, "timed out");
                    return Err(McpError::Timeout { method: method.to_string(), ms: self.timeout.as_millis() as u64 });
                }
                _ = cancelled(cancel) => {
                    self.cancel_remote(id, "cancelled by aho");
                    return Err(McpError::Cancelled);
                }
            };
            match outcome {
                Ok((v, _)) => return Ok(v),
                Err(Failure::Error(e)) => return Err(e),
                Err(Failure::Expired) if !renewed => {
                    renewed = true;
                    self.renew(epoch).await?;
                }
                Err(Failure::Expired) => {
                    return Err(McpError::Transport(format!(
                        "MCP server `{}` rejected `{method}` with HTTP 404 again right after aho started a new session; the request was not retried further. Check the server's session handling or restart it.",
                        self.name
                    )));
                }
            }
        }
    }

    /// Every page of a cursor-paginated list; `key` names the array in each result.
    async fn paginate(self: &Arc<Self>, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request(method, params, None, None).await?;
            out.extend(r.get(key).and_then(Value::as_array).cloned().unwrap_or_default());
            match r.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => {
                    if !seen.insert(c.to_string()) {
                        return Err(McpError::Transport(format!(
                            "MCP server `{}` repeated the `{method}` cursor `{c}`; the list was not used",
                            self.name
                        )));
                    }
                    cursor = Some(c.to_string());
                }
                _ => return Ok(out),
            }
        }
        Err(McpError::Transport(format!("MCP server `{}` returned more than {MAX_PAGES} pages for `{method}`", self.name)))
    }

    async fn list_tools(self: &Arc<Self>) -> Result<Vec<RemoteTool>, McpError> {
        if !self.info().tools {
            return Ok(Vec::new());
        }
        let pages = self.paginate("tools/list", "tools").await?;
        pages.into_iter().map(|t| wire::parse_tool(&self.name, t)).collect()
    }

    fn require_resources(&self) -> Result<(), McpError> {
        if self.info().resources {
            Ok(())
        } else {
            Err(McpError::Unsupported(format!("MCP server `{}` does not offer resources", self.name)))
        }
    }

    /// GET the notification stream, within the handshake timeout.
    #[allow(clippy::disallowed_methods, reason = "authority: mcp-http-stream")]
    async fn open_stream(&self) -> Opened {
        self.set_listening(Listening::Connecting);
        let (sid, version, epoch) = self.session_snapshot();
        let req =
            self.http.get(self.url.clone()).headers(self.session_headers(sid.as_deref(), version.as_deref())).header(ACCEPT, ACCEPT_GET);
        let resp = match tokio::time::timeout(self.handshake, req.send()).await {
            Err(_) => {
                let ms = self.handshake.as_millis();
                self.set_listening(Listening::Retrying(format!("the notification-stream GET got no answer within {ms} ms")));
                return Opened::Failed;
            }
            Ok(Err(e)) => {
                self.set_listening(Listening::Retrying(self.unreachable(e).to_string()));
                return Opened::Failed;
            }
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            self.set_listening(Listening::Unsupported);
            return Opened::Unsupported;
        }
        if status == StatusCode::NOT_FOUND && sid.is_some() {
            return Opened::Expired(epoch);
        }
        if !status.is_success() {
            let e = self.http_error("GET (notification stream)", resp).await;
            self.set_listening(Listening::Retrying(e.to_string()));
            return Opened::Failed;
        }
        let ctype = content_type(&resp);
        if ctype != ACCEPT_GET {
            self.set_listening(Listening::Retrying(format!("the notification-stream GET was answered with content type `{ctype}`")));
            return Opened::Failed;
        }
        self.set_listening(Listening::Connected);
        Opened::Stream(resp)
    }
}

/// The GET listener. Holds only a weak reference between events so dropping the server ends it.
async fn listen(weak: Weak<Inner>) {
    let mut wait = LISTEN_RETRY_MIN;
    loop {
        let opened = match weak.upgrade() {
            Some(inner) => inner.open_stream().await,
            None => return,
        };
        match opened {
            Opened::Stream(resp) => {
                wait = LISTEN_RETRY_MIN;
                if !read_notifications(&weak, resp).await {
                    return;
                }
            }
            Opened::Unsupported => return,
            Opened::Expired(epoch) => {
                let Some(inner) = weak.upgrade() else { return };
                match inner.renew(epoch).await {
                    Ok(()) => continue,
                    Err(e) => inner.set_listening(Listening::Retrying(e.to_string())),
                }
            }
            Opened::Failed => {}
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(LISTEN_RETRY_MAX);
    }
}

/// Reads one GET stream to its end; `false` when the server handle is gone.
async fn read_notifications(weak: &Weak<Inner>, mut resp: Response) -> bool {
    let mut sse = SseDecoder::new();
    loop {
        let (events, ended) = match resp.chunk().await {
            Ok(Some(bytes)) => (sse.push(&bytes), None),
            Ok(None) => (sse.finish().into_iter().collect(), Some("the server closed the notification stream".to_string())),
            Err(e) => (sse.finish().into_iter().collect(), Some(format!("the notification stream broke ({})", describe(e)))),
        };
        let Some(inner) = weak.upgrade() else { return false };
        for ev in events {
            if let Ok(messages) = inner.messages(ev, "the notification stream") {
                for m in messages {
                    inner.dispatch(m, None);
                }
            }
        }
        if let Some(why) = ended {
            inner.set_listening(Listening::Retrying(why));
            return true;
        }
    }
}

impl Drop for Inner {
    #[allow(clippy::disallowed_methods, reason = "authority: mcp-http-session-close")]
    fn drop(&mut self) {
        if let Some(task) = lock(&self.listener).take() {
            task.abort();
        }
        let (sid, version) = {
            let s = lock(&self.session);
            (s.id.clone(), s.info.protocol_version.clone())
        };
        let Some(sid) = sid else { return };
        let req = self.http.delete(self.url.clone()).headers(self.session_headers(Some(&sid), Some(&version)));
        let done = Arc::new(AtomicBool::new(false));
        let (flag, waiter) = (done.clone(), std::thread::current());
        self.rt.handle.spawn(async move {
            let sent = tokio::time::timeout(SHUTDOWN_GRACE, req.send()).await;
            flag.store(true, Ordering::SeqCst);
            waiter.unpark();
            matches!(sent, Ok(Ok(_)))
        });
        if self.rt.on_runtime_thread() {
            return;
        }
        let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
        while !done.load(Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::park_timeout(left);
        }
    }
}

impl McpServer for HttpServer {
    fn name(&self) -> &str {
        &self.inner.name
    }

    fn class(&self) -> Option<Consequence> {
        self.inner.class
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
        self.list_tools_now()
    }

    fn call(&self, raw_name: &str, args: Value, cancel: CancelToken) -> BoxFuture<'_, Result<CallResult, McpError>> {
        self.call_with_progress(raw_name, args, cancel, None)
    }

    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<Resource>, McpError>> {
        let inner = self.inner.clone();
        self.inner.rt.run(async move {
            inner.require_resources()?;
            Ok(inner.paginate("resources/list", "resources").await?.iter().filter_map(wire::parse_resource).collect())
        })
    }

    fn list_resource_templates(&self) -> BoxFuture<'_, Result<Vec<ResourceTemplate>, McpError>> {
        let inner = self.inner.clone();
        self.inner.rt.run(async move {
            inner.require_resources()?;
            Ok(inner.paginate("resources/templates/list", "resourceTemplates").await?.iter().filter_map(wire::parse_template).collect())
        })
    }

    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ResourceContents>, McpError>> {
        let inner = self.inner.clone();
        let uri = uri.to_string();
        self.inner.rt.run(async move {
            inner.require_resources()?;
            let r = inner.request("resources/read", json!({"uri": uri}), None, None).await?;
            Ok(wire::parse_contents(&uri, &r))
        })
    }
}
