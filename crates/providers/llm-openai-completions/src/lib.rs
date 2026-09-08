//! Protocol-keyed adapter (01, 16): any vendor speaking chat-completions is a config row here.
//!
//! Routes, each registered as the service `llm:<name>`:
//! * `openai` — when `provider.openai.key` resolves; `provider.openai.base_url` defaults to
//!   `https://api.openai.com/v1`.
//! * `ollama` — built in, keyless, at `http://localhost:11434/v1`; on when `provider.ollama.enabled = true`.
//! * any `provider.<name>` row with `protocol = "openai-completions"` and a `base_url`
//!   (`key` optional; `cred:<n>` reads `credentials.<n>`).
//!
//! Named `openrouter` and `github-copilot` routes activate independently of OpenAI.
//! Copilot dispatches each model to its native Messages, Responses, or completions endpoint.

#![forbid(unsafe_code)]

mod copilot;
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

pub use wire::{Flavor, StreamDecoder, classify, encode_request};

pub const PROTOCOL: Protocol = Protocol::OpenAiCompletions;
/// `16`: a late result is a second `tool` message with the original `tool_call_id`.
pub const LATE_RESULTS: LateResults = LateResults::ToolMessage;
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const COPILOT_BASE_URL: &str = llm_anthropic_messages::COPILOT_BASE_URL;

/// Idle time allowed between streamed bytes; local models may take minutes on a long prompt.
const READ_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// One configured endpoint. `Debug` never prints the key.
#[derive(Clone)]
pub struct RouteConfig {
    pub name: String,
    pub base_url: String,
    auth: Option<AuthSource>,
    pub flavor: Flavor,
}

impl RouteConfig {
    pub fn new(name: &str, base_url: &str, key: Option<String>, flavor: Flavor) -> RouteConfig {
        RouteConfig {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: key.as_deref().map(|value| AuthSource::from_key(name, value)),
            flavor,
        }
    }
    pub fn has_key(&self) -> bool {
        self.auth.is_some()
    }
}

impl std::fmt::Debug for RouteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("flavor", &self.flavor)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct CompletionsConfig {
    pub routes: Vec<RouteConfig>,
}

/// Secrets resolve from the loadout's secret store; undeclared keys (custom rows) fall back to
/// their plain value. `cred:<n>` indirects through `credentials.<n>`.
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

fn url(l: &Loadout, key: &str, default: Option<&str>) -> Result<Option<String>, ConfigError> {
    match l.get(key) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ConfigError::Type { key: key.to_string(), expected: "a URL string" }),
        None => Ok(default.map(str::to_string)),
    }
}

