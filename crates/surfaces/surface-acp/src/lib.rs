//! The acp surface: aho as an ACP agent over stdio (`subagent-acp` is the client side). Speaks
//! ACP protocol version 1, types from schema release 1.23.0 (see `aho-acp`).
//!
//! * One ACP session is one aho [`Session`]: `session/new` asks the embedder's
//!   [`SessionFactory`] to boot one with the client's `cwd` and MCP servers; `session/load`
//!   resumes one and replays its head branch as `session/update`s before answering.
//! * `session/prompt` runs one turn. Streamed text and thinking become `agent_message_chunk` /
//!   `agent_thought_chunk`; `Tool/Call` becomes `tool_call` (kind from the consequence class),
//!   `Op/Started` an `in_progress` update, `Tool/Result` a `completed`/`failed` update with the
//!   result content; `Todo/Written` becomes `plan`. The answer (`stopReason`) follows every
//!   update of the turn.
//! * Approval cards become `session/request_permission` with `allow-once`, `allow-always` (aho's
//!   session scope), and `reject-once`. Authority is process ownership: the ACP client is the
//!   operator, like the rpc surface; the agent never answers its own card.
//! * `session/cancel` aborts the turn: the Session stops its running calls and ends the turn at
//!   the next boundary (between steps, mid model request, or while waiting) without calling the
//!   model again; calls the turn left open are reported `failed`, then the prompt answers
//!   `cancelled`. Approval cards raised after the cancel are answered `cancelled` without asking.
//!   `$/cancel_request` on a prompt does the same and answers `-32800`.
//! * Capabilities advertised: `loadSession` when the factory can resume, `embeddedContext`
//!   (text resources are inlined into the prompt), no image/audio, MCP over stdio only, no
//!   authentication, no client fs/terminal use.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

mod live;
pub mod map;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aho_acp::schema::{
    AgentCapabilities, CancelNotification, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, McpServer, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
};
use aho_acp::{Handler, HandlerFuture, Peer, RpcError, methods};
use aho_approval::ApprovalRequest;
use aho_cap::{BuildError, Capability, CapabilityId, Kind, Manifest, Registrar};
use aho_log::{EntryKind, Takeover};
use aho_session::Session;
use aho_types::{BoxFuture, Principal, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub use live::{ALLOW_ONCE, ALLOW_SESSION, REJECT_ONCE};

// ───────────────────────────── capability ─────────────────────────────

pub struct SurfaceAcp;

impl Capability for SurfaceAcp {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SurfaceAcp)
    }
    fn register(&self, _r: &mut Registrar<'_>) {}
}

aho_cap::declare_capability!(
    SurfaceAcp,
    "surface-acp",
    kind = Kind::Surface,
    provides = [],
    requires = [],
    tools = &[],
    doc = "ACP v1 over stdio: aho as an Agent Client Protocol agent; one ACP session per aho session, approvals as permission requests."
);

// ───────────────────────────── embedder API ─────────────────────────────

/// A `session/new` (or, with `load`, `session/load`) as the embedder sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRequest {
    /// Absolute (checked); the session's working directory.
    pub cwd: PathBuf,
    /// MCP servers the client wants connected; see [`config_sets`].
    pub mcp_servers: Vec<McpServer>,
    /// `Some(id)`: resume the session the client names (an id from an earlier `session/new`).
    pub load: Option<String>,
    /// `clientInfo.name` from `initialize`, if the client sent one.
    pub client: Option<String>,
}

/// A booted session: the surface drives it, and answers its approval cards when `approvals` is
/// the receiver of the `ChannelApproval` it was booted with (`None`: prompts off, cards are
/// `Unavailable`).
pub struct Opened {
    pub session: Arc<Session>,
    pub approvals: Option<mpsc::Receiver<ApprovalRequest>>,
}

/// Boots or resumes the aho session behind one ACP session.
pub trait SessionFactory: Send + Sync + 'static {
    /// `Err` is operator voice; it reaches the client as the request's error message.
    fn open(&self, req: SessionRequest) -> BoxFuture<'_, Result<Opened, String>>;
    /// Whether `open` accepts `load: Some(..)`; advertised as `loadSession`.
    fn supports_load(&self) -> bool {
        false
    }
}

