//! JSON-RPC 2.0 over the bound world's separate stdin/stdout/stderr protocol transport.
//!
//! One runtime thread owns the world process and its lossless streams. Blocking requests route
//! replies by id and enforce deadlines/cancellation; async methods use tokio's blocking pool.
//! Dropping the last handle closes stdin, allows a short exit grace, then kills the world process
//! group and joins its runtime. There is no host process fallback.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aho_cap::CancelToken;
use aho_mcp::*;
use aho_proc::{SpawnSpec, SubprocessHandle, TransportChild};
use aho_types::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Stderr kept for error messages.
const STDERR_TAIL: usize = 4096;
/// Interval at which a blocked request checks its cancellation token.
const POLL: Duration = Duration::from_millis(20);
/// Upper bound on pages for one paginated list (a looping cursor is a server bug).
const MAX_PAGES: usize = 1000;
/// Grace between closing stdin and killing the process on drop.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

type Reply = Result<Value, (i64, String)>;

/// State shared with the reader thread.
struct Conn {
    writer: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    stderr: Mutex<VecDeque<u8>>,
    closed: AtomicBool,
    tools_changed: AtomicBool,
}

impl Conn {
    fn send(&self, msg: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(msg).map_err(|e| McpError::Transport(format!("could not encode a request: {e}")))?;
        line.push('\n');
        let w = lock(&self.writer);
        let Some(writer) = w.as_ref() else { return Err(McpError::Closed(self.stderr_tail())) };
        writer.send(line.into_bytes()).map_err(|_| McpError::Closed(self.stderr_tail()))
    }

    fn stderr_tail(&self) -> String {
        let buf = lock(&self.stderr);
        let (a, b) = buf.as_slices();
        let mut bytes = a.to_vec();
        bytes.extend_from_slice(b);
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let start = lines.len().saturating_sub(3);
        lines[start..].join(" | ")
    }

    fn on_message(&self, msg: Value) {
        if let Value::Array(batch) = msg {
            batch.into_iter().for_each(|m| self.on_message(m));
            return;
        }
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("aho does not offer `{method}` to MCP servers")}})
                };
                let _ = self.send(&reply);
            }
            (Some(method), None) => {
                if method == "notifications/tools/list_changed" {
                    self.tools_changed.store(true, Ordering::SeqCst);
                }
            }
            (None, Some(id)) => {
                let Some(n) = id.as_u64() else { return };
                let Some(tx) = lock(&self.pending).remove(&n) else { return };
                let reply = match msg.get("error") {
                    Some(e) => Err((
                        e.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        e.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
                    )),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
            }
            (None, None) => {}
        }
    }
}

/// The `initialize` answer.
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    pub protocol_version: String,
    pub name: String,
    pub version: String,
    pub instructions: Option<String>,
    pub tools: bool,
    pub resources: bool,
}

struct Inner {
    name: String,
    class: Option<Consequence>,
    conn: Arc<Conn>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    next_id: AtomicU64,
    timeout: Duration,
    info: ServerInfo,
}

