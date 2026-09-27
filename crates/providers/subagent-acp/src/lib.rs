//! Subagents that are external ACP agents: aho is the ACP client (`surface-acp` is the server
//! side), speaking protocol version 1 (schema 1.23.0, see `aho-acp`), on the `subagent` seam next
//! to `subagent-inproc`.
//!
//! * An agent is `caps.subagent-acp.agents.<name>.{command, args, env}` (`env` values may be
//!   `cred:<name>`); a spawn uses `SpawnRequest.model` as the agent name, else
//!   `caps.subagent-acp.default`. The process starts in the child policy's workspace, gets
//!   `initialize` (no fs/terminal capabilities offered) and `session/new {cwd: workspace}`; every
//!   run is one `session/prompt`.
//! * The child's `session/update`s are mirrored into the parent log as `Subagent/*` custom
//!   entries (`Sent`, `Message`, `Note`, `ToolUse`, `ToolResult`, `Blocked`, `TurnEnd`, `Error`,
//!   `Attached`, `Closed`; body `{child, provider, …}`), the convention the foreign-CLI subagent
//!   providers share. `Subagent/Started` / `Subagent/Ended` come from `tool-subagent` as for every
//!   provider.
//! * Authority stays with the parent's operator: a `session/request_permission` whose claimed
//!   kind is outside the child policy (parent ⊓ declaration) is refused without asking;
//!   anything else is put to the parent's Approval handle (logged there). An async child
//!   without `bubble` has no approval channel, so its requests are refused. The agent's own
//!   process enforces nothing aho can check; the operator's card says so.
//! * `interrupt` sends `session/cancel` and answers every waiting permission request
//!   `cancelled`, as the protocol requires. An ACP agent takes one prompt at a time, so `send`
//!   to a running child is refused rather than queued.
//! * Not supported (rejected, never degraded): fork, persona, tool filters.

#![forbid(unsafe_code)]

mod child;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_acp::schema::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
};
use aho_acp::schema::{PromptRequest, PromptResponse, StopReason as AcpStop};
use aho_acp::{Peer, RpcError, methods};
use aho_approval::{Approval, ApprovalHandle, NeverApproval};
use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::{EntryKind, LogStore};
use aho_policy::SandboxPolicy;
use aho_subagent::{ChildInfo, Features, Mode, Result_, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use child::{ChildShared, ClientSide, Mirror, Stderr, lock};

pub use child::class_of;

/// Capability id; also `Subagent/Started.provider` and the mirrored entries' `provider`.
pub const PROVIDER: &str = "subagent-acp";
pub const DEFAULT_MAX_DEPTH: u32 = 3;
pub const DEFAULT_HANDSHAKE_MS: u64 = 30_000;
/// `Result_::diagnostic` bound (bytes).
pub const DIAGNOSTIC_MAX: usize = 4096;
/// Service the session seeds with its own log (`Arc<dyn LogStore>`): the mirror's target.
pub const LOG_SERVICE: &str = "aho.log";

// ───────────────────────────── agents ─────────────────────────────

/// A configured agent program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// An ACP byte stream to an agent.
pub struct Duplex {
    pub reader: Box<dyn AsyncRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
}

/// Opens a connection to an agent that is not a child process (an in-process `surface-acp`, a
/// socket the embedder owns). Gets the child policy's workspace.
pub type Connector = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, Result<Duplex, String>> + Send + Sync>;

#[derive(Clone)]
pub enum Launch {
    Command(AgentCommand),
    Connect(Connector),
}

/// Plain value or secret, with `cred:<n>` read from `credentials.<n>`.
fn secret_value(l: &Loadout, key: &str, raw: &str) -> Result<String, (String, String)> {
    let Some(name) = raw.strip_prefix("cred:") else { return Ok(raw.to_string()) };
    let target = format!("credentials.{name}");
    if let Some(s) = l.secrets.get(&target) {
        return Ok(s.expose().to_string());
    }
    match l.get(&target) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err((
            format!("`{key}` names credential `{name}`, which is not set; the agent was not started"),
            format!("aho credential set {name}"),
        )),
    }
}

