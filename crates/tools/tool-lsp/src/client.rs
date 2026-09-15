//! A JSON-RPC client over a world `Child`'s stdin/stdout.
//!
//! One reader task pumps the child's output through the framing decoder: responses complete
//! their waiting request, server→client requests are answered (this client is read-only, so
//! `workspace/applyEdit` is always refused), and notifications update the published
//! diagnostics and the indexing state `wait_ready` watches.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_cap::CancelToken;
use aho_proc::Child;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use crate::framing::{Decoder, FrameError, encode};

/// How often a waiting call re-checks cancellation.
const TICK: Duration = Duration::from_millis(50);
/// Without `experimental/serverStatus`, the server counts as idle once no progress is running
/// and nothing arrived for this long.
const IDLE_SETTLE: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, PartialEq)]
pub enum LspError {
    Timeout {
        method: String,
        ms: u64,
    },
    /// The server answered with a JSON-RPC error.
    Server {
        method: String,
        code: i64,
        message: String,
    },
    /// The server process is gone or its stream is unreadable.
    Exited(String),
    Io(String),
    Cancelled,
}

impl fmt::Display for LspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LspError::Timeout { method, ms } => write!(
                f,
                "the language server did not answer `{method}` within {ms} ms; the request was cancelled. Retry, or raise caps.tool-lsp.request_timeout_ms."
            ),
            LspError::Server { method, code, message } => write!(f, "the language server refused `{method}` (error {code}): {message}"),
            LspError::Exited(why) => f.write_str(why),
            LspError::Io(m) => f.write_str(m),
            LspError::Cancelled => f.write_str("the call was cancelled; its request to the language server was withdrawn."),
        }
    }
}

impl std::error::Error for LspError {}

/// The last `textDocument/publishDiagnostics` for one document.
#[derive(Clone, Debug)]
pub struct Published {
    pub version: Option<i64>,
    pub items: Vec<Value>,
    pub at: Instant,
}

type Reply = oneshot::Sender<Result<Value, (i64, String)>>;

struct State {
    pending: HashMap<i64, Reply>,
    diagnostics: HashMap<String, Published>,
    progress: BTreeSet<String>,
    /// `experimental/serverStatus.quiescent` once the server sent one (rust-analyzer does).
    quiescent: Option<bool>,
    last_activity: Instant,
    dead: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    /// Bumped on every message; waiters re-check their condition.
    tick: watch::Sender<u64>,
    /// Answers to `workspace/configuration`, keyed by section.
    settings: Value,
    /// Answer to `workspace/workspaceFolders`.
    folders: Value,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn bump(&self) {
        self.tick.send_modify(|n| *n = n.wrapping_add(1));
    }
    fn die(&self, why: String) {
        let pending = {
            let mut st = self.lock();
            if st.dead.is_none() {
                st.dead = Some(why.clone());
            }
            std::mem::take(&mut st.pending)
        };
        for (_, tx) in pending {
            // A dropped receiver is a request that already gave up.
            drop(tx.send(Err((-32099, why.clone()))));
        }
        self.bump();
    }
}

