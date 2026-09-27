//! `aho-sdk-min`: the spine plus the rpc surface, embeddable as a Rust library.
//!
//! * Rust: [`Sdk::builder`] loads the composition, [`Sdk::open_session`] boots a [`Session`] and
//!   attaches an in-process rpc [`Client`] to it (`prompt`, `subscribe`, `approve`, typed
//!   [`Event`]s). The same client drives a child `aho rpc` ([`Client::spawn`]) or a socket.
//!   [`SdkSession::session`] is the `Session` API itself.
//! * TS/Python: thin clients over `aho rpc` stdio, generated from the protocol table
//!   ([`protocol`]) by `cargo run -p aho-sdk -- gen` into `aho/sdk/`; a test fails when the
//!   committed sources drift from the table ([`codegen`]).
//!
//! Nothing here re-implements the session: approvals, takeover, replay, and every method go
//! through surface-rpc exactly as for any other client.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod client;
pub mod codegen;
pub mod events;
pub mod protocol;
pub mod validate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_approval::{Approval, ChannelApproval, NeverApproval};
use aho_cap::{Composition, Registry};
use aho_compose::{ComposeError, Inputs, Planned};
use aho_log::{LogStore, MemoryLog};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session, SessionError};
use aho_types::*;
use serde_json::json;
use surface_rpc::{CapsContext, RpcContext, RpcError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;

pub use client::{Client, Events, Spawn, hello};
pub use events::{Answer, Answered, ApprovalCard, Checkpoint, Event, Fold, Idle, InvariantState, Subscribed};

/// The product name; bundle defaults are attributed to it (`Layer::Bundle("aho-sdk-min")`).
pub const NAME: &str = "aho-sdk-min";

/// `hello.client_instance` of the in-process client.
pub const CLIENT_INSTANCE: &str = "aho-sdk";

/// The spine plus the rpc surface.
pub fn sdk_min() -> Composition {
    Composition::new(NAME).include(aho_base::spine()).surface::<surface_rpc::SurfaceRpc>()
}

// ───────────────────────────── errors ─────────────────────────────

#[derive(Debug)]
pub enum SdkError {
    /// Loading the composition failed (bad config file, unknown preset, …).
    Compose(ComposeError),
    /// The session could not boot or its log could not be written.
    Session(SessionError),
    Io(std::io::Error),
    /// The builder or a call was given something unusable.
    Config(String),
    /// The server refused or never answered `hello`.
    Handshake(String),
    /// The server answered a request with an `error` frame.
    Rpc {
        method: String,
        message: String,
    },
    /// The connection ended.
    Closed(String),
    /// A frame did not have the shape this client was built for.
    Protocol(String),
    /// The in-process rpc server stopped with an error.
    Server(String),
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SdkError::Compose(e) => write!(f, "{e}"),
            SdkError::Session(e) => write!(f, "{e}"),
            SdkError::Io(e) => write!(f, "I/O failed: {e}"),
            SdkError::Config(m) => f.write_str(m),
            SdkError::Handshake(m) => write!(f, "the rpc handshake failed: {m}"),
            SdkError::Rpc { method, message } => write!(f, "`{method}` failed: {message}"),
            SdkError::Closed(m) => write!(f, "{m}; nothing more can be sent on this connection. Open a new session or reconnect."),
            SdkError::Protocol(m) => {
                write!(f, "{m}. This client and the server disagree about the protocol; upgrade the older side or regenerate the SDK.")
            }
            SdkError::Server(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SdkError {}

impl From<ComposeError> for SdkError {
    fn from(e: ComposeError) -> Self {
        SdkError::Compose(e)
    }
}

impl From<SessionError> for SdkError {
    fn from(e: SessionError) -> Self {
        SdkError::Session(e)
    }
}

impl From<aho_log::LogError> for SdkError {
    fn from(e: aho_log::LogError) -> Self {
        SdkError::Session(SessionError::Log(e))
    }
}

impl From<std::io::Error> for SdkError {
    fn from(e: std::io::Error) -> Self {
        SdkError::Io(e)
    }
}

// ───────────────────────────── builder ─────────────────────────────

/// Configures an [`Sdk`]: which composition, which project and home, and the operator's layers
/// (`set`, `disable`, `profile`, env). Everything an operator can pass `aho` on the command line.
pub struct SdkBuilder {
    composition: Composition,
    project: Option<PathBuf>,
    home: Option<PathBuf>,
    sets: Vec<(String, String)>,
    disable: Vec<String>,
    profile: Option<String>,
    env: BTreeMap<String, String>,
    inherit_env: bool,
    strict: bool,
    script: Option<PathBuf>,
    approvals: bool,
    seed: Registry,
}

impl SdkBuilder {
    /// The product to run; default [`sdk_min`]. Extend it with `sdk_min().consume::<MyTool>()`.
    pub fn composition(mut self, c: Composition) -> Self {
        self.composition = c;
        self
    }
    /// The project directory (`./aho.toml`, `session.dir`, the default workspace root);
    /// default: the process's current directory.
    pub fn project(mut self, dir: impl Into<PathBuf>) -> Self {
        self.project = Some(dir.into());
        self
    }
    /// The operator's home (`~/.aho`); required unless [`SdkBuilder::inherit_env`].
    pub fn home(mut self, dir: impl Into<PathBuf>) -> Self {
        self.home = Some(dir.into());
        self
    }
    /// `--set key=value` (the Arg layer); `value` is `--set` text: a TOML scalar or a bare string.
    pub fn set(mut self, key: &str, value: &str) -> Self {
        self.sets.push((key.to_string(), value.to_string()));
        self
    }
    /// `--disable <id>`: always wins, even over a broken config.
    pub fn disable(mut self, id: &str) -> Self {
        self.disable.push(id.to_string());
        self
    }
    /// `--profile <name>`: `~/.aho/profiles/<name>.toml`.
    pub fn profile(mut self, name: &str) -> Self {
        self.profile = Some(name.to_string());
        self
    }
    /// One environment variable as the resolver sees it (`AHO_*`, `PATH` for binary probes,
    /// credential variables). Wins over [`SdkBuilder::inherit_env`].
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }
    /// Read the process environment (`HOME`, `PATH`, `AHO_*`, credentials) like the `aho` CLI.
    pub fn inherit_env(mut self) -> Self {
        self.inherit_env = true;
        self
    }
    /// Refuse to build on any loadout diagnostic (`--strict`).
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }
    /// Drive the deterministic scripted model with this JSON script (like `aho run --script`).
    pub fn script(mut self, path: impl Into<PathBuf>) -> Self {
        self.script = Some(path.into());
        self
    }
    /// Route approval cards to rpc clients (default). Off: calls that need a human are denied.
    pub fn approvals(mut self, on: bool) -> Self {
        self.approvals = on;
        self
    }
    /// Extra handles/services the embedder provides, seeded before the composition starts.
    pub fn seed(mut self, seed: Registry) -> Self {
        self.seed = seed;
        self
    }

    #[allow(clippy::disallowed_methods, reason = "authority: sdk-builder-project")]
    pub fn build(self) -> Result<Sdk, SdkError> {
        let project = match self.project {
            Some(p) => p,
            None => std::env::current_dir()
                .map_err(|e| SdkError::Config(format!("the current directory cannot be read ({e}); pass .project(dir)")))?,
        };
        let project = std::fs::canonicalize(&project).map_err(|e| {
            SdkError::Config(format!(
                "project directory {} cannot be opened ({e}); pass an existing directory to .project()",
                project.display()
            ))
        })?;
        let mut sets = self.sets;
        if let Some(script) = &self.script {
            let path = std::fs::canonicalize(script).map_err(|e| {
                SdkError::Config(format!("model script {} cannot be opened ({e}); pass an existing file to .script()", script.display()))
            })?;
            sets.push(("provider.scripted.script".into(), path.display().to_string()));
            sets.push(("model.provider".into(), "scripted".into()));
        }
        let mut inputs = if self.inherit_env {
            Inputs::from_process(project.clone(), sets, self.disable, self.profile, self.strict)?
        } else {
            let home = self.home.clone().ok_or_else(|| {
                SdkError::Config("no home directory: pass .home(dir) (where ~/.aho lives) or .inherit_env() to use $HOME".into())
            })?;
            let mut i = Inputs::new(project.clone(), home);
            i.sets = sets;
            i.disable = self.disable;
            i.profile = self.profile;
            i.strict = self.strict;
            i
        };
        if let Some(h) = self.home {
            inputs.home = h;
        }
        inputs.env.extend(self.env);
        inputs.capture_files()?;
        let composition = embedder_defaults(self.composition, &project, &inputs.home);
        let planned = aho_compose::load(&composition, &inputs)?;
        Ok(Sdk { composition, inputs, planned, project, approvals: self.approvals, seed: self.seed })
    }
}

/// The CLI enters the project directory so the host world's default root is the project; an
/// embedder cannot change the process's directory for one session, so the project (and a shadow
/// dir under the given home) become bundle defaults instead; every operator layer still wins.
fn embedder_defaults(c: Composition, project: &Path, home: &Path) -> Composition {
    let schema = aho_compose::schema_of(&c);
    let shadow = home.join(".aho").join("shadow");
    [("workspace.root", project.display().to_string()), ("workspace.shadow_dir", shadow.display().to_string())].into_iter().fold(
        c,
        |c, (k, v)| {
            if schema.keys.contains_key(k) && !c.defaults().iter().any(|(d, _)| d == k) { c.default(k, json!(v)) } else { c }
        },
    )
}

// ───────────────────────────── sdk ─────────────────────────────

/// The log a session runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Open {
    /// A new session under `session.dir` (`<id>.jsonl`), like `aho run`.
    New,
    /// Resume `<session.dir>/<id>.jsonl`.
    Resume(SessionId),
    /// A log that lives only as long as the session (nothing on disk).
    InMemory,
}

