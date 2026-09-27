//! Messages protocol routes for Anthropic and Kimi Coding. OAuth credentials resolve before
//! each request; Claude subscription requests use the Claude Code identity and tool conventions.
//! Results must follow their `tool_use` directly (`LateResults::AsUserBlock`).

#![forbid(unsafe_code)]

mod wire;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_llm::sse::SseDecoder;
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Protocol, Request, Response, Sink};
use aho_types::{BoxFuture, ContentBlock, Value};
use provider_auth::AuthSource;
use serde_json::json;

pub use wire::{ANTHROPIC_VERSION, DEFAULT_MAX_TOKENS, StreamDecoder, classify, encode_request};

pub const PROTOCOL: Protocol = Protocol::AnthropicMessages;
pub const LATE_RESULTS: LateResults = LateResults::AsUserBlock;
pub const ROUTE: &str = "anthropic";
pub const BASE_URL: &str = "https://api.anthropic.com";
pub const KIMI_BASE_URL: &str = "https://api.kimi.com/coding";
pub const COPILOT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

/// Idle time allowed between streamed bytes (long thinking phases stream pings).
const READ_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `Debug` never prints the key.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    name: String,
    auth: AuthSource,
}

impl AnthropicConfig {
    pub fn new(base_url: &str, key: &str) -> AnthropicConfig {
        Self::for_provider(ROUTE, base_url, key)
    }

    pub fn for_provider(provider: &str, base_url: &str, key: &str) -> Self {
        Self::with_auth(provider, base_url, AuthSource::from_key(provider, key))
    }

    pub fn with_auth(provider: &str, base_url: &str, auth: AuthSource) -> Self {
        Self { base_url: base_url.trim_end_matches('/').to_string(), name: provider.to_string(), auth }
    }
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig").field("name", &self.name).field("base_url", &self.base_url).field("auth", &self.auth).finish()
    }
}

/// `cred:<n>` indirects through `credentials.<n>`.
fn secret(l: &Loadout, key: &str) -> Result<Option<String>, ConfigError> {
    let raw = match l.secrets.get(key) {
        Some(s) => s.expose().to_string(),
        None => match l.get(key) {
            None => return Ok(None),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(ConfigError::Type { key: key.to_string(), expected: "a string" }),
        },
    };
    match raw.strip_prefix("cred:") {
        Some(name) => {
            let target = format!("credentials.{name}");
            match secret(l, &target)? {
                Some(v) => Ok(Some(v)),
                None => Err(ConfigError::MissingKey(target)),
            }
        }
        None if raw.is_empty() => Ok(None),
        None => Ok(Some(raw)),
    }
}

impl Config for AnthropicConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "provider.anthropic.key",
                Ty::String,
                None,
                "Anthropic API key or OAuth credential reference; enables `anthropic` (`aho login anthropic` or `aho credential set anthropic`).",
            )
            .secret(),
            ConfigKey::new(
                "provider.anthropic.base_url",
                Ty::String,
                Some(json!(BASE_URL)),
                "Endpoint of the `anthropic` route (Messages API).",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let key = secret(l, "provider.anthropic.key")?.ok_or_else(|| ConfigError::MissingKey("provider.anthropic.key".into()))?;
        let base = match l.get("provider.anthropic.base_url") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(ConfigError::Type { key: "provider.anthropic.base_url".into(), expected: "a URL string" }),
            None => BASE_URL.to_string(),
        };
        Ok(AnthropicConfig::new(&base, &key))
    }
}

#[derive(Clone, Debug)]
pub struct MessagesConfig {
    pub routes: Vec<AnthropicConfig>,
}

impl Config for MessagesConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = AnthropicConfig::keys();
        keys.push(
            ConfigKey::new("provider.kimi-coding.key", Ty::String, None, "Kimi Coding API key or OAuth credential reference.").secret(),
        );
        keys.push(ConfigKey::new(
            "provider.kimi-coding.base_url",
            Ty::String,
            Some(json!(KIMI_BASE_URL)),
            "Kimi Coding Messages endpoint.",
        ));
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let mut routes = Vec::new();
        for (name, default) in [(ROUTE, BASE_URL), ("kimi-coding", KIMI_BASE_URL)] {
            let Some(key) = secret(l, &format!("provider.{name}.key"))? else { continue };
            let base_key = format!("provider.{name}.base_url");
            let base = match l.get(&base_key) {
                Some(Value::String(s)) => s.as_str(),
                Some(_) => return Err(ConfigError::Type { key: base_key, expected: "a URL string" }),
                None => default,
            };
            routes.push(AnthropicConfig::for_provider(name, base, &key));
        }
        Ok(Self { routes })
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: llm-anthropic-egress")]
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("could not initialise the HTTP client ({e}); the anthropic route is unavailable"))
}

/// The `Llm` behind `llm:anthropic`.
pub struct MessagesRoute {
    cfg: AnthropicConfig,
    client: reqwest::Client,
}