pub struct Client {
    child: Arc<dyn Child>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    reader: tokio::task::JoinHandle<()>,
    /// Name used in error messages (`rust-analyzer`).
    name: String,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Client {
    /// Starts reading `child`'s output. `settings` answers `workspace/configuration`.
    pub fn new(name: &str, child: Arc<dyn Child>, settings: Value, folders: Value) -> Client {
        let (tick, _) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: HashMap::new(),
                diagnostics: HashMap::new(),
                progress: BTreeSet::new(),
                quiescent: None,
                last_activity: Instant::now(),
                dead: None,
            }),
            tick,
            settings,
            folders,
        });
        let reader = tokio::spawn(read_loop(name.to_string(), child.clone(), shared.clone()));
        Client { child, shared, next_id: AtomicI64::new(1), reader, name: name.to_string() }
    }

    pub fn child(&self) -> &Arc<dyn Child> {
        &self.child
    }

    /// Why the server is unusable, once it is.
    pub fn dead(&self) -> Option<String> {
        self.shared.lock().dead.clone()
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        if let Some(why) = self.dead() {
            return Err(LspError::Exited(why));
        }
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    async fn write(&self, msg: &Value) -> Result<(), LspError> {
        self.child.write_stdin(encode(msg)).await.map_err(|e| {
            let why = format!("writing to {} failed ({e}); the server was abandoned.", self.name);
            self.shared.die(why.clone());
            LspError::Exited(why)
        })
    }

    /// One request; withdrawn with `$/cancelRequest` on timeout or cancellation.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &CancelToken) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = oneshot::channel();
        {
            let mut st = self.shared.lock();
            if let Some(why) = &st.dead {
                return Err(LspError::Exited(why.clone()));
            }
            st.pending.insert(id, tx);
        }
        if let Err(e) = self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await {
            self.shared.lock().pending.remove(&id);
            return Err(e);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let outcome = loop {
            tokio::select! {
                r = &mut rx => break Ok(r),
                _ = tokio::time::sleep(TICK) => {
                    if cancel.is_cancelled() {
                        break Err(LspError::Cancelled);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break Err(LspError::Timeout { method: method.to_string(), ms: timeout.as_millis() as u64 });
                    }
                }
            }
        };
        match outcome {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err((-32099, message)))) => Err(LspError::Exited(message)),
            Ok(Ok(Err((code, message)))) => Err(LspError::Server { method: method.to_string(), code, message }),
            Ok(Err(_)) => Err(LspError::Exited(self.dead().unwrap_or_else(|| format!("{} stopped answering.", self.name)))),
            Err(e) => {
                self.shared.lock().pending.remove(&id);
                if self.dead().is_none() {
                    self.notify("$/cancelRequest", json!({"id": id})).await?;
                }
                Err(e)
            }
        }
    }

    pub fn diagnostics(&self, uri: &str) -> Option<Published> {
        self.shared.lock().diagnostics.get(uri).cloned()
    }

    /// Idle: `serverStatus.quiescent` when the server reports it, else no running progress and
    /// nothing received for a moment.
    pub fn is_ready(&self) -> bool {
        let st = self.shared.lock();
        match st.quiescent {
            Some(q) => q,
            None => st.progress.is_empty() && st.last_activity.elapsed() >= IDLE_SETTLE,
        }
    }

    /// Waits until `is_ready`, the server dies, `cancel` fires, or `timeout` passes. `Ok(false)`
    /// means it was still busy at the deadline.
    pub async fn wait_ready(&self, timeout: Duration, cancel: &CancelToken) -> Result<bool, LspError> {
        self.wait_until(timeout, cancel, |c| c.is_ready()).await
    }

    /// Waits until `cond` holds, re-checking on every message and every tick.
    pub async fn wait_until(&self, timeout: Duration, cancel: &CancelToken, cond: impl Fn(&Client) -> bool) -> Result<bool, LspError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.shared.tick.subscribe();
        loop {
            if let Some(why) = self.dead() {
                return Err(LspError::Exited(why));
            }
            if cond(self) {
                return Ok(true);
            }
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return Ok(cond(self)) },
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

async fn read_loop(name: String, child: Arc<dyn Child>, shared: Arc<Shared>) {
    let mut decoder = Decoder::new();
    let mut cursor = 0u64;
    loop {
        let (bytes, next, done) = child.output(cursor);
        cursor = next;
        if !bytes.is_empty() {
            decoder.push(&bytes);
            loop {
                match decoder.next() {
                    Ok(Some(msg)) => handle(&child, &shared, msg).await,
                    Ok(None) => break,
                    // Framed correctly, just not JSON: skip it, the stream is still in sync.
                    Err(FrameError::BadJson(_)) => continue,
                    Err(e) => {
                        shared.die(format!("{name} broke the protocol: {e}; it was stopped, and the next lsp call starts it again."));
                        // Killing a server that already exited has nothing left to stop.
                        drop(child.kill().await);
                        return;
                    }
                }
            }
            shared.bump();
        }
        if done {
            let status = match child.wait().await {
                Ok(exit) => format!("{name} exited (code {}); the next lsp call starts it again.", exit.code),
                Err(e) => format!("{name} stopped ({e}); the next lsp call starts it again."),
            };
            shared.die(status);
            return;
        }
        child.readable(cursor).await;
    }
}

async fn handle(child: &Arc<dyn Child>, shared: &Shared, msg: Value) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned().filter(|v| !v.is_null());
    match (method, id) {
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let Some(tx) = shared.lock().pending.remove(&id) else { return };
            let reply = match msg.get("error") {
                Some(e) => Err((
                    e.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                    e.get("message").and_then(Value::as_str).unwrap_or("no message").to_string(),
                )),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            // The requester may have timed out already; then nobody wants this answer.
            drop(tx.send(reply));
        }
        (Some(m), Some(id)) => {
            let reply = match answer(shared, m, msg.get("params").unwrap_or(&Value::Null)) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            };
            if let Err(e) = child.write_stdin(encode(&reply)).await {
                shared.die(format!("answering the server's `{m}` request failed ({e})."));
            }
        }
        (Some(m), None) => notification(shared, m, msg.get("params").unwrap_or(&Value::Null)),
        (None, None) => {}
    }
}