/// A loaded product: composition, the operator's inputs, and the resolved plan.
pub struct Sdk {
    composition: Composition,
    inputs: Inputs,
    planned: Planned,
    project: PathBuf,
    approvals: bool,
    seed: Registry,
}

struct Booted {
    session: Arc<Session>,
    rpc: Arc<RpcContext>,
    path: Option<PathBuf>,
}

/// Time-ordered id: milliseconds in the high bits, then pid and sub-millisecond noise (the
/// scheme `aho` uses, so SDK sessions sort with CLI sessions in `aho session list`).
pub fn new_session_id() -> SessionId {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let ms = now.as_millis() & ((1u128 << 48) - 1);
    let pid = u128::from(std::process::id() & 0xffff);
    let noise = u128::from(now.subsec_nanos() % 1_000_000);
    SessionId((ms << 64) | (pid << 32) | noise)
}

impl Sdk {
    pub fn builder() -> SdkBuilder {
        SdkBuilder {
            composition: sdk_min(),
            project: None,
            home: None,
            sets: Vec::new(),
            disable: Vec::new(),
            profile: None,
            env: BTreeMap::new(),
            inherit_env: false,
            strict: false,
            script: None,
            approvals: true,
            seed: Registry::default(),
        }
    }

    pub fn composition(&self) -> &Composition {
        &self.composition
    }
    /// The resolved loadout and plan (what `aho plan` prints).
    pub fn planned(&self) -> &Planned {
        &self.planned
    }
    pub fn project(&self) -> &Path {
        &self.project
    }
    /// `session.dir`, relative to the project.
    pub fn session_dir(&self) -> PathBuf {
        self.project.join(self.planned.loadout.str("session.dir").unwrap_or(".aho/sessions"))
    }