impl MessagesRoute {
    pub fn new(cfg: AnthropicConfig, client: reqwest::Client) -> MessagesRoute {
        MessagesRoute { cfg, client }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: llm-anthropic-send")]
    async fn run(&self, req: Request, mut sink: Sink<'_>) -> Result<Response, LlmError> {
        let has_images = req.messages.iter().flat_map(|m| &m.content).any(|b| match b {
            ContentBlock::Image { .. } => true,
            ContentBlock::ToolResult { content, .. } => content.iter().any(|c| matches!(c, ContentBlock::Image { .. })),
            _ => false,
        });
        let name = &self.cfg.name;
        let auth = self.cfg.auth.resolve(&self.client).await.map_err(|e| LlmError::Auth(e.to_string()))?;
        let claude_oauth = name == ROUTE && auth.oauth;
        let body = wire::encode_authenticated_request(&req, claude_oauth);
        let base = if name == "github-copilot" && self.cfg.base_url == COPILOT_BASE_URL {
            auth.base_url.as_deref().unwrap_or(&self.cfg.base_url)
        } else {
            &self.cfg.base_url
        };
        let endpoint = if claude_oauth { "/v1/messages?beta=true" } else { "/v1/messages" };
        let mut rb = self
            .client
            .post(format!("{}{endpoint}", base.trim_end_matches('/')))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "application/json")
            .json(&body);
        rb = if auth.oauth || name == "github-copilot" { rb.bearer_auth(&auth.token) } else { rb.header("x-api-key", &auth.token) };
        if name == "github-copilot" {
            rb = copilot_headers(rb, &req, has_images, auth.oauth).header("anthropic-dangerous-direct-browser-access", "true");
        }
        for (key, value) in &auth.headers {
            rb = rb.header(key.as_str(), value.as_str());
        }
        let mut resp = rb.send().await.map_err(|e| transport_error(&self.cfg, &e))?;
        let status = resp.status();
        if !status.is_success() {
            let retry = aho_llm::retry_after_ms(None, resp.headers().get("retry-after").and_then(|v| v.to_str().ok()));
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => format!("(the error body could not be read: {e})"),
            };
            return Err(classify(Some(status.as_u16()), retry, &text, has_images, name));
        }
        let mut sse = SseDecoder::new();
        let mut decoder = StreamDecoder::new(name);
        let oauth_tools = if claude_oauth { req.tools.as_slice() } else { &[] };
        while !decoder.is_done() {
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    for ev in sse.push(&bytes) {
                        decoder.on_authenticated_event(ev.event.as_deref(), &ev.data, &mut sink, oauth_tools)?;
                    }
                }
                Ok(None) => {
                    if let Some(ev) = sse.finish() {
                        decoder.on_authenticated_event(ev.event.as_deref(), &ev.data, &mut sink, oauth_tools)?;
                    }
                    break;
                }
                Err(e) => return Err(transport_error(&self.cfg, &e)),
            }
        }
        decoder.finish(&mut sink)
    }
}

/// Copilot identifies editor requests and distinguishes user turns from autonomous tool followups.
pub fn copilot_headers(mut rb: reqwest::RequestBuilder, req: &Request, has_images: bool, oauth: bool) -> reqwest::RequestBuilder {
    let agent = req.messages.last().is_some_and(|m| {
        m.role != aho_types::Role::User
            || (!m.content.is_empty()
                && m.content.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. } | ContentBlock::ProviderMetadata { .. })))
    });
    let initiator = if agent { "agent" } else { "user" };
    if !oauth {
        rb = rb
            .header("user-agent", "GitHubCopilotChat/0.35.0")
            .header("editor-version", "vscode/1.107.0")
            .header("editor-plugin-version", "copilot-chat/0.35.0")
            .header("copilot-integration-id", "vscode-chat");
    }
    rb = rb.header("x-initiator", initiator).header("openai-intent", "conversation-edits");
    if has_images {
        rb = rb.header("copilot-vision-request", "true");
    }
    rb
}

fn transport_error(cfg: &AnthropicConfig, e: &reqwest::Error) -> LlmError {
    let name = &cfg.name;
    if e.is_builder() {
        return LlmError::Fatal(format!(
            "`provider.{name}.base_url` = {} is not a usable URL ({e}); fix it with `aho set provider.{name}.base_url <url>`",
            cfg.base_url
        ));
    }
    if e.is_timeout() {
        return LlmError::Transient(format!("route `{name}` at {} sent nothing for too long ({e})", cfg.base_url));
    }
    if e.is_connect() {
        return LlmError::Transient(format!(
            "could not connect to route `{name}` at {} ({e}); check the network or change `provider.{name}.base_url`",
            cfg.base_url
        ));
    }
    LlmError::Transient(format!("route `{name}` at {}: the connection failed ({e})", cfg.base_url))
}

impl Llm for MessagesRoute {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { protocol: PROTOCOL, late_results: LATE_RESULTS, route: self.cfg.name.clone() }
    }

    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(self.run(req, sink))
    }
}

pub struct AnthropicMessages {
    routes: Vec<Arc<MessagesRoute>>,
}

impl Capability for AnthropicMessages {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = MessagesConfig;

    fn build(_: (), cfg: MessagesConfig) -> Result<Self, BuildError> {
        let client = http_client().map_err(BuildError::Probe)?;
        Ok(AnthropicMessages { routes: cfg.routes.into_iter().map(|c| Arc::new(MessagesRoute::new(c, client.clone()))).collect() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for route in &self.routes {
            r.service(aho_llm::service_name(&route.cfg.name), LlmRoute(route.clone()));
        }
    }
}

declare_capability!(
    AnthropicMessages,
    "llm-anthropic-messages",
    kind = Kind::Provider,
    provides = [SeamId(aho_llm::SEAM)],
    requires = [Requirement::AnyOf(&[Requirement::Credential("anthropic"), Requirement::Credential("kimi-coding")])],
    tools = &[],
    doc = "Anthropic Messages protocol: routes `anthropic` and `kimi-coding`; OAuth-aware auth and prompt caching; late results as user blocks."
);