impl Config for CompletionsConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "provider.openai.key",
                Ty::String,
                None,
                "OpenAI API key; enables the `openai` route (`aho credential set openai`).",
            )
            .secret(),
            ConfigKey::new(
                "provider.openai.base_url",
                Ty::String,
                Some(json!(OPENAI_BASE_URL)),
                "Endpoint of the `openai` route (chat completions).",
            ),
            ConfigKey::new(
                "provider.ollama.enabled",
                Ty::Bool,
                Some(json!(false)),
                "Enable the keyless `ollama` route (local Ollama server).",
            ),
            ConfigKey::new("provider.ollama.base_url", Ty::String, Some(json!(OLLAMA_BASE_URL)), "Endpoint of the `ollama` route."),
            ConfigKey::new("provider.ollama.key", Ty::String, None, "Optional bearer token for an `ollama` route behind a proxy.").secret(),
            ConfigKey::new("provider.openrouter.key", Ty::String, None, "OpenRouter API key or OAuth credential reference.").secret(),
            ConfigKey::new(
                "provider.openrouter.base_url",
                Ty::String,
                Some(json!(OPENROUTER_BASE_URL)),
                "OpenRouter completions endpoint.",
            ),
            ConfigKey::new("provider.github-copilot.key", Ty::String, None, "GitHub Copilot token or OAuth credential reference.").secret(),
            ConfigKey::new(
                "provider.github-copilot.base_url",
                Ty::String,
                Some(json!(COPILOT_BASE_URL)),
                "Copilot endpoint; the default follows the signed-in account.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let mut routes = Vec::new();
        if let Some(key) = secret(l, "provider.openai.key")? {
            let base = url(l, "provider.openai.base_url", Some(OPENAI_BASE_URL))?.unwrap_or_else(|| OPENAI_BASE_URL.to_string());
            routes.push(RouteConfig::new("openai", &base, Some(key), Flavor::OpenAi));
        }
        if l.get("provider.ollama.enabled") == Some(&Value::Bool(true)) {
            let base = url(l, "provider.ollama.base_url", Some(OLLAMA_BASE_URL))?.unwrap_or_else(|| OLLAMA_BASE_URL.to_string());
            routes.push(RouteConfig::new("ollama", &base, secret(l, "provider.ollama.key")?, Flavor::Compatible));
        }
        for (name, default) in [("openrouter", OPENROUTER_BASE_URL), ("github-copilot", COPILOT_BASE_URL)] {
            if let Some(key) = secret(l, &format!("provider.{name}.key"))? {
                let base = url(l, &format!("provider.{name}.base_url"), Some(default))?.unwrap_or_else(|| default.to_string());
                routes.push(RouteConfig::new(name, &base, Some(key), Flavor::Compatible));
            }
        }
        for (name, row) in l.table("provider") {
            if matches!(name.as_str(), "openai" | "ollama" | "openrouter" | "github-copilot")
                || row["protocol"].as_str() != Some(PROTOCOL.tag())
            {
                continue;
            }
            let base_key = format!("provider.{name}.base_url");
            let Some(base) = url(l, &base_key, None)? else { return Err(ConfigError::MissingKey(base_key)) };
            routes.push(RouteConfig::new(&name, &base, secret(l, &format!("provider.{name}.key"))?, Flavor::Compatible));
        }
        Ok(CompletionsConfig { routes })
    }
}

/// A single chat-completions endpoint: the `Llm` behind one `llm:<name>` service.
pub struct CompletionsRoute {
    cfg: RouteConfig,
    client: reqwest::Client,
    copilot: Option<copilot::Routes>,
}

#[allow(clippy::disallowed_methods, reason = "authority: llm-completions-egress")]
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("could not initialise the HTTP client ({e}); model routes are unavailable"))
}

