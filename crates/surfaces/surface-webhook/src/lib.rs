//! `08` — the webhook surface: GitHub events become `prompt`s on one bound session.
//!
//! A second HTTP listener (not the web surface's port) serves `POST /github`. Each request passes,
//! in order: a body size fence (413), GitHub's `X-Hub-Signature-256` HMAC-SHA256 checked in
//! constant time against the secret named by a credential ref (401 when missing or wrong),
//! header validation (400), the subscription list `surface.webhook.events` (204 when not
//! subscribed, or when an `issue_comment` does not mention the trigger), and dedupe by
//! `X-GitHub-Delivery` (a replay answers 200 and prompts nothing). An accepted delivery answers
//! 202 at once; a single worker hands prompts to the [`PromptSink`] in arrival order.
//!
//! Authority (04): webhook text is written by whoever can open an issue. The rendered template is
//! an [`Untrusted<String>`](aho_policy::Untrusted) whose only exit is a frame that labels it
//! external content without operator authority and quotes every line. The surface holds no
//! `Operator`, answers no approvals, and runs no operator commands; widening policy stays behind
//! the Approval handle like any other agent request.
//!
//! Transport: exactly surface-web's rule. HTTPS with the operator's PEM pair
//! (`surface.webhook.tls_cert` / `tls_key`; `aho cert` writes one); plain HTTP only on a loopback
//! bind and only with the explicit acknowledgment `surface.webhook.insecure_loopback = true`
//! (the CLI's `--insecure-loopback`), e.g. behind a local TLS-terminating proxy.

#![forbid(unsafe_code)]

mod config;
mod github;
mod server;
mod template;
mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_policy::Untrusted;
use aho_session::{Session, SessionError, SessionEvent};
use aho_types::BoxFuture;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use config::{DEFAULT_BIND, DEFAULT_CREDENTIAL, DEFAULT_MAX_BODY, DEFAULT_MENTION, Transport, WebhookOptions};
pub use github::{DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER, sign};
pub use server::{PATH, QUEUE_DEPTH};
pub use template::{DEFAULT_EVENTS, DEFAULT_TEMPLATES, render};

pub struct SurfaceWebhook;

impl Capability for SurfaceWebhook {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = WebhookOptions;
    fn build(_: (), _: WebhookOptions) -> Result<Self, BuildError> {
        Ok(SurfaceWebhook)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}

declare_capability!(
    SurfaceWebhook,
    "surface-webhook",
    kind = Kind::Surface,
    provides = [],
    requires = [Requirement::AnyOf(&[
        Requirement::Credential(DEFAULT_CREDENTIAL),
        Requirement::Setting { key: "surface.webhook.secret", pred: Pred::Set },
    ])],
    tools = &[],
    unlock = Fix::cli("aho credential set github-webhook"),
    doc = "GitHub webhooks → prompts on a bound session: second HTTPS listener, HMAC-SHA256 from a credential ref, dedupe by delivery id, payloads framed as untrusted external content."
);

/// One accepted delivery. The text is reachable only through [`WebhookPrompt::into_text`], which
/// frames it as untrusted external content.
pub struct WebhookPrompt {
    /// `X-GitHub-Delivery` (validated: letters, digits, `-`).
    pub delivery: String,
    /// The subscription that matched: `<event>.<action>` or `<event>`.
    pub event: String,
    body: Untrusted<String>,
}

impl WebhookPrompt {
    pub fn new(delivery: String, event: String, body: Untrusted<String>) -> WebhookPrompt {
        WebhookPrompt { delivery, event, body }
    }

    /// The session's inbox dedupe key; survives restarts because the log keeps it.
    pub fn client_key(&self) -> String {
        format!("github:{}", self.delivery)
    }

    /// The prompt text: a provenance header, then every line of the rendered template quoted.
    pub fn into_text(self) -> String {
        let from = template::Provenance { event: &self.event, delivery: &self.delivery };
        template::frame(self.body, &from)
    }
}

/// Where accepted deliveries go. The CLI passes a [`SessionSink`] over the bound session.
pub trait PromptSink: Send + Sync + 'static {
    fn deliver(&self, prompt: WebhookPrompt) -> BoxFuture<'_, Result<(), String>>;
}

/// Prompts a live session. A turn already running (another surface, or the previous delivery's
/// follow-ups) is waited out, never interrupted.
pub struct SessionSink {
    session: Arc<Session>,
}

impl SessionSink {
    pub fn new(session: Arc<Session>) -> SessionSink {
        SessionSink { session }
    }
}

/// How long to wait for an `Idle` event before trying the session again anyway.
const BUSY_RETRY: Duration = Duration::from_secs(2);

