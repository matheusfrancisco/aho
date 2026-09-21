//! `08`/`12` — the rpc surface: NDJSON over stdio or a unix socket, every `Session` call 1:1,
//! plus the `caps.*` verb set of 05. The SDK protocol and the default daemon topology.
//!
//! Envelope: every frame carries `v`. The first client frame is `hello{min,max,…}`; the server
//! answers `hello_ok{version, capabilities}` with the highest common version or an `error` and
//! closes. Unknown fields are ignored. Requests carry `id`; the reply is a `result` or `error`
//! frame with `reply_to = id`. Events (`entry`, `stream`, `idle`, `approval`, `error`) carry
//! `reply_to: null`. Authentication is process ownership: stdio, or a socket created `0600`.
//!
//! Takeover (12) is decided from the hello and logged as `SurfaceAttached`: allowed when the
//! session has no owner, the client is the owner, or it has local history and asks to take over.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod caps;
pub mod wire;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aho_approval::{ApprovalRequest, Decided};
use aho_cap::{BuildError, Capability, CapabilityId, Kind, Manifest, Registrar};
use aho_log::{EntryKind, Takeover};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Session, SessionEvent};
use aho_types::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

pub use caps::{CapsContext, CapsError};
pub use wire::{ClientFrame, Hello, HelloOk, METHODS, ServerFrame, VERSION_MAX, VERSION_MIN, negotiate};

// ───────────────────────────── capability ─────────────────────────────

pub struct SurfaceRpc;

impl Capability for SurfaceRpc {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SurfaceRpc)
    }
    fn register(&self, _r: &mut Registrar<'_>) {}
}

aho_cap::declare_capability!(
    SurfaceRpc,
    "surface-rpc",
    kind = Kind::Surface,
    provides = [],
    requires = [],
    tools = &[],
    doc = "NDJSON rpc over stdio or a unix socket: versioned envelope, Session API 1:1, and the caps.* verb set (08, 12)."
);

// ───────────────────────────── errors ─────────────────────────────