/// Splits a command line on whitespace, honoring `"…"` and `'…'` quoting.
fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quote, mut any) = (String::new(), None::<char>, false);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parses `caps.subagent-acp.agents.<name>`; errors carry `(what happened, fix)`.
pub fn agent_command(l: &Loadout, name: &str, row: &Value) -> Result<AgentCommand, (String, String)> {
    let key = |k: &str| format!("caps.subagent-acp.agents.{name}.{k}");
    let set_cmd = format!("aho set {} <program>", key("command"));
    let Value::Object(row) = row else {
        return Err((format!("`caps.subagent-acp.agents.{name}` must be a table"), set_cmd));
    };
    let line = match row.get("command") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return Err((format!("`{}` is not set", key("command")), set_cmd)),
    };
    let (program, args) = match row.get("args") {
        None | Some(Value::Null) => {
            let mut parts = split_command(&line);
            let program = if parts.is_empty() { line.clone() } else { parts.remove(0) };
            (program, parts)
        }
        Some(Value::Array(a)) => {
            let args = a.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>().ok_or_else(|| {
                (format!("`{}` must be an array of strings", key("args")), format!("aho set {} '[\"--acp\"]'", key("args")))
            })?;
            (line, args)
        }
        Some(_) => {
            return Err((format!("`{}` must be an array of strings", key("args")), format!("aho set {} '[\"--acp\"]'", key("args"))));
        }
    };
    let mut env = Vec::new();
    match row.get("env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(vars)) => {
            for (var, v) in vars {
                let k = format!("{}.{var}", key("env"));
                let raw = v.as_str().ok_or_else(|| (format!("`{k}` must be a string"), format!("aho set {k} <value>")))?;
                env.push((var.clone(), secret_value(l, &k, raw)?));
            }
        }
        Some(_) => return Err((format!("`{}` must be a table of strings", key("env")), format!("aho set {}.<VAR> <value>", key("env")))),
    }
    Ok(AgentCommand { program, args, env })
}

/// `program` as the process would find it: a path with a separator is taken as is, else the
/// first executable file of that name on `PATH`.
#[allow(clippy::disallowed_methods, reason = "authority: subagent-acp-program")]
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| p.is_file())
}

// ───────────────────────────── capability ─────────────────────────────

pub struct AcpDeps {
    pub approval: ApprovalHandle,
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for AcpDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Approval];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(AcpDeps { approval: r.handle()?, log: r.service(LOG_SERVICE) })
    }
}

#[derive(Clone, Debug)]
pub struct AcpConfig {
    pub agents: BTreeMap<String, Result<AgentCommand, (String, String)>>,
    pub default: String,
    pub max_depth: u32,
    pub handshake_ms: u64,
    /// Secret values the agents' env resolved (redacted from diagnostics).
    pub secrets: Vec<String>,
}

impl Config for AcpConfig {
    const PREFIX: &'static str = "caps.subagent-acp";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "caps.subagent-acp.agents",
                Ty::Table,
                None,
                "External ACP agents: `caps.subagent-acp.agents.<name>.{command, args, env}`; `env` values may be `cred:<name>`.",
            ),
            ConfigKey::new(
                "caps.subagent-acp.default",
                Ty::String,
                None,
                "The agent a spawn uses when it names none (`<name>` under `agents`).",
            ),
            ConfigKey::new(
                "caps.subagent-acp.max_depth",
                Ty::Int,
                Some(json!(DEFAULT_MAX_DEPTH)),
                "Deepest delegation an ACP child may sit at (the root session is 0); a spawn past it is refused.",
            ),
            ConfigKey::new(
                "caps.subagent-acp.handshake_timeout_ms",
                Ty::Int,
                Some(json!(DEFAULT_HANDSHAKE_MS)),
                "Longest an agent may take to answer `initialize` and `session/new`.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let int = |key: &str, d: u64| -> Result<u64, ConfigError> {
            match l.get(key) {
                None => Ok(d),
                Some(v) => v.as_u64().ok_or(ConfigError::Type { key: key.into(), expected: "a non-negative integer" }),
            }
        };
        let max_depth = u32::try_from(int("caps.subagent-acp.max_depth", u64::from(DEFAULT_MAX_DEPTH))?)
            .map_err(|_| ConfigError::Type { key: "caps.subagent-acp.max_depth".into(), expected: "a small non-negative integer" })?;
        let handshake_ms = int("caps.subagent-acp.handshake_timeout_ms", DEFAULT_HANDSHAKE_MS)?.max(1);
        let agents: BTreeMap<String, Result<AgentCommand, (String, String)>> =
            l.table("caps.subagent-acp.agents").into_iter().map(|(name, row)| (name.clone(), agent_command(l, &name, &row))).collect();
        let secrets = l.secrets.values().map(|s| s.expose().to_string()).filter(|s| !s.is_empty()).collect();
        Ok(AcpConfig { agents, default: cfg_str(l, "caps.subagent-acp.default", ""), max_depth, handshake_ms, secrets })
    }
}

