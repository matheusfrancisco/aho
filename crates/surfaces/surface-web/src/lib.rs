//! The web surface: axum behind HTTPS, `POST /api/<ns>/<method>` + the `/api/mux`
//! WebSocket, and a small embedded single-page UI (plain HTML/JS/CSS compiled in, no build step).
//!
//! Transport choice: the operator supplies the TLS pair (`aho cert` writes a local CA + leaf);
//! the surface never mints certificates itself. Plain HTTP exists only as a development escape
//! hatch and only on a loopback address: pass `tls = None` (the CLI exposes this solely as
//! `--insecure-loopback`); any other bind without a pair is refused.
//!
//! Auth: a random process token, printed at startup, is accepted as `Authorization: Bearer` or
//! exchanged once (`/auth?token=…` or `POST /api/auth/exchange`) for an HttpOnly, SameSite=Strict
//! cookie. Every request passes the Host/Origin fence first (DNS rebinding, cross-site calls).
//! The token holder is the operator: only the operator's terminal showed it.
//!
//! Slash commands: `POST /api/session/command {"line": "/name args…"}` runs a command from the
//! app's [`surface_tui::Commands`] registry (the same one the TUI dispatches), behind the same
//! auth and fence as every `/api` route; the SPA sends `/…` lines from its prompt box there.
//!
//! Panels: the UI renders only the slots of Active capabilities, read from the registry
//! (`Registrar::panel`) and filtered by the fold.

#![forbid(unsafe_code)]
// Handlers return `Result<_, Response>`; axum responses are large by design and returned once.
#![allow(clippy::result_large_err)]

mod auth;
mod mux;
mod server;
mod tls;
mod views;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use aho_approval::ApprovalRequest;
use aho_cap::*;
use aho_compose::Planned;
use aho_policy::{Operator, OperatorProof};
use aho_session::Session;
use surface_tui::Commands;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

pub use auth::Fence;
pub use views::{PROTOCOL_VERSION, active_slots, frame, parse_mode, stream_json};

pub struct SurfaceWeb;

impl Capability for SurfaceWeb {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SurfaceWeb)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.panel("chat", &["transcript", "stream", "checkpoints", "approvals"]);
        r.panel("sidebar.tree", &["tree"]);
        r.panel("sidebar.inventory", &["inventory"]);
        r.panel("sidebar.jobs", &["ops"]);
        r.panel("sidebar.stats", &["stats"]);
        r.panel("sidebar.invariants", &["invariants"]);
    }
}

declare_capability!(
    SurfaceWeb,
    "surface-web",
    kind = Kind::Surface,
    provides = [],
    requires = [],
    tools = &[],
    doc = "Browser UI over HTTPS: chat with tool cards and checkpoint timeline, approval modal, tree/inventory/jobs/invariants panels."
);

pub const SLOTS: &[&str] = &[
    "chat",
    "sidebar.tree",
    "sidebar.inventory",
    "sidebar.goals",
    "sidebar.todo",
    "sidebar.jobs",
    "sidebar.stats",
    "sidebar.vm",
    "sidebar.mcp",
    "sidebar.skills",
    "sidebar.invariants",
    "settings.*",
];

/// Knobs beyond `serve`'s arguments.
#[derive(Clone, Debug, Default)]
pub struct WebOptions {
    /// Extra `Host` names (with or without `:port`) the fence accepts, e.g. a LAN hostname when
    /// bound to `0.0.0.0`. Loopback names and the bind address are always accepted.
    pub allowed_hosts: Vec<String>,
    /// Composition label shown in the tree header; default `aho`.
    pub label: Option<String>,
}

/// A running server.
pub struct WebServer {
    pub addr: SocketAddr,
    pub token: String,
    pub tls: bool,
    task: JoinHandle<Result<(), String>>,
}