#[derive(Debug)]
pub enum RpcError {
    Io(std::io::Error),
    /// The hello was missing, malformed, or shared no version with the server.
    Handshake(String),
    /// Another client owns the session and this one may not take it over.
    Refused(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Io(e) => write!(f, "rpc connection failed: {e}"),
            RpcError::Handshake(m) => write!(f, "rpc handshake failed: {m}"),
            RpcError::Refused(m) => write!(f, "rpc attach refused: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

impl From<std::io::Error> for RpcError {
    fn from(e: std::io::Error) -> Self {
        RpcError::Io(e)
    }
}

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ───────────────────────────── approvals ─────────────────────────────

struct PendingCard {
    payload: Value,
    reply: oneshot::Sender<Decided>,
}

/// Numbers the session's approval cards, fans them out to every attached client, and routes the
/// one `approval.answer` back. A card nobody answers keeps the call waiting; a client that
/// disconnects does not answer for anyone.
pub struct ApprovalBroker {
    next: AtomicU64,
    pending: Mutex<BTreeMap<u64, PendingCard>>,
    cards: broadcast::Sender<Value>,
}

impl ApprovalBroker {
    fn new() -> Arc<ApprovalBroker> {
        let (cards, _) = broadcast::channel(64);
        Arc::new(ApprovalBroker { next: AtomicU64::new(1), pending: Mutex::new(BTreeMap::new()), cards })
    }

    fn pump(self: &Arc<Self>, mut rx: mpsc::Receiver<ApprovalRequest>, session: Arc<Session>) {
        let me = self.clone();
        tokio::spawn(async move {
            while let Some((card, reply)) = rx.recv().await {
                let id = me.next.fetch_add(1, Ordering::Relaxed);
                let reg = session.registry();
                let cap = reg
                    .tools
                    .iter()
                    .find(|(_, d, _)| d.name == card.tool)
                    .map(|(c, _, _)| c.name)
                    .or_else(|| reg.dynamic_tools.iter().find(|(_, s, _)| s.name == card.tool).map(|(c, _, _)| c.name));
                let payload = wire::approval_payload(id, &card, cap, session.policy_view().epoch.0 + 1);
                lk(&me.pending).insert(id, PendingCard { payload: payload.clone(), reply });
                // No attached client is fine: the card stays pending and is sent on attach.
                if me.cards.send(payload).is_err() {
                    continue;
                }
            }
        });
    }

    pub fn pending(&self) -> Vec<Value> {
        let mut p = lk(&self.pending);
        p.retain(|_, c| !c.reply.is_closed());
        p.values().map(|c| c.payload.clone()).collect()
    }

    /// The `preset_cut` of a waiting card, for the `preset` outcome.
    pub fn preset_cut(&self, id: u64) -> Result<String, String> {
        let p = lk(&self.pending);
        let card = p.get(&id).ok_or_else(|| format!("no approval is waiting with id {id}; it was answered or never asked"))?;
        card.payload["preset_cut"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("approval {id} names no preset that would allow it; answer allow, reject, or cancel"))
    }

    pub fn answer(&self, id: u64, d: Decided) -> Result<(), String> {
        let card =
            lk(&self.pending).remove(&id).ok_or_else(|| format!("no approval is waiting with id {id}; it was answered or never asked"))?;
        card.reply.send(d).map_err(|_| format!("approval {id} was already given up by the call that asked (turn aborted)"))
    }

    fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.cards.subscribe()
    }
}

// ───────────────────────────── server context ─────────────────────────────

/// Shared by every connection to one session.
pub struct RpcContext {
    caps: Option<CapsContext>,
    approvals: Arc<ApprovalBroker>,
    owner: Mutex<Option<String>>,
}

impl RpcContext {
    /// `approvals` is the receiver of the `ChannelApproval` the session was booted with (absent
    /// when prompts are off); `caps` enables the `caps.*` verbs. Must run inside a tokio runtime.
    pub fn new(session: &Arc<Session>, approvals: Option<mpsc::Receiver<ApprovalRequest>>, caps: Option<CapsContext>) -> Arc<RpcContext> {
        let broker = ApprovalBroker::new();
        if let Some(rx) = approvals {
            broker.pump(rx, session.clone());
        }
        Arc::new(RpcContext { caps, approvals: broker, owner: Mutex::new(None) })
    }

    pub fn approvals(&self) -> &ApprovalBroker {
        &self.approvals
    }

    fn attach(&self, h: &Hello) -> Takeover {
        let mut owner = lk(&self.owner);
        match owner.as_deref() {
            None => {
                *owner = Some(h.client_instance.clone());
                Takeover::Allowed
            }
            Some(o) if o == h.client_instance => Takeover::Allowed,
            Some(_) if h.allow_takeover && h.has_local_history => {
                *owner = Some(h.client_instance.clone());
                Takeover::Allowed
            }
            Some(o) => Takeover::Refused(format!(
                "client `{o}` owns this session; reconnect with allow_takeover and local history to take it over"
            )),
        }
    }

    fn detach(&self, instance: &str) {
        let mut owner = lk(&self.owner);
        if owner.as_deref() == Some(instance) {
            *owner = None;
        }
    }
}

// ───────────────────────────── connection ─────────────────────────────

async fn send<W: AsyncWrite + Unpin>(w: &mut W, f: &ServerFrame) -> std::io::Result<()> {
    let mut line = serde_json::to_string(f).map_err(std::io::Error::other)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn next_event(rx: &mut Option<broadcast::Receiver<SessionEvent>>) -> Result<SessionEvent, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_card(rx: &mut Option<broadcast::Receiver<Value>>) -> Result<Value, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

struct Conn {
    v: u16,
    session: Arc<Session>,
    ctx: Arc<RpcContext>,
    events: Option<broadcast::Receiver<SessionEvent>>,
    skip_upto: Option<EntryId>,
    /// In-flight long requests; each yields its reply frame (`None` for a notification).
    tasks: JoinSet<Option<ServerFrame>>,
}

fn params<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, String> {
    let v = if v.is_null() { json!({}) } else { v.clone() };
    serde_json::from_value(v).map_err(|e| format!("bad params: {e}"))
}

impl Conn {
    /// Maps a live session event to a frame; `None` when it duplicates a replayed entry.
    fn frame_of(&self, ev: &SessionEvent) -> Option<ServerFrame> {
        Some(match ev {
            SessionEvent::Entry(e) => {
                if self.skip_upto.is_some_and(|s| e.id <= s) {
                    return None;
                }
                ServerFrame::entry(self.v, e)
            }
            SessionEvent::Stream(s) => ServerFrame::new(self.v, None, "stream", wire::stream_payload(s)),
            SessionEvent::Idle { turn, transition } => ServerFrame::new(self.v, None, "idle", wire::idle_payload(*turn, transition)),
            SessionEvent::Error(m) => ServerFrame::error(self.v, None, m.clone()),
        })
    }

    fn reply(&self, id: Option<u64>, r: Result<Value, String>) -> Option<ServerFrame> {
        match (id, r) {
            (None, Ok(_)) => None,
            (id, Ok(v)) => Some(ServerFrame::result(self.v, id, v)),
            (id, Err(m)) => Some(ServerFrame::error(self.v, id, m)),
        }
    }

    /// `[p]` on the card (05): the smallest preset that admits the call becomes `policy.preset`
    /// through the same plan → apply as `aho preset`, and this call is allowed for the session.
    fn answer_with_preset(&self, id: u64) -> Result<Value, String> {
        let caps =
            self.ctx.caps.as_ref().ok_or("the preset answer needs the caps.* verbs; this server was started without a composition")?;
        let name = self.ctx.approvals.preset_cut(id)?;
        let inputs = caps.fresh_inputs().map_err(|e| e.to_string())?;
        let prop = caps::propose(&caps.composition, &inputs, vec![("policy.preset".into(), Value::String(name.clone()))])
            .map_err(|e| e.to_string())?;
        let wrote = prop.commit_approval_preset(&self.session).map_err(|e| e.to_string())?;
        let operator = Operator::authenticate(OperatorProof::local_tty());
        self.session.refresh_operator_policy(&operator).map_err(|e| e.to_string())?;
        self.ctx.approvals.answer(id, Decided::allowed(aho_log::GrantScope::Session))?;
        Ok(json!({ "preset": name, "wrote": wrote }))
    }

    /// Handles one request line. Long calls (prompt, rewind) run as tasks whose frame the loop sends.
    async fn handle<W: AsyncWrite + Unpin>(&mut self, line: &str, w: &mut W) -> std::io::Result<()> {
        let frame: ClientFrame = match serde_json::from_str(line) {
            Ok(f) => f,
            Err(e) => return send(w, &ServerFrame::error(self.v, None, format!("not a frame ({e}); send one JSON object per line"))).await,
        };
        let id = frame.id;
        if frame.v != self.v {
            let msg = format!("this frame says v{} but the connection negotiated v{}", frame.v, self.v);
            return send(w, &ServerFrame::error(self.v, id, msg)).await;
        }
        let result: Result<Value, String> = match frame.method.as_str() {
            "session.prompt" => match params::<wire::PromptParams>(&frame.params) {
                Ok(p) => {
                    let (s, v) = (self.session.clone(), self.v);
                    self.tasks.spawn(async move {
                        match s.prompt(p.text, p.client_key).await {
                            Ok(t) => id.map(|_| ServerFrame::result(v, id, json!({ "transition": t }))),
                            Err(e) => Some(ServerFrame::error(v, id, e.to_string())),
                        }
                    });
                    return Ok(());
                }
                Err(e) => Err(e),
            },
            "session.steer" => params::<wire::SteerParams>(&frame.params).map(|p| {
                self.session.steer(p.text, p.urgent);
                json!({})
            }),
            "session.abort" => {
                self.session.abort();
                Ok(json!({}))
            }
            "session.subscribe" => match params::<wire::SubscribeParams>(&frame.params) {
                Ok(p) => {
                    let rx = self.session.subscribe();
                    let replay = self.session.subscribe_from(p.from);
                    for e in &replay {
                        send(w, &ServerFrame::entry(self.v, e)).await?;
                    }
                    self.skip_upto = replay.last().map(|e| e.id).or(p.from);
                    self.events = Some(rx);
                    Ok(json!({ "replayed": replay.len(), "head": self.session.log().head() }))
                }
                Err(e) => Err(e),
            },
            "session.fold" => self.session.fold().map(|s| wire::state_payload(&s)).map_err(|e| e.to_string()),
            "session.rewind" => {
                match params::<wire::RewindParams>(&frame.params).and_then(|p| wire::rewind_mode(p.mode.as_deref()).map(|m| (p, m))) {
                    Ok((p, mode)) => {
                        let (s, v) = (self.session.clone(), self.v);
                        self.tasks.spawn(async move {
                            // Process ownership is this surface's authentication (08).
                            let op = Operator::authenticate(OperatorProof::local_tty());
                            match s.rewind(&op, CheckpointId(p.checkpoint), mode).await {
                                Ok(e) => id.map(|_| ServerFrame::result(v, id, json!({ "entry": e }))),
                                Err(e) => Some(ServerFrame::error(v, id, e.to_string())),
                            }
                        });
                        return Ok(());
                    }
                    Err(e) => Err(e),
                }
            }
            "approval.answer" => match params::<wire::AnswerParams>(&frame.params) {
                Ok(p) if p.outcome == "preset" => self.answer_with_preset(p.id),
                Ok(p) => wire::decided(&p.outcome, p.scope.as_deref()).and_then(|d| self.ctx.approvals.answer(p.id, d)).map(|_| json!({})),
                Err(e) => Err(e),
            },
            "hello" => Err("hello was already negotiated on this connection".into()),
            m if m.starts_with("caps.") => match &self.ctx.caps {
                None => Err("the caps.* verbs are not available: this server was started without a composition".into()),
                Some(c) => {
                    let mutation = if caps::is_apply(m, &frame.params) {
                        self.session.try_mutation().map(Some).map_err(|e| e.to_string())
                    } else {
                        Ok(None)
                    };
                    mutation.and_then(|_guard| caps::call_in_session(c, &self.session, m, &frame.params).map_err(|e| e.to_string()))
                }
            },
            other => Err(format!("unknown method `{other}`; hello_ok.capabilities lists what this server speaks")),
        };
        match self.reply(id, result) {
            Some(f) => send(w, &f).await,
            None => Ok(()),
        }
    }
}

/// Serves one client over a duplex stream (a socket connection, an in-memory pipe).
pub async fn serve<S>(stream: S, session: Arc<Session>, ctx: Arc<RpcContext>) -> Result<(), RpcError>
where
    S: AsyncRead + AsyncWrite,
{
    let (r, w) = tokio::io::split(stream);
    serve_io(r, w, session, ctx).await
}

/// Serves one client over a reader/writer pair (stdio). Returns when the client closes its side
/// and every request it made has been answered.
pub async fn serve_io<R, W>(reader: R, mut writer: W, session: Arc<Session>, ctx: Arc<RpcContext>) -> Result<(), RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let first = loop {
        match lines.next_line().await? {
            None => return Ok(()),
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
        }
    };
    let (id, hello) = match serde_json::from_str::<ClientFrame>(&first) {
        Ok(f) if f.method == "hello" => match serde_json::from_value::<Hello>(f.params) {
            Ok(h) => (f.id, h),
            Err(e) => return refuse(&mut writer, f.id, RpcError::Handshake(format!("bad hello params: {e}"))).await,
        },
        Ok(f) => {
            return refuse(&mut writer, f.id, RpcError::Handshake(format!("the first frame must be `hello`, got `{}`", f.method))).await;
        }
        Err(e) => return refuse(&mut writer, None, RpcError::Handshake(format!("the first line is not a frame: {e}"))).await,
    };
    let v = match negotiate(&hello) {
        Ok(v) => v,
        Err(m) => return refuse(&mut writer, id, RpcError::Handshake(m)).await,
    };
    let takeover = ctx.attach(&hello);
    session
        .log()
        .append(
            None,
            Principal::system("surface-rpc"),
            EntryKind::SurfaceAttached {
                client_instance: hello.client_instance.clone(),
                has_local_history: hello.has_local_history,
                takeover: takeover.clone(),
            },
        )
        .map_err(|e| RpcError::Io(std::io::Error::other(e.to_string())))?;
    if let Takeover::Refused(why) = takeover {
        return refuse(&mut writer, id, RpcError::Refused(why)).await;
    }
    let ok = HelloOk {
        version: v,
        capabilities: METHODS.iter().map(|m| m.to_string()).collect(),
        session: format!("{:x}", session.id.0),
        head: session.log().head(),
    };
    let ok = serde_json::to_value(&ok).map_err(|e| RpcError::Io(std::io::Error::other(e)))?;
    send(&mut writer, &ServerFrame::new(v, id, "hello_ok", ok)).await?;

    let mut conn = Conn { v, session, ctx: ctx.clone(), events: None, skip_upto: None, tasks: JoinSet::new() };
    let mut cards = Some(ctx.approvals.subscribe());
    for card in ctx.approvals.pending() {
        send(&mut writer, &ServerFrame::new(v, None, "approval", card)).await?;
    }
    let result = run(&mut conn, &mut lines, &mut writer, &mut cards).await;
    ctx.detach(&hello.client_instance);
    result
}

async fn run<R, W>(
    conn: &mut Conn,
    lines: &mut tokio::io::Lines<BufReader<R>>,
    writer: &mut W,
    cards: &mut Option<broadcast::Receiver<Value>>,
) -> Result<(), RpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reading = true;
    loop {
        if !reading && conn.tasks.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            ev = next_event(&mut conn.events) => match ev {
                Ok(ev) => if let Some(f) = conn.frame_of(&ev) { send(writer, &f).await? },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let msg = format!("this client fell {n} events behind; resubscribe from the last entry id it saw");
                    send(writer, &ServerFrame::error(conn.v, None, msg)).await?;
                }
                Err(broadcast::error::RecvError::Closed) => conn.events = None,
            },
            card = next_card(cards) => match card {
                Ok(c) => send(writer, &ServerFrame::new(conn.v, None, "approval", c)).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for c in conn.ctx.approvals.pending() {
                        send(writer, &ServerFrame::new(conn.v, None, "approval", c)).await?;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => *cards = None,
            },
            line = lines.next_line(), if reading => match line {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => conn.handle(&l, writer).await?,
                Ok(None) | Err(_) => reading = false,
            },
            Some(done) = conn.tasks.join_next(), if !conn.tasks.is_empty() => match done {
                Ok(Some(f)) => send(writer, &f).await?,
                Ok(None) => {}
                Err(e) => send(writer, &ServerFrame::error(conn.v, None, format!("a request failed inside the server: {e}"))).await?,
            },
        }
    }
    // Events a finished request emitted are already queued; flush them.
    while let Some(Ok(ev)) = conn.events.as_mut().map(|r| r.try_recv()) {
        if let Some(f) = conn.frame_of(&ev) {
            send(writer, &f).await?;
        }
    }
    Ok(())
}

async fn refuse<W: AsyncWrite + Unpin>(w: &mut W, id: Option<u64>, e: RpcError) -> Result<(), RpcError> {
    let msg = match &e {
        RpcError::Handshake(m) | RpcError::Refused(m) => m.clone(),
        RpcError::Io(err) => err.to_string(),
    };
    send(w, &ServerFrame::error(VERSION_MAX, id, msg)).await?;
    Err(e)
}

/// Listens on a unix socket (mode `0600`) and serves every connection against `session` until
/// the task is dropped. Refuses to replace a socket another server still answers on.
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: rpc-unix-listener")]
pub async fn serve_unix(path: &Path, session: Arc<Session>, ctx: Arc<RpcContext>) -> Result<(), RpcError> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(RpcError::Io(std::io::Error::other(format!(
                "another server is listening on {}; stop it or pick another --socket",
                path.display()
            ))));
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (s, c) = (session.clone(), ctx.clone());
        tokio::spawn(async move {
            if let Err(e) = serve(stream, s, c).await {
                eprintln!("{e}");
            }
        });
    }
}

#[cfg(test)]
mod tests;