    #[allow(clippy::disallowed_methods, reason = "authority: sdk-workspace-root")]
    fn workspace(&self) -> PathBuf {
        match self.planned.loadout.str("workspace.root").filter(|s| !s.is_empty()) {
            Some(r) => std::fs::canonicalize(self.project.join(r)).unwrap_or_else(|_| self.project.join(r)),
            None => self.project.clone(),
        }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: sdk-session-open")]
    async fn boot(&self, open: Open) -> Result<Booted, SdkError> {
        let dir = self.session_dir();
        // Through the cross-session index, like every other writer of this directory. Opened
        // only for on-disk sessions: an in-memory one must not create the directory or database.
        let writer = |id: SessionId| -> Result<Arc<dyn LogStore>, SdkError> {
            let index = Arc::new(
                aho_log::SessionIndex::open(dir.join("index.sqlite3"), &dir)
                    .map_err(|e| SdkError::Config(format!("the session index in {} could not be opened ({e})", dir.display())))?,
            );
            let log =
                index.open_log(id).map_err(|e| SdkError::Config(format!("the log of session {:x} could not be opened ({e})", id.0)))?;
            Ok(Arc::new(log))
        };
        let (id, log, path): (SessionId, Arc<dyn LogStore>, Option<PathBuf>) = match open {
            Open::New => {
                let id = new_session_id();
                (id, writer(id)?, Some(dir.join(format!("{:x}.jsonl", id.0))))
            }
            Open::Resume(id) => {
                let path = dir.join(format!("{:x}.jsonl", id.0));
                if !path.exists() {
                    return Err(SdkError::Config(format!(
                        "no session {:x} in {}; nothing was opened. Open a new session, or pass an id `aho session list` shows",
                        id.0,
                        dir.display()
                    )));
                }
                (id, writer(id)?, Some(path))
            }
            Open::InMemory => (new_session_id(), Arc::new(MemoryLog::new()), None),
        };
        let (approval, cards): (Arc<dyn Approval>, _) = if self.approvals {
            let (a, rx) = ChannelApproval::new(4);
            (Arc::new(a), Some(rx))
        } else {
            (Arc::new(NeverApproval), None)
        };
        let session = Session::boot(Boot {
            id,
            composition: &self.composition,
            plan: &self.planned.plan,
            loadout: &self.planned.loadout,
            bootstrap: aho_compose::bootstrap_entries(&self.planned),
            log,
            approval,
            // The embedding process owns the session, as stdio ownership does for `aho rpc`.
            operator: Operator::authenticate(OperatorProof::local_tty()),
            workspace: self.workspace().display().to_string(),
            home: self.inputs.home.display().to_string(),
            spill_dir: dir.join(format!("{:x}.spill", id.0)),
            seed: self.seed.clone(),
        })
        .await?;
        let caps = CapsContext::new(self.composition.clone(), self.inputs.clone());
        let rpc = RpcContext::new(&session, cards, Some(caps));
        Ok(Booted { session, rpc, path })
    }

    /// Boots a session and attaches an in-process rpc client to it. Must run inside a tokio runtime.
    pub async fn open_session(&self, open: Open) -> Result<SdkSession, SdkError> {
        let b = self.boot(open).await?;
        let (near, far) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(surface_rpc::serve(far, b.session.clone(), b.rpc.clone()));
        let (r, w) = tokio::io::split(near);
        let client = Client::connect(r, w, hello(CLIENT_INSTANCE)).await?;
        Ok(SdkSession { client, session: b.session, rpc: b.rpc, server, path: b.path })
    }

    /// Boots a session and serves the rpc protocol on `reader`/`writer` (stdio for `aho-sdk rpc`)
    /// until the client closes its side and every request it made has been answered.
    pub async fn serve_io<R, W>(&self, open: Open, reader: R, writer: W) -> Result<SessionId, SdkError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let b = self.boot(open).await?;
        let id = b.session.id;
        surface_rpc::serve_io(reader, writer, b.session, b.rpc).await.map_err(|e| SdkError::Server(e.to_string()))?;
        Ok(id)
    }
}

/// An embedded session with its in-process rpc client. Derefs to [`Client`]: `prompt`,
/// `subscribe`, `approve`, `events`, `fold`, … are rpc calls answered by surface-rpc.
pub struct SdkSession {
    client: Client,
    session: Arc<Session>,
    rpc: Arc<RpcContext>,
    server: JoinHandle<Result<(), RpcError>>,
    path: Option<PathBuf>,
}

impl std::ops::Deref for SdkSession {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}

impl SdkSession {
    pub fn id(&self) -> SessionId {
        self.session.id
    }
    /// The session log on disk; `None` for [`Open::InMemory`].
    pub fn log_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    /// The `Session` API itself: what every surface drives.
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }
    /// The rpc context: attach more clients with `surface_rpc::serve(stream, session, rpc)`.
    pub fn rpc(&self) -> &Arc<RpcContext> {
        &self.rpc
    }
    /// Closes the client and waits for the in-process server to finish what was in flight.
    pub async fn close(self) -> Result<(), SdkError> {
        self.client.close().await?;
        match self.server.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SdkError::Server(format!("the in-process rpc server stopped: {e}"))),
            Err(e) => Err(SdkError::Server(format!("the in-process rpc server task failed ({e})"))),
        }
    }
}