/// The capability: registers the provider as the `subagent` service (and concretely as
/// `subagent-acp`).
pub struct SubagentAcp {
    provider: Arc<AcpProvider>,
}

impl Capability for SubagentAcp {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = AcpDeps;
    type Config = AcpConfig;

    fn build(deps: AcpDeps, cfg: AcpConfig) -> Result<Self, BuildError> {
        let log = deps.log.ok_or(BuildError::Unsupported("the session did not seed `aho.log`, where ACP subagents are mirrored"))?;
        let default = cfg.default.clone();
        match cfg.agents.get(&default) {
            None => {
                return Err(BuildError::Probe(format!(
                    "`caps.subagent-acp.default = {default}` names no agent under `caps.subagent-acp.agents`; set `caps.subagent-acp.agents.{default}.command`"
                )));
            }
            Some(Err((what, fix))) => return Err(BuildError::Probe(format!("{what}; fix: `{fix}`"))),
            Some(Ok(a)) if resolve_program(&a.program).is_none() => {
                return Err(BuildError::Probe(format!(
                    "agent `{default}`: `{}` was not found (not a file, and not on PATH); install it or set `caps.subagent-acp.agents.{default}.command` to its full path",
                    a.program
                )));
            }
            Some(Ok(_)) => {}
        }
        let agents = cfg.agents.iter().filter_map(|(n, a)| a.as_ref().ok().map(|a| (n.clone(), Launch::Command(a.clone())))).collect();
        let broken =
            cfg.agents.iter().filter_map(|(n, a)| a.as_ref().err().map(|(w, f)| (n.clone(), format!("{w}; fix: `{f}`")))).collect();
        let settings = ProviderSettings {
            agents,
            broken,
            default,
            max_depth: cfg.max_depth,
            handshake: Duration::from_millis(cfg.handshake_ms),
            secrets: cfg.secrets,
        };
        Ok(SubagentAcp { provider: Arc::new(AcpProvider::new(settings, deps.approval.0, log)) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let dynp: Arc<dyn SubagentProvider> = self.provider.clone();
        r.service(aho_subagent::SERVICE, dynp);
        r.service(PROVIDER, self.provider.clone());
    }
}

declare_capability!(
    SubagentAcp,
    "subagent-acp",
    kind = Kind::Provider,
    provides = [aho_subagent::SEAM],
    requires = [Requirement::Setting { key: "caps.subagent-acp.default", pred: Pred::Set }],
    tools = &[],
    entry_kinds = [
        "Subagent/Attached",
        "Subagent/Sent",
        "Subagent/Message",
        "Subagent/Note",
        "Subagent/ToolUse",
        "Subagent/ToolResult",
        "Subagent/Blocked",
        "Subagent/TurnEnd",
        "Subagent/Error",
        "Subagent/Closed"
    ],
    doc = "External ACP agents as subagents: aho is the ACP client; child updates mirror into the parent log as Subagent/*, permission requests go to the parent's operator under parent ⊓ declaration."
);

// ───────────────────────────── provider ─────────────────────────────

/// The provider's settings; the capability builds them from config, embedders directly.
#[derive(Clone)]
pub struct ProviderSettings {
    pub agents: BTreeMap<String, Launch>,
    /// Agents whose config is wrong, with the operator-facing reason.
    pub broken: BTreeMap<String, String>,
    pub default: String,
    pub max_depth: u32,
    pub handshake: Duration,
    pub secrets: Vec<String>,
}

#[derive(Clone, Default)]
struct Run {
    runs: u64,
    result: Option<Result_>,
}

struct Child {
    id: SessionId,
    label: String,
    depth: u32,
    mode: Mode,
    peer: Peer,
    shared: Arc<ChildShared>,
    state: watch::Sender<Run>,
    interrupted: AtomicBool,
    conn: JoinHandle<()>,
    /// Killed when the child is dropped (`kill_on_drop`).
    _process: Option<tokio::process::Child>,
}

impl Drop for Child {
    fn drop(&mut self) {
        self.conn.abort();
    }
}

impl Child {
    fn info(&self) -> ChildInfo {
        let r = self.state.borrow();
        ChildInfo {
            child: self.id,
            provider: PROVIDER.into(),
            label: self.label.clone(),
            depth: self.depth,
            running: r.runs > 0 && r.result.is_none(),
            last_stop: r.result.as_ref().map(|x| x.stop),
        }
    }
    fn running(&self) -> bool {
        let r = self.state.borrow();
        r.runs > 0 && r.result.is_none()
    }
}

/// One session's ACP subagent provider.
pub struct AcpProvider {
    settings: ProviderSettings,
    parent_approval: Arc<dyn Approval>,
    parent_log: Arc<dyn LogStore>,
    children: Mutex<Vec<Arc<Child>>>,
    seq: AtomicU64,
}

fn provider_err(e: impl std::fmt::Display) -> SubagentError {
    SubagentError::Provider(e.to_string())
}

fn redact(text: &str, secrets: &[String]) -> String {
    secrets.iter().filter(|s| !s.is_empty()).fold(text.to_string(), |t, s| t.replace(s.as_str(), "[redacted]"))
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mark = "…";
    let mut end = max.saturating_sub(mark.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{mark}", &s[..end])
}

/// The depth the parent session sits at: its log's `Subagent/Descriptor`, or 0 for a root.
fn parent_depth(log: &dyn LogStore) -> u32 {
    log.read_from(None)
        .iter()
        .take(64)
        .find_map(|e| if let EntryKind::SubagentDescriptor { depth, .. } = e.kind { Some(depth) } else { None })
        .unwrap_or(0)
}

impl AcpProvider {
    pub fn new(settings: ProviderSettings, parent_approval: Arc<dyn Approval>, parent_log: Arc<dyn LogStore>) -> AcpProvider {
        AcpProvider { settings, parent_approval, parent_log, children: Mutex::new(Vec::new()), seq: AtomicU64::new(0) }
    }

    /// The ACP `sessionId` the agent gave a child.
    pub fn acp_session(&self, child: SessionId) -> Option<String> {
        self.find(child).and_then(|c| lock(&c.shared.session).clone())
    }

    fn find(&self, child: SessionId) -> Option<Arc<Child>> {
        lock(&self.children).iter().find(|c| c.id == child).cloned()
    }

    fn get(&self, child: SessionId) -> Result<Arc<Child>, SubagentError> {
        self.find(child)
            .ok_or_else(|| provider_err(format!("there is no subagent {} in this session; list_agents shows the live ones.", child.0)))
    }

    fn diagnostic(&self, text: &str) -> String {
        clip(&redact(text, &self.settings.secrets), DIAGNOSTIC_MAX)
    }

    /// Fresh child id below 2^53, so it stays exact wherever it is read as a number.
    fn next_id(&self) -> SessionId {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let h = Hash::of_json(&(PROVIDER, std::process::id(), n, Timestamp::now().0));
        let mut b = [0u8; 8];
        b.copy_from_slice(&h.0[..8]);
        SessionId(u128::from((u64::from_le_bytes(b) & ((1u64 << 53) - 1)).max(1)))
    }

    #[allow(clippy::disallowed_methods, reason = "authority: subagent-acp-launch")]
    async fn launch(
        &self,
        name: &str,
        launch: &Launch,
        cwd: PathBuf,
        stderr: Arc<Stderr>,
    ) -> Result<(Duplex, Option<tokio::process::Child>), SubagentError> {
        match launch {
            Launch::Connect(c) => {
                stderr.close();
                Ok((
                    c(cwd).await.map_err(|e| provider_err(format!("agent `{name}` could not be reached ({e}); nothing was started")))?,
                    None,
                ))
            }
            Launch::Command(a) => {
                let mut proc = tokio::process::Command::new(&a.program)
                    .args(&a.args)
                    .envs(a.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .env_remove("AHO_READY_FD") // the daemon's readiness fd (daemon::ready::READY_FD_ENV) is never a child's to write
                    .current_dir(&cwd)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| {
                        provider_err(format!(
                            "could not start `{}` for ACP agent `{name}` in {} ({e}); nothing is running. Check `caps.subagent-acp.agents.{name}.command`.",
                            a.program,
                            cwd.display()
                        ))
                    })?;
                let (Some(stdin), Some(stdout), Some(err)) = (proc.stdin.take(), proc.stdout.take(), proc.stderr.take()) else {
                    return Err(provider_err(format!("ACP agent `{name}` started without pipes; it was stopped")));
                };
                tokio::spawn(async move {
                    let mut lines = BufReader::new(err).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        stderr.push(l);
                    }
                    stderr.close();
                });
                Ok((Duplex { reader: Box::new(stdout), writer: Box::new(stdin) }, Some(proc)))
            }
        }
    }

    async fn handshake(&self, name: &str, peer: &Peer, shared: &ChildShared, cwd: &str) -> Result<String, String> {
        let init = InitializeRequest {
            protocol_version: aho_acp::PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities::default(),
            client_info: Some(Implementation {
                name: "aho".into(),
                title: Some("aho".into()),
                version: env!("CARGO_PKG_VERSION").into(),
                meta: None,
            }),
            meta: None,
        };
        let within = |what: &'static str, r: Result<Result<Value, RpcError>, tokio::time::error::Elapsed>| match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(format!("agent `{name}` refused `{what}`: {e}")),
            Err(_) => Err(format!("agent `{name}` did not answer `{what}` within {} ms", self.settings.handshake.as_millis())),
        };
        let v =
            within("initialize", tokio::time::timeout(self.settings.handshake, peer.call::<_, Value>(methods::INITIALIZE, &init)).await)?;
        let init: InitializeResponse =
            serde_json::from_value(v).map_err(|e| format!("agent `{name}`'s initialize result does not match ACP v1 ({e})"))?;
        if init.protocol_version != aho_acp::PROTOCOL_VERSION {
            return Err(format!(
                "agent `{name}` speaks ACP protocol version {}; aho speaks {}",
                init.protocol_version,
                aho_acp::PROTOCOL_VERSION
            ));
        }
        let new = NewSessionRequest { cwd: cwd.to_string(), additional_directories: vec![], mcp_servers: vec![], meta: None };
        let v =
            within("session/new", tokio::time::timeout(self.settings.handshake, peer.call::<_, Value>(methods::SESSION_NEW, &new)).await)?;
        let new: NewSessionResponse =
            serde_json::from_value(v).map_err(|e| format!("agent `{name}`'s session/new result does not match ACP v1 ({e})"))?;
        *lock(&shared.session) = Some(new.session_id.clone());
        shared.mirror.put(
            "Attached",
            json!({ "agent": name, "session": new.session_id, "protocol_version": init.protocol_version, "agent_info": init.agent_info }),
        );
        Ok(new.session_id)
    }

    async fn spawn_inner(&self, req: SpawnRequest, policy: SandboxPolicy) -> Result<SessionId, SubagentError> {
        let depth = aho_subagent::depth(parent_depth(&*self.parent_log).saturating_add(1), req.depth);
        if depth > self.settings.max_depth {
            return Err(SubagentError::DepthLimit(self.settings.max_depth));
        }
        if req.fork_at.is_some() {
            return Err(SubagentError::Unsupported("forking a conversation into an external ACP agent"));
        }
        if req.persona.is_some() {
            return Err(SubagentError::Unsupported("a persona for an external ACP agent"));
        }
        if !req.tool_filter.is_empty() {
            return Err(SubagentError::Unsupported("tool filters for an external ACP agent"));
        }
        if req.prompt.trim().is_empty() {
            return Err(provider_err("the subagent prompt is empty; nothing was started. Say what the child should do."));
        }
        let name = req.model.clone().unwrap_or_else(|| self.settings.default.clone());
        let Some(launch) = self.settings.agents.get(&name) else {
            let why = self.settings.broken.get(&name).cloned().unwrap_or_else(|| {
                let known: Vec<&str> = self.settings.agents.keys().map(String::as_str).collect();
                format!("there is no ACP agent `{name}` (configured: {})", if known.is_empty() { "none".into() } else { known.join(", ") })
            });
            return Err(provider_err(format!("{why}; nothing was started.")));
        };
        if matches!(launch, Launch::Command(_)) && !policy.allowed().contains(Consequence::SpawnsProcess) {
            return Err(provider_err(
                "this subagent's policy excludes SpawnsProcess, and an ACP agent is a process; nothing was started. Use a preset that allows processes.",
            ));
        }
        let cwd = policy.workspace().to_string();
        if !std::path::Path::new(&cwd).is_absolute() {
            return Err(provider_err(format!("the workspace `{cwd}` is not an absolute path, which ACP requires for the session cwd")));
        }

        let id = self.next_id();
        // An async child cannot reach a human unless bubbled.
        let approval: Arc<dyn Approval> = if !req.is_async || req.bubble { self.parent_approval.clone() } else { Arc::new(NeverApproval) };
        let shared =
            Arc::new(ChildShared::new(req.label.clone(), name.clone(), policy, approval, Mirror::new(self.parent_log.clone(), id)));
        let (duplex, process) = self.launch(&name, launch, PathBuf::from(&cwd), shared.stderr.clone()).await?;
        let (peer, driver) = aho_acp::connection(duplex.reader, duplex.writer);
        let watcher = shared.clone();
        let conn = tokio::spawn(async move {
            let reason = match driver.run(Arc::new(ClientSide(watcher.clone()))).await {
                Ok(()) => "the agent closed its output".to_string(),
                Err(e) => format!("the connection to the agent failed ({e})"),
            };
            watcher.mirror.put("Closed", json!({ "reason": reason, "stderr": watcher.stderr_tail().await }));
        });
        if let Err(e) = self.handshake(&name, &peer, &shared, &cwd).await {
            conn.abort();
            drop(process);
            let tail = shared.stderr_tail().await.map(|t| format!("\nagent stderr:\n{t}")).unwrap_or_default();
            return Err(provider_err(self.diagnostic(&format!("{e}; the agent was stopped and nothing was started.{tail}"))));
        }
        let child = Arc::new(Child {
            id,
            label: req.label,
            depth,
            mode: req.mode,
            peer,
            shared,
            state: watch::channel(Run::default()).0,
            interrupted: AtomicBool::new(false),
            conn,
            _process: process,
        });
        lock(&self.children).push(child.clone());
        self.start_run(&child, req.prompt);
        Ok(id)
    }

    /// Starts a run unless one is going; returns whether it started.
    fn start_run(&self, child: &Arc<Child>, text: String) -> bool {
        let started = child.state.send_if_modified(|r| {
            if r.runs == 0 || r.result.is_some() {
                r.runs += 1;
                r.result = None;
                true
            } else {
                false
            }
        });
        if !started {
            return false;
        }
        child.interrupted.store(false, Ordering::SeqCst);
        let c = child.clone();
        let secrets = self.settings.secrets.clone();
        tokio::spawn(async move {
            c.shared.begin_run(&text);
            let session = lock(&c.shared.session).clone().unwrap_or_default();
            let prompt = PromptRequest { session_id: session, prompt: vec![ContentBlock::text(text)], meta: None };
            let answer = c.peer.call::<_, PromptResponse>(methods::SESSION_PROMPT, &prompt).await;
            let last = c.shared.end_run();
            let interrupted = c.interrupted.load(Ordering::SeqCst);
            let (stop, mut diag) = match answer {
                Ok(r) => stop_of(r.stop_reason, interrupted),
                Err(e) => {
                    let tail = c.shared.stderr_tail().await.map(|t| format!("\nagent stderr:\n{t}")).unwrap_or_default();
                    (StopReason::Error, Some(format!("The ACP agent's turn failed: {e}.{tail}")))
                }
            };
            let failures = c.shared.mirror.take_failures();
            if !failures.is_empty() {
                diag = Some(
                    [diag.unwrap_or_default(), failures.join("; ")]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            let diag = diag.map(|d| clip(&redact(&d, &secrets), DIAGNOSTIC_MAX));
            let last = last.map(|m| redact(&m, &secrets));
            c.shared.mirror.put("TurnEnd", json!({ "stop": stop, "last_message": last }));
            if let Some(d) = &diag {
                c.shared.mirror.put("Error", json!({ "message": d, "retrying": false }));
            }
            let result = Result_ { child: c.id, stop, last_message: last, diagnostic: diag };
            c.state.send_modify(|r| r.result = Some(result));
        });
        true
    }
}

/// The agent's `stopReason` on the seam's closed `StopReason`, with an operator-voice
/// diagnostic for anything but a normal finish.
pub fn stop_of(s: AcpStop, interrupted: bool) -> (StopReason, Option<String>) {
    match s {
        AcpStop::EndTurn => (StopReason::EndTurn, None),
        AcpStop::MaxTokens => (StopReason::MaxTokens, None),
        AcpStop::Refusal => (StopReason::Refusal, None),
        AcpStop::Cancelled if interrupted => {
            (StopReason::Aborted, Some("The parent interrupted this subagent; its last message is what it had when it stopped.".into()))
        }
        AcpStop::Cancelled => (StopReason::Aborted, Some("The ACP agent cancelled the turn on its own.".into())),
        AcpStop::MaxTurnRequests => (
            StopReason::Aborted,
            Some("The ACP agent hit its own limit on model requests in one turn without finishing. Give it a smaller task.".into()),
        ),
    }
}

impl SubagentProvider for AcpProvider {
    fn features(&self) -> Features {
        Features { fork: false, persona: false, tool_filter: false, output_schema: false, max_depth: self.settings.max_depth }
    }

    fn spawn(&self, req: SpawnRequest, policy: SandboxPolicy) -> BoxFuture<'_, Result<SessionId, SubagentError>> {
        Box::pin(self.spawn_inner(req, policy))
    }

    fn send(&self, child: SessionId, text: String) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            if self.start_run(&c, text) {
                Ok(())
            } else {
                Err(provider_err(format!(
                    "subagent `{}` is still working, and an ACP agent takes one prompt at a time; nothing was sent. Wait for it, or interrupt it first.",
                    c.label
                )))
            }
        })
    }

    fn interrupt(&self, child: SessionId) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            if c.running() {
                c.interrupted.store(true, Ordering::SeqCst);
                // Every permission request still waiting answers `cancelled` (prompt-turn §Cancellation).
                c.shared.cancel.send_modify(|n| *n += 1);
                let session = lock(&c.shared.session).clone().unwrap_or_default();
                c.peer.notify(methods::SESSION_CANCEL, json!({ "sessionId": session })).map_err(|e| {
                    provider_err(format!("the cancel did not reach subagent `{}` ({e}); it is no longer connected", c.label))
                })?;
            }
            Ok(())
        })
    }

    fn wait(&self, child: SessionId) -> BoxFuture<'_, Result<Result_, SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            let mut rx = c.state.subscribe();
            let done = rx.wait_for(|r| r.runs > 0 && r.result.is_some()).await.map(|r| r.result.clone());
            let result = done.ok().flatten().ok_or_else(|| provider_err(format!("subagent {} went away before it finished", child.0)))?;
            if c.mode == Mode::OneShot {
                lock(&self.children).retain(|x| x.id != child);
            }
            Ok(result)
        })
    }

    fn child_info(&self, child: SessionId) -> Option<ChildInfo> {
        self.find(child).map(|c| c.info())
    }

    fn children(&self) -> Vec<ChildInfo> {
        lock(&self.children).iter().map(|c| c.info()).collect()
    }
}