impl Drop for Inner {
    fn drop(&mut self) {
        lock(&self.conn.writer).take();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A running stdio MCP server. Cheap to clone; the process stops when the last clone drops.
#[derive(Clone)]
pub struct StdioServer {
    inner: Arc<Inner>,
}

impl StdioServer {
    /// Spawn in the bound world, `initialize`, then `notifications/initialized`.
    /// The deadline covers process startup as well as the protocol handshake.
    pub fn start(proc: &SubprocessHandle, spec: &ServerSpec, handshake_timeout: Duration) -> Result<StdioServer, McpError> {
        let deadline = Instant::now() + handshake_timeout;
        let (writer, writes) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let conn = Arc::new(Conn {
            writer: Mutex::new(Some(writer)),
            pending: Mutex::new(HashMap::new()),
            stderr: Mutex::new(VecDeque::with_capacity(STDERR_TAIL)),
            closed: AtomicBool::new(false),
            tools_changed: AtomicBool::new(false),
        });
        let (world, server, thread_conn) = (proc.clone(), spec.clone(), conn.clone());
        let worker = std::thread::Builder::new()
            .name(format!("mcp-{}", spec.name))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = ready.send(Err(McpError::Spawn(format!("could not start the MCP transport runtime: {e}"))));
                        return;
                    }
                };
                runtime.block_on(run_transport(world, server, thread_conn, writes, stop, ready));
            })
            .map_err(|e| thread_error(&spec.name, e))?;
        let mut inner = Inner {
            name: spec.name.clone(),
            class: spec.class,
            conn,
            shutdown: Some(shutdown),
            worker: Some(worker),
            next_id: AtomicU64::new(1),
            timeout: Duration::from_millis(spec.timeout_ms.max(1)),
            info: ServerInfo::default(),
        };
        // `inner` now owns shutdown, including failures during spawn or initialize.
        started.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|_| {
            McpError::Handshake(format!("MCP server `{}` did not start before its handshake deadline; it was stopped.", spec.name))
        })??;
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "aho", "version": env!("CARGO_PKG_VERSION")}
        });
        let init = inner
            .request_with("initialize", params, deadline.saturating_duration_since(Instant::now()), None)
            .map_err(|e| McpError::Handshake(format!("MCP server `{}` did not complete `initialize`: {e}. It was stopped.", spec.name)))?;
        let version = init.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(McpError::Handshake(format!(
                "MCP server `{}` speaks protocol `{version}`, which aho does not support ({}); it was stopped.",
                spec.name,
                SUPPORTED_VERSIONS.join(", ")
            )));
        }
        let caps = init.get("capabilities").cloned().unwrap_or(Value::Null);
        let info = init.get("serverInfo").cloned().unwrap_or(Value::Null);
        inner.info = ServerInfo {
            protocol_version: version.to_string(),
            name: info.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            version: info.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
            instructions: init.get("instructions").and_then(Value::as_str).map(str::to_string),
            tools: caps.get("tools").is_some(),
            resources: caps.get("resources").is_some(),
        };
        inner.conn.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(StdioServer { inner: Arc::new(inner) })
    }

    pub fn info(&self) -> &ServerInfo {
        &self.inner.info
    }

    /// The server sent `notifications/tools/list_changed` since start.
    pub fn tools_changed(&self) -> bool {
        self.inner.conn.tools_changed.load(Ordering::SeqCst)
    }

    pub fn list_tools_blocking(&self) -> Result<Vec<RemoteTool>, McpError> {
        if !self.inner.info.tools {
            return Ok(Vec::new());
        }
        let pages = self.inner.paginate("tools/list", "tools")?;
        pages.into_iter().map(|t| parse_tool(&self.inner.name, t)).collect()
    }

    pub fn call_blocking(&self, raw: &str, args: Value, cancel: Option<CancelToken>) -> Result<CallResult, McpError> {
        let arguments = if args.is_null() { json!({}) } else { args };
        let r = self.inner.request_with("tools/call", json!({"name": raw, "arguments": arguments}), self.inner.timeout, cancel.as_ref())?;
        Ok(CallResult {
            content: r.get("content").and_then(Value::as_array).cloned().unwrap_or_default(),
            structured: r.get("structuredContent").cloned().filter(|v| !v.is_null()),
            is_error: r.get("isError").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn require_resources(&self) -> Result<(), McpError> {
        if self.inner.info.resources {
            Ok(())
        } else {
            Err(McpError::Unsupported(format!("MCP server `{}` does not offer resources", self.inner.name)))
        }
    }

    pub fn list_resources_blocking(&self) -> Result<Vec<Resource>, McpError> {
        self.require_resources()?;
        Ok(self
            .inner
            .paginate("resources/list", "resources")?
            .into_iter()
            .filter_map(|r| {
                Some(Resource {
                    uri: r.get("uri")?.as_str()?.to_string(),
                    name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    title: opt_str(&r, "title"),
                    description: opt_str(&r, "description"),
                    mime: opt_str(&r, "mimeType"),
                })
            })
            .collect())
    }

    pub fn list_resource_templates_blocking(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        self.require_resources()?;
        Ok(self
            .inner
            .paginate("resources/templates/list", "resourceTemplates")?
            .into_iter()
            .filter_map(|r| {
                Some(ResourceTemplate {
                    uri_template: r.get("uriTemplate")?.as_str()?.to_string(),
                    name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    title: opt_str(&r, "title"),
                    description: opt_str(&r, "description"),
                    mime: opt_str(&r, "mimeType"),
                })
            })
            .collect())
    }

    pub fn read_resource_blocking(&self, uri: &str) -> Result<Vec<ResourceContents>, McpError> {
        self.require_resources()?;
        let r = self.inner.request_with("resources/read", json!({"uri": uri}), self.inner.timeout, None)?;
        let contents = r.get("contents").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(contents
            .into_iter()
            .map(|c| {
                let uri = c.get("uri").and_then(Value::as_str).unwrap_or(uri).to_string();
                let mime = opt_str(&c, "mimeType");
                match (c.get("text").and_then(Value::as_str), c.get("blob").and_then(Value::as_str)) {
                    (Some(t), _) => ResourceContents::Text { uri, mime, text: t.to_string() },
                    (None, Some(b)) => ResourceContents::Blob { uri, mime, base64: b.to_string() },
                    (None, None) => ResourceContents::Text { uri, mime, text: String::new() },
                }
            })
            .collect())
    }
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn parse_tool(server: &str, t: Value) -> Result<RemoteTool, McpError> {
    let Some(name) = t.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else {
        return Err(McpError::Transport(format!("MCP server `{server}` listed a tool without a name; its tool list was not used")));
    };
    let mut schema = match t.get("inputSchema") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({"type": "object"}),
    };
    if schema.get("type").is_none() {
        schema["type"] = json!("object");
    }
    Ok(RemoteTool {
        raw_name: name.to_string(),
        title: opt_str(&t, "title").or_else(|| t.get("annotations").and_then(|a| opt_str(a, "title"))),
        description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        schema: RootSchema(schema),
    })
}

impl Inner {
    fn request_with(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&CancelToken>) -> Result<Value, McpError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(McpError::Cancelled);
        }
        if self.conn.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed(self.conn.stderr_tail()));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.conn.pending).insert(id, tx);
        if let Err(e) = self.conn.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            lock(&self.conn.pending).remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            match rx.recv_timeout(POLL.min(deadline.saturating_duration_since(now))) {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err((code, message))) => return Err(McpError::Rpc { code, message }),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(McpError::Closed(self.conn.stderr_tail())),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let cancelled = cancel.map(CancelToken::is_cancelled).unwrap_or(false);
                    if cancelled || Instant::now() >= deadline {
                        lock(&self.conn.pending).remove(&id);
                        let reason = if cancelled { "cancelled by aho" } else { "timed out" };
                        let _ = self.conn.send(&json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/cancelled",
                            "params": {"requestId": id, "reason": reason}
                        }));
                        return Err(if cancelled {
                            McpError::Cancelled
                        } else {
                            McpError::Timeout { method: method.to_string(), ms: timeout.as_millis() as u64 }
                        });
                    }
                }
            }
        }
    }

    /// Every page of a cursor-paginated list; `key` names the array in each result.
    fn paginate(&self, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request_with(method, params, self.timeout, None)?;
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
}