impl CompletionsRoute {
    pub fn new(cfg: RouteConfig, client: reqwest::Client) -> CompletionsRoute {
        let copilot = (cfg.name == "github-copilot").then(|| copilot::Routes::new(&cfg, &client));
        CompletionsRoute { cfg, client, copilot }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: llm-completions-send")]
    async fn run(&self, req: Request, mut sink: Sink<'_>) -> Result<Response, LlmError> {
        if let Some(routes) = &self.copilot {
            match copilot::protocol(&req.model) {
                Protocol::AnthropicMessages => {
                    let messages = routes
                        .messages
                        .as_ref()
                        .ok_or_else(|| LlmError::Auth("GitHub Copilot requires a credential; run `aho login github-copilot`".into()))?;
                    return messages.stream(req, sink).await;
                }
                Protocol::OpenAiResponses => return routes.responses.stream(req, sink).await,
                _ => {}
            }
        }
        let name = &self.cfg.name;
        let has_images = req.messages.iter().flat_map(|m| &m.content).any(|b| match b {
            ContentBlock::Image { .. } => true,
            ContentBlock::ToolResult { content, .. } => content.iter().any(|c| matches!(c, ContentBlock::Image { .. })),
            _ => false,
        });
        let auth = match &self.cfg.auth {
            Some(source) => Some(source.resolve(&self.client).await.map_err(|e| LlmError::Auth(e.to_string()))?),
            None => None,
        };
        let base = if name == "github-copilot" && self.cfg.base_url == COPILOT_BASE_URL {
            auth.as_ref().and_then(|a| a.base_url.as_deref()).unwrap_or(&self.cfg.base_url)
        } else {
            &self.cfg.base_url
        };
        let mut body = encode_request(&req, self.cfg.flavor);
        if name == "github-copilot"
            && let Some(body) = body.as_object_mut()
            && let Some(max_tokens) = body.remove("max_tokens")
        {
            body.insert("max_completion_tokens".into(), max_tokens);
        }
        let mut rb = self.client.post(format!("{}/chat/completions", base.trim_end_matches('/'))).json(&body);
        if name == "github-copilot" {
            rb = llm_anthropic_messages::copilot_headers(rb, &req, has_images, auth.as_ref().is_some_and(|a| a.oauth));
        }
        if let Some(auth) = &auth {
            rb = rb.bearer_auth(&auth.token);
            for (key, value) in &auth.headers {
                rb = rb.header(key.as_str(), value.as_str());
            }
        }
        let mut resp = rb.send().await.map_err(|e| transport_error(&self.cfg, &e))?;
        let status = resp.status();
        if !status.is_success() {
            let header = |h: &str| resp.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
            let retry = aho_llm::retry_after_ms(header("retry-after-ms").as_deref(), header("retry-after").as_deref());
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => format!("(the error body could not be read: {e})"),
            };
            return Err(classify(Some(status.as_u16()), retry, &text, has_images, name));
        }
        let mut sse = SseDecoder::new();
        let mut decoder = StreamDecoder::new(name);
        while !decoder.is_done() {
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    for ev in sse.push(&bytes) {
                        decoder.on_data(&ev.data, &mut sink)?;
                    }
                }
                Ok(None) => {
                    if let Some(ev) = sse.finish() {
                        decoder.on_data(&ev.data, &mut sink)?;
                    }
                    break;
                }
                Err(e) => return Err(transport_error(&self.cfg, &e)),
            }
        }
        decoder.finish(&mut sink)
    }
}

fn transport_error(cfg: &RouteConfig, e: &reqwest::Error) -> LlmError {
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
            "could not connect to route `{name}` at {} ({e}); check the server is running or change `provider.{name}.base_url`",
            cfg.base_url
        ));
    }
    LlmError::Transient(format!("route `{name}` at {}: the connection failed ({e})", cfg.base_url))
}

impl Llm for CompletionsRoute {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { protocol: PROTOCOL, late_results: LATE_RESULTS, route: self.cfg.name.clone() }
    }

    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(self.run(req, sink))
    }
}

pub struct OpenAiCompletions {
    routes: Vec<Arc<CompletionsRoute>>,
}

impl Capability for OpenAiCompletions {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = CompletionsConfig;

    fn build(_: (), cfg: CompletionsConfig) -> Result<Self, BuildError> {
        if cfg.routes.is_empty() {
            return Err(BuildError::Probe(
                "no chat-completions route is configured; set an OpenAI key (`aho credential set openai`), enable Ollama (`aho set provider.ollama.enabled true`), or add a `provider.<name>` row with `protocol = \"openai-completions\"`".into(),
            ));
        }
        let client = http_client().map_err(BuildError::Probe)?;
        Ok(OpenAiCompletions { routes: cfg.routes.into_iter().map(|c| Arc::new(CompletionsRoute::new(c, client.clone()))).collect() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for route in &self.routes {
            r.service(aho_llm::service_name(&route.cfg.name), LlmRoute(route.clone()));
        }
    }
}

declare_capability!(
    OpenAiCompletions,
    "llm-openai-completions",
    kind = Kind::Provider,
    provides = [SeamId(aho_llm::SEAM)],
    requires = [Requirement::AnyOf(&[
        Requirement::Credential("openai"),
        Requirement::Credential("openrouter"),
        Requirement::Credential("github-copilot"),
        Requirement::Setting { key: "provider.ollama.enabled", pred: Pred::True },
    ])],
    tools = &[],
    doc = "Chat-completions routes `openai`, `ollama`, `openrouter`, custom protocol rows, and per-model native API dispatch for `github-copilot`; OAuth-aware auth."
);