/// Server→client requests. Everything that would change the workspace is refused.
fn answer(shared: &Shared, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "workspace/configuration" => {
            let items = params.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            Ok(Value::Array(
                items
                    .iter()
                    .map(|it| match it.get("section").and_then(Value::as_str) {
                        Some(section) => lookup(&shared.settings, section),
                        None => shared.settings.clone(),
                    })
                    .collect(),
            ))
        }
        "workspace/workspaceFolders" => Ok(shared.folders.clone()),
        "workspace/applyEdit" => Ok(json!({"applied": false, "failureReason": "aho's lsp tools are read-only; no edit was applied."})),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest"
        | "window/showDocument"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/diagnostic/refresh"
        | "workspace/foldingRange/refresh" => Ok(Value::Null),
        other => Err((-32601, format!("aho does not handle `{other}`"))),
    }
}

/// `a.b.c` in a nested settings table; `null` when absent.
fn lookup(settings: &Value, section: &str) -> Value {
    if let Some(v) = settings.get(section) {
        return v.clone();
    }
    section.split('.').try_fold(settings, |v, k| v.get(k)).cloned().unwrap_or(Value::Null)
}

fn notification(shared: &Shared, method: &str, params: &Value) {
    let mut st = shared.lock();
    st.last_activity = Instant::now();
    match method {
        "textDocument/publishDiagnostics" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else { return };
            let items = params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
            let version = params.get("version").and_then(Value::as_i64);
            st.diagnostics.insert(uri.to_string(), Published { version, items, at: Instant::now() });
        }
        "$/progress" => {
            let token = match params.get("token") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => return,
            };
            match params.pointer("/value/kind").and_then(Value::as_str) {
                Some("begin") => {
                    st.progress.insert(token);
                }
                Some("end") => {
                    st.progress.remove(&token);
                }
                _ => {}
            }
        }
        "experimental/serverStatus" => {
            st.quiescent = params.get("quiescent").and_then(Value::as_bool);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(settings: Value) -> Shared {
        let (tick, _) = watch::channel(0);
        Shared {
            state: Mutex::new(State {
                pending: HashMap::new(),
                diagnostics: HashMap::new(),
                progress: BTreeSet::new(),
                quiescent: None,
                last_activity: Instant::now(),
                dead: None,
            }),
            tick,
            settings,
            folders: json!([]),
        }
    }

    #[test]
    fn configuration_requests_are_answered_per_section_and_edits_are_refused() {
        let s = shared(json!({"rust-analyzer": {"checkOnSave": false}, "a": {"b": {"c": 1}}}));
        let r =
            answer(&s, "workspace/configuration", &json!({"items": [{"section": "rust-analyzer"}, {"section": "a.b"}, {"section": "zz"}]}));
        assert_eq!(r, Ok(json!([{"checkOnSave": false}, {"c": 1}, null])));
        let r = answer(&s, "workspace/applyEdit", &json!({"edit": {}})).unwrap();
        assert_eq!(r["applied"], json!(false));
        assert_eq!(answer(&s, "window/workDoneProgress/create", &json!({})), Ok(Value::Null));
        assert!(matches!(answer(&s, "custom/unknown", &json!({})), Err((-32601, _))));
    }

    #[test]
    fn progress_and_server_status_drive_readiness_and_diagnostics_are_kept_per_document() {
        let s = shared(Value::Null);
        notification(&s, "$/progress", &json!({"token": "index", "value": {"kind": "begin"}}));
        notification(&s, "$/progress", &json!({"token": 7, "value": {"kind": "begin"}}));
        assert_eq!(s.lock().progress.len(), 2);
        notification(&s, "$/progress", &json!({"token": "index", "value": {"kind": "end"}}));
        notification(&s, "$/progress", &json!({"token": 7, "value": {"kind": "end"}}));
        assert!(s.lock().progress.is_empty());
        notification(&s, "experimental/serverStatus", &json!({"health": "ok", "quiescent": false}));
        assert_eq!(s.lock().quiescent, Some(false));
        notification(
            &s,
            "textDocument/publishDiagnostics",
            &json!({"uri": "file:///a.rs", "version": 3, "diagnostics": [{"message": "x"}]}),
        );
        let st = s.lock();
        assert_eq!(st.diagnostics["file:///a.rs"].version, Some(3));
        assert_eq!(st.diagnostics["file:///a.rs"].items.len(), 1);
    }
}