fn thread_error(server: &str, e: std::io::Error) -> McpError {
    McpError::Spawn(format!("could not start the I/O thread for MCP server `{server}` ({e}); it was stopped"))
}

async fn run_transport(
    world: SubprocessHandle,
    spec: ServerSpec,
    conn: Arc<Conn>,
    mut writes: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    ready: mpsc::SyncSender<Result<(), McpError>>,
) {
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(spec.cmd.clone());
    argv.extend(spec.args);
    let spawn = SpawnSpec { argv, env: spec.env, ..SpawnSpec::default() };
    let transport = tokio::select! {
        _ = &mut stop => return,
        result = world.0.spawn_transport(spawn) => result,
    };
    let TransportChild { child, mut stdin, stdout, mut stderr } = match transport {
        Ok(transport) => transport,
        Err(e) => {
            let _ = ready.send(Err(McpError::Spawn(format!(
                "could not start `{}` for MCP server `{}` in the bound world ({e}); no host fallback was attempted.",
                spec.cmd, spec.name
            ))));
            return;
        }
    };
    let output_conn = conn.clone();
    let mut reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(text) {
                        Ok(msg) => output_conn.on_message(msg),
                        Err(_) => append_stderr(&output_conn, format!("stdout: {text}\n").as_bytes()),
                    }
                }
            }
        }
    });
    let error_conn = conn.clone();
    let mut errors = tokio::spawn(async move {
        let mut bytes = [0u8; 1024];
        loop {
            match stderr.read(&mut bytes).await {
                Ok(0) | Err(_) => return,
                Ok(n) => append_stderr(&error_conn, &bytes[..n]),
            }
        }
    });
    let mut writer = tokio::spawn(async move {
        while let Some(bytes) = writes.recv().await {
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let _ = ready.send(Ok(()));
    let mut reader_done = false;
    tokio::select! {
        _ = &mut stop => {},
        _ = &mut reader => { reader_done = true; },
        _ = &mut writer => {},
        _ = child.wait() => {},
    }
    // Stop the writer even when it is backpressured, dropping stdin to signal EOF.
    writer.abort();
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
    // Also kill after the leader exits: its descendants may still own pipes.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.kill()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    if !reader_done && tokio::time::timeout(SHUTDOWN_GRACE, &mut reader).await.is_err() {
        reader.abort();
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, &mut errors).await.is_err() {
        errors.abort();
    }
    conn.closed.store(true, Ordering::SeqCst);
    lock(&conn.pending).clear();
    lock(&conn.writer).take();
}

fn append_stderr(conn: &Conn, bytes: &[u8]) {
    let mut buf = lock(&conn.stderr);
    buf.extend(bytes);
    while buf.len() > STDERR_TAIL {
        buf.pop_front();
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, McpError> + Send + 'static) -> Result<T, McpError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| McpError::Transport(format!("the MCP worker thread failed ({e})")))?
}

impl McpServer for StdioServer {
    fn name(&self) -> &str {
        &self.inner.name
    }
    fn class(&self) -> Option<Consequence> {
        self.inner.class
    }
    fn instructions(&self) -> Option<&str> {
        self.inner.info.instructions.as_deref()
    }
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
        let s = self.clone();
        Box::pin(blocking(move || s.list_tools_blocking()))
    }
    fn call(&self, raw_name: &str, args: Value, cancel: CancelToken) -> BoxFuture<'_, Result<CallResult, McpError>> {
        let (s, raw) = (self.clone(), raw_name.to_string());
        Box::pin(blocking(move || s.call_blocking(&raw, args, Some(cancel))))
    }
    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<Resource>, McpError>> {
        let s = self.clone();
        Box::pin(blocking(move || s.list_resources_blocking()))
    }
    fn list_resource_templates(&self) -> BoxFuture<'_, Result<Vec<ResourceTemplate>, McpError>> {
        let s = self.clone();
        Box::pin(blocking(move || s.list_resource_templates_blocking()))
    }
    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ResourceContents>, McpError>> {
        let (s, uri) = (self.clone(), uri.to_string());
        Box::pin(blocking(move || s.read_resource_blocking(&uri)))
    }
}