impl WebServer {
    /// Binds and starts serving; returns once the listener is up. `commands` answers the `/…`
    /// lines of the prompt box.
    pub async fn start(
        session: Arc<Session>,
        approvals: mpsc::Receiver<ApprovalRequest>,
        planned: Planned,
        commands: Commands,
        bind: SocketAddr,
        tls: Option<(String, String)>,
        opts: WebOptions,
    ) -> Result<WebServer, String> {
        if tls.is_none() && !bind.ip().is_loopback() {
            return Err(format!(
                "refusing to serve plain HTTP on {bind}: the web surface is HTTPS-only. Pass a certificate and key (`aho cert` writes a pair), or bind 127.0.0.1 with --insecure-loopback for local development."
            ));
        }
        let tls_cfg = match &tls {
            Some((cert, key)) => Some(tls::server_config(cert, key)?),
            None => None,
        };
        #[allow(clippy::disallowed_methods, reason = "authority: web-listener")]
        let tcp = tokio::net::TcpListener::bind(bind).await.map_err(|e| format!("could not listen on {bind}: {e}"))?;
        let addr = tcp.local_addr().map_err(|e| format!("the listener has no address: {e}"))?;
        let token = auth::random_hex(32)?;
        let (web_events, _) = broadcast::channel(256);
        let app = Arc::new(server::AppState {
            session,
            planned,
            commands,
            label: opts.label.unwrap_or_else(|| "aho".into()),
            token: token.clone(),
            cookie: auth::random_hex(32)?,
            exchanged: AtomicBool::new(false),
            secure: tls_cfg.is_some(),
            fence: Fence::new(addr, tls_cfg.is_some(), &opts.allowed_hosts),
            operator: Operator::authenticate(OperatorProof::local_tty()),
            pending: Mutex::new(Default::default()),
            next_card: AtomicU64::new(0),
            web_events,
        });
        server::spawn_approval_pump(app.clone(), approvals);
        let router = server::router(app);
        let task = match tls_cfg {
            Some(cfg) => {
                let listener = tls::TlsListener::new(tcp, cfg)?;
                tokio::spawn(async move { axum::serve(listener, router).await.map_err(|e| format!("the web server stopped: {e}")) })
            }
            None => tokio::spawn(async move { axum::serve(tcp, router).await.map_err(|e| format!("the web server stopped: {e}")) }),
        };
        Ok(WebServer { addr, token, tls: tls.is_some(), task })
    }

    pub fn base_url(&self) -> String {
        format!("{}://{}", server::origin_scheme(self.tls), server::display_host(self.addr))
    }

    /// Open once in the browser: exchanges the token for the session cookie.
    pub fn login_url(&self) -> String {
        format!("{}/auth?token={}", self.base_url(), self.token)
    }

    /// Serves until the server stops.
    pub async fn wait(self) -> Result<(), String> {
        self.task.await.map_err(|e| format!("the web server task failed: {e}"))?
    }

    pub fn shutdown(self) {
        self.task.abort();
    }
}

/// Serves the session until the process ends. Prints the one-time login link and the bearer token
/// on stderr (the operator's terminal).
pub async fn serve(
    session: Arc<Session>,
    approvals: mpsc::Receiver<ApprovalRequest>,
    planned: Planned,
    commands: Commands,
    bind: SocketAddr,
    tls: Option<(String, String)>,
) -> Result<(), String> {
    serve_with(session, approvals, planned, commands, bind, tls, WebOptions::default()).await
}

pub async fn serve_with(
    session: Arc<Session>,
    approvals: mpsc::Receiver<ApprovalRequest>,
    planned: Planned,
    commands: Commands,
    bind: SocketAddr,
    tls: Option<(String, String)>,
    opts: WebOptions,
) -> Result<(), String> {
    let s = WebServer::start(session, approvals, planned, commands, bind, tls, opts).await?;
    if !s.tls {
        eprintln!("aho web: plain HTTP on loopback ({}), for development only", s.addr);
    }
    eprintln!("aho web: open {}", s.login_url());
    eprintln!("aho web: API clients send `Authorization: Bearer {}`", s.token);
    s.wait().await
}