impl PromptSink for SessionSink {
    fn deliver(&self, prompt: WebhookPrompt) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let key = prompt.client_key();
            let text = prompt.into_text();
            loop {
                let mut events = self.session.subscribe();
                match self.session.prompt(text.clone(), Some(key.clone())).await {
                    Ok(_) => return Ok(()),
                    Err(SessionError::Busy) => {
                        let idle = async {
                            loop {
                                match events.recv().await {
                                    Ok(SessionEvent::Idle { .. }) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                }
                            }
                        };
                        // Either way, try again; the sleep covers an Idle that fired before we listened.
                        tokio::select! {
                            () = idle => {}
                            () = tokio::time::sleep(BUSY_RETRY) => {}
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        })
    }
}

/// A running webhook listener.
pub struct WebhookServer {
    pub addr: SocketAddr,
    pub tls: bool,
    task: JoinHandle<Result<(), String>>,
}

#[allow(clippy::disallowed_methods, reason = "authority: webhook-tls-input")]
async fn read_pem(path: &std::path::Path, what: &str) -> Result<String, String> {
    tokio::fs::read_to_string(path).await.map_err(|e| {
        format!("the webhook TLS {what} {} cannot be read ({e}); nothing was started. Point surface.webhook.tls_{what} at the file `aho cert` wrote.", path.display())
    })
}

impl WebhookServer {
    /// Binds and starts serving; returns once the listener is up.
    pub async fn start(opts: WebhookOptions, sink: Arc<dyn PromptSink>) -> Result<WebhookServer, String> {
        let bind = opts.bind;
        let tls_cfg = match &opts.transport {
            None => {
                return Err(format!(
                    "the webhook surface has no transport configured for {bind}; nothing was started. Set surface.webhook.tls_cert and surface.webhook.tls_key (`aho cert` writes a pair), or, on a loopback bind only, surface.webhook.insecure_loopback = true (--insecure-loopback)."
                ));
            }
            Some(Transport::InsecureLoopback) if !bind.ip().is_loopback() => {
                return Err(format!(
                    "refusing to serve plain HTTP on {bind}: the webhook surface is HTTPS-only off loopback. Pass a certificate and key (`aho cert` writes a pair), or bind 127.0.0.1 with --insecure-loopback for local development or behind a local TLS proxy."
                ));
            }
            Some(Transport::InsecureLoopback) => None,
            Some(Transport::Tls { cert, key }) => Some(tls::server_config(&read_pem(cert, "cert").await?, &read_pem(key, "key").await?)?),
        };
        #[allow(clippy::disallowed_methods, reason = "authority: webhook-listener")]
        let tcp = tokio::net::TcpListener::bind(bind).await.map_err(|e| format!("could not listen on {bind}: {e}"))?;
        let addr = tcp.local_addr().map_err(|e| format!("the listener has no address: {e}"))?;
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let app = Arc::new(server::AppState::new(opts, tx));
        server::spawn_worker(app.clone(), rx, sink);
        let router = server::router(app);
        let tls = tls_cfg.is_some();
        let task = match tls_cfg {
            Some(cfg) => {
                let listener = tls::TlsListener::new(tcp, cfg)?;
                let svc = router.into_make_service_with_connect_info::<tls::Peer>();
                tokio::spawn(async move { axum::serve(listener, svc).await.map_err(|e| format!("the webhook server stopped: {e}")) })
            }
            None => {
                let svc = router.into_make_service_with_connect_info::<tls::Peer>();
                tokio::spawn(async move { axum::serve(tcp, svc).await.map_err(|e| format!("the webhook server stopped: {e}")) })
            }
        };
        Ok(WebhookServer { addr, tls, task })
    }

    /// The payload URL to paste into the GitHub hook settings (host as bound).
    pub fn url(&self) -> String {
        format!("{}://{}{PATH}", if self.tls { "https" } else { "http" }, self.addr)
    }

    /// Serves until the server stops.
    pub async fn wait(self) -> Result<(), String> {
        self.task.await.map_err(|e| format!("the webhook server task failed: {e}"))?
    }

    pub fn shutdown(self) {
        self.task.abort();
    }
}

/// Serves until the process ends; prints the payload URL and subscriptions on stderr.
pub async fn serve(opts: WebhookOptions, session_sink: Arc<dyn PromptSink>) -> Result<(), String> {
    let events = opts.events.join(", ");
    let s = WebhookServer::start(opts, session_sink).await?;
    if !s.tls {
        eprintln!("aho webhook: plain HTTP on loopback ({}), for development or a local TLS proxy only", s.addr);
    }
    eprintln!("aho webhook: GitHub payload URL {} (content type application/json), events: {events}", s.url());
    s.wait().await
}