/// Sink for operator-facing diagnostics (stdout is the protocol; `serve_stdio` uses stderr).
pub type Diag = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct AcpOptions {
    /// `agentInfo`.
    pub name: String,
    pub title: Option<String>,
    pub version: String,
    pub diag: Diag,
}

impl Default for AcpOptions {
    fn default() -> Self {
        AcpOptions {
            name: "aho".into(),
            title: Some("aho".into()),
            version: env!("CARGO_PKG_VERSION").into(),
            diag: Arc::new(|_: &str| {}),
        }
    }
}

#[derive(Debug)]
pub enum AcpError {
    Io(std::io::Error),
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Io(e) => write!(f, "the ACP connection failed ({e}); the sessions it drove were cancelled"),
        }
    }
}

impl std::error::Error for AcpError {}

/// The `--set` pairs an embedder applies before booting the session for `req`: `workspace.root`
/// (the host world's workspace) = the client's `cwd`, plus the client's stdio MCP servers
/// (see [`map::mcp_sets`]).
pub fn config_sets(req: &SessionRequest) -> Result<Vec<(String, String)>, String> {
    let cwd = req.cwd.to_str().ok_or_else(|| format!("the cwd {} is not UTF-8; aho config keys hold UTF-8 paths", req.cwd.display()))?;
    let mut sets = vec![("workspace.root".to_string(), serde_json::to_string(cwd).unwrap_or_default())];
    sets.extend(map::mcp_sets(&req.mcp_servers)?);
    Ok(sets)
}

// ───────────────────────────── server ─────────────────────────────

struct Agent {
    peer: Peer,
    factory: Arc<dyn SessionFactory>,
    opts: AcpOptions,
    initialized: AtomicBool,
    client: Mutex<Option<String>>,
    lives: live::Lives,
}

#[derive(Clone)]
struct AgentHandler(Arc<Agent>);

fn absolute(cwd: &str) -> Result<PathBuf, RpcError> {
    let p = Path::new(cwd);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Err(RpcError::invalid_params(format!("`cwd` must be an absolute path, got `{cwd}`; nothing was started")))
    }
}

fn no_extra_roots(dirs: &[String]) -> Result<(), RpcError> {
    if dirs.is_empty() {
        Ok(())
    } else {
        Err(RpcError::invalid_params(
            "aho does not take additionalDirectories (sessionCapabilities.additionalDirectories is not advertised); nothing was started",
        ))
    }
}

impl Agent {
    fn require_init(&self) -> Result<(), RpcError> {
        if self.initialized.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(RpcError::invalid_request("call `initialize` first; the connection has not negotiated a protocol version"))
        }
    }

    fn client(&self) -> Option<String> {
        self.client.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        if let Some(info) = &req.client_info {
            *self.client.lock().unwrap_or_else(|p| p.into_inner()) = Some(info.name.clone());
        }
        self.initialized.store(true, Ordering::SeqCst);
        InitializeResponse {
            protocol_version: aho_acp::negotiate(req.protocol_version),
            agent_capabilities: AgentCapabilities {
                load_session: self.factory.supports_load(),
                prompt_capabilities: PromptCapabilities { image: false, audio: false, embedded_context: true, meta: None },
                mcp_capabilities: McpCapabilities { http: false, sse: false, meta: None },
                session_capabilities: serde_json::Map::new(),
                meta: None,
            },
            auth_methods: Vec::new(),
            agent_info: Some(Implementation {
                name: self.opts.name.clone(),
                title: self.opts.title.clone(),
                version: self.opts.version.clone(),
                meta: None,
            }),
            meta: None,
        }
    }

    /// Boots through the factory, logs the attachment, and starts forwarding.
    async fn attach(&self, req: SessionRequest) -> Result<Arc<live::Live>, RpcError> {
        let load = req.load.clone();
        let opened = self.factory.open(req.clone()).await.map_err(RpcError::internal)?;
        let id = load.clone().unwrap_or_else(|| format!("{:x}", opened.session.id.0));
        if self.lives.contains(&id) {
            return Err(RpcError::invalid_request(format!("session {id} is already open on this connection; keep using it")));
        }
        opened
            .session
            .log()
            .append(
                None,
                Principal::system("surface-acp"),
                EntryKind::SurfaceAttached {
                    client_instance: format!("acp:{}", req.client.as_deref().unwrap_or("client")),
                    has_local_history: load.is_some(),
                    takeover: Takeover::Allowed,
                },
            )
            .map_err(|e| RpcError::internal(format!("the session log could not record the attachment ({e}); nothing was started")))?;
        let live = live::Live::start(id, opened.session, self.peer.clone(), self.opts.diag.clone(), opened.approvals, load.is_some())
            .map_err(RpcError::internal)?;
        self.lives.insert(live.clone());
        Ok(live)
    }

    async fn handle(self: Arc<Self>, method: String, params: Value) -> Result<Value, RpcError> {
        match method.as_str() {
            methods::INITIALIZE => aho_acp::result(&self.initialize(aho_acp::params(&method, params)?)),
            methods::AUTHENTICATE => {
                self.require_init()?;
                Err(RpcError::invalid_params("aho offers no authentication methods (authMethods is empty); no authentication is needed"))
            }
            methods::SESSION_NEW => {
                self.require_init()?;
                let p: NewSessionRequest = aho_acp::params(&method, params)?;
                no_extra_roots(&p.additional_directories)?;
                let req = SessionRequest { cwd: absolute(&p.cwd)?, mcp_servers: p.mcp_servers, load: None, client: self.client() };
                let live = self.attach(req).await?;
                aho_acp::result(&NewSessionResponse { session_id: live.id.clone(), modes: None, config_options: None, meta: None })
            }
            methods::SESSION_LOAD => {
                self.require_init()?;
                if !self.factory.supports_load() {
                    return Err(RpcError::method_not_found(&method));
                }
                let p: LoadSessionRequest = aho_acp::params(&method, params)?;
                no_extra_roots(&p.additional_directories)?;
                let req =
                    SessionRequest { cwd: absolute(&p.cwd)?, mcp_servers: p.mcp_servers, load: Some(p.session_id), client: self.client() };
                self.attach(req).await?;
                aho_acp::result(&LoadSessionResponse::default())
            }
            methods::SESSION_PROMPT => {
                self.require_init()?;
                let p: PromptRequest = aho_acp::params(&method, params)?;
                let live = self.lives.get(&p.session_id).ok_or_else(|| {
                    RpcError::resource_not_found(format!("there is no session {} on this connection; call session/new first", p.session_id))
                })?;
                let text = map::prompt_text(&p.prompt)?;
                aho_acp::result(&live.prompt(text).await?)
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }
}

impl Handler for AgentHandler {
    fn request(&self, method: String, params: Value) -> HandlerFuture {
        Box::pin(self.0.clone().handle(method, params))
    }

    fn notification(&self, method: String, params: Value) {
        if method != methods::SESSION_CANCEL {
            return;
        }
        match aho_acp::params::<CancelNotification>(&method, params) {
            Ok(c) => match self.0.lives.get(&c.session_id) {
                Some(live) => live.cancel(),
                None => (self.0.opts.diag)(&format!("session/cancel names unknown session {}; nothing was cancelled", c.session_id)),
            },
            Err(e) => (self.0.opts.diag)(&format!("session/cancel ignored: {e}")),
        }
    }

    fn input_closed(&self) {
        // The client is gone: nobody will read these turns' updates; wind them down.
        self.0.lives.stop_all();
    }
}

/// Serves one ACP client over a reader/writer pair. Returns when the client closes its side;
/// turns still running are cancelled and answered first.
pub async fn serve_io<R, W>(reader: R, writer: W, factory: Arc<dyn SessionFactory>, opts: AcpOptions) -> Result<(), AcpError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (peer, driver) = aho_acp::connection(reader, writer);
    let agent = Arc::new(Agent {
        peer,
        factory,
        opts,
        initialized: AtomicBool::new(false),
        client: Mutex::new(None),
        lives: live::Lives::default(),
    });
    let served = driver.run(Arc::new(AgentHandler(agent.clone()))).await;
    agent.lives.stop_all();
    served.map_err(AcpError::Io)
}

/// `aho acp`: serves the ACP client that spawned this process on stdin/stdout; diagnostics go
/// to stderr (stdout carries only ACP messages).
pub async fn serve_stdio(factory: Arc<dyn SessionFactory>, mut opts: AcpOptions) -> Result<(), AcpError> {
    opts.diag = Arc::new(|m: &str| eprintln!("aho acp: {m}"));
    serve_io(tokio::io::stdin(), tokio::io::stdout(), factory, opts).await
}

#[cfg(test)]
mod tests;
