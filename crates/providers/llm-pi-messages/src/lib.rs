//! Native pi-messages gateway transport. Radius credentials are resolved for every request;
//! OAuth gateway discovery supplies the endpoint unless configuration explicitly overrides it.

#![forbid(unsafe_code)]

mod decode;
mod encode;
mod error;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Layer, Loadout, Ty};
use aho_llm::sse::SseDecoder;
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Protocol, Request, Response, Sink};
use aho_types::{BoxFuture, ContentBlock, Value};
use provider_auth::AuthSource;
use serde_json::json;

pub use decode::StreamDecoder;
pub use encode::encode_request;
pub use error::classify;

pub const PROTOCOL: Protocol = Protocol::PiMessages;
// Radius can route to backends requiring adjacent results, including Anthropic.
pub const LATE_RESULTS: LateResults = LateResults::AsUserBlock;
pub const ROUTE: &str = "radius";
pub const BASE_URL: &str = "https://radius.pi.dev";

#[derive(Clone, Debug)]
pub struct RadiusConfig {
    pub base_url: String,
    auth: AuthSource,
    base_url_override: bool,
}

impl RadiusConfig {
    /// Explicit endpoint, suitable for a configured gateway or a local transport fixture.
    pub fn new(base_url: &str, key: &str) -> Self {
        Self { base_url: base_url.trim_end_matches('/').to_string(), auth: AuthSource::from_key(ROUTE, key), base_url_override: true }
    }
}

fn secret(loadout: &Loadout, key: &str) -> Result<Option<String>, ConfigError> {
    let raw = match loadout.secrets.get(key) {
        Some(value) => value.expose().to_string(),
        None => match loadout.get(key) {
            None => return Ok(None),
            Some(Value::String(value)) => value.clone(),
            Some(_) => return Err(ConfigError::Type { key: key.into(), expected: "a string" }),
        },
    };
    match raw.strip_prefix("cred:") {
        Some(name) => {
            let target = format!("credentials.{name}");
            secret(loadout, &target)?.map(Some).ok_or(ConfigError::MissingKey(target))
        }
        None if raw.is_empty() => Ok(None),
        None => Ok(Some(raw)),
    }
}

impl Config for RadiusConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("provider.radius.key", Ty::String, None, "Radius API key or OAuth credential reference (`aho login radius`).")
                .secret(),
            ConfigKey::new(
                "provider.radius.base_url",
                Ty::String,
                Some(json!(BASE_URL)),
                "Radius pi-messages gateway; OAuth discovery supplies the default endpoint.",
            ),
            ConfigKey::new(
                "provider.radius.protocol",
                Ty::String,
                Some(json!(PROTOCOL.tag())),
                "Radius native protocol (`pi-messages`), not OpenAI chat completions.",
            ),
        ]
    }

    fn from_loadout(loadout: &Loadout) -> Result<Self, ConfigError> {
        let key = secret(loadout, "provider.radius.key")?.ok_or_else(|| ConfigError::MissingKey("provider.radius.key".into()))?;
        if let Some(protocol) = loadout.get("provider.radius.protocol")
            && protocol.as_str() != Some(PROTOCOL.tag())
        {
            return Err(ConfigError::Type { key: "provider.radius.protocol".into(), expected: "pi-messages" });
        }
        let base = match loadout.get("provider.radius.base_url") {
            Some(Value::String(value)) => value.as_str(),
            None => BASE_URL,
            Some(_) => return Err(ConfigError::Type { key: "provider.radius.base_url".into(), expected: "a URL string" }),
        };
        let mut config = Self::new(base, &key);
        config.base_url_override = loadout.config.get("provider.radius.base_url").is_some_and(|value| value.winner.layer != Layer::Default);
        Ok(config)
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: llm-pi-messages-egress")]
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(600))
        .build()
        .map_err(|_| "could not initialise the HTTP client; the radius route is unavailable".into())
}

pub struct PiMessagesRoute {
    cfg: RadiusConfig,
    client: reqwest::Client,
}

impl PiMessagesRoute {
    pub fn new(cfg: RadiusConfig, client: reqwest::Client) -> Self {
        Self { cfg, client }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: llm-pi-messages-send")]
    async fn run(&self, req: Request, mut sink: Sink<'_>) -> Result<Response, LlmError> {
        let auth = self.cfg.auth.resolve(&self.client).await.map_err(|error| LlmError::Auth(error.to_string()))?;
        let base_url = if self.cfg.base_url_override { &self.cfg.base_url } else { auth.base_url.as_ref().unwrap_or(&self.cfg.base_url) };
        let has_images = req.messages.iter().any(|message| message.content.iter().any(has_image));
        let body = encode_request(&req);
        let mut request = self
            .client
            .post(format!("{}/messages", base_url.trim_end_matches('/')))
            .bearer_auth(&auth.token)
            .header("accept", "text/event-stream")
            .json(&body);
        for (key, value) in &auth.headers {
            request = request.header(key, value);
        }
        let mut response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            let retry = aho_llm::retry_after_ms(None, response.headers().get("retry-after").and_then(|value| value.to_str().ok()));
            let body = response.text().await.map_err(transport_error)?;
            return Err(classify(Some(status.as_u16()), retry, &body, has_images));
        }
        let mut sse = SseDecoder::new();
        let mut decoder = StreamDecoder::new(&req.model).with_images(has_images);
        while !decoder.is_done() {
            match response.chunk().await.map_err(transport_error)? {
                Some(bytes) => {
                    for event in sse.push(&bytes) {
                        decoder.on_event(&event.data, &mut sink)?;
                        if decoder.is_done() {
                            break;
                        }
                    }
                }
                None => {
                    if let Some(event) = sse.finish() {
                        decoder.on_event(&event.data, &mut sink)?;
                    }
                    break;
                }
            }
        }
        decoder.finish()
    }
}

fn has_image(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Image { .. } => true,
        ContentBlock::ToolResult { content, .. } => content.iter().any(has_image),
        _ => false,
    }
}

fn transport_error(error: reqwest::Error) -> LlmError {
    // reqwest's Display contains the URL, which can carry private gateway parameters.
    if error.is_builder() {
        LlmError::Fatal("Radius endpoint or credential headers are invalid; check `provider.radius.base_url` and log in again".into())
    } else if error.is_timeout() {
        LlmError::Transient("Radius gateway sent no data before the request timeout".into())
    } else if error.is_connect() {
        LlmError::Transient("could not connect to the Radius gateway; check the network and `provider.radius.base_url`".into())
    } else {
        LlmError::Transient("Radius gateway connection failed".into())
    }
}

impl Llm for PiMessagesRoute {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { protocol: PROTOCOL, late_results: LATE_RESULTS, route: ROUTE.into() }
    }

    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(self.run(req, sink))
    }
}

pub struct PiMessages {
    route: Arc<PiMessagesRoute>,
}

impl Capability for PiMessages {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = RadiusConfig;

    fn build(_: (), cfg: RadiusConfig) -> Result<Self, BuildError> {
        Ok(Self { route: Arc::new(PiMessagesRoute::new(cfg, http_client().map_err(BuildError::Probe)?)) })
    }

    fn register(&self, registrar: &mut Registrar<'_>) {
        registrar.service(aho_llm::service_name(ROUTE), LlmRoute(self.route.clone()));
    }
}

declare_capability!(
    PiMessages,
    "llm-pi-messages",
    kind = Kind::Provider,
    provides = [SeamId(aho_llm::SEAM)],
    requires = [Requirement::Credential("radius")],
    tools = &[],
    doc = "Radius native pi-messages protocol; live OAuth resolution, native replay metadata, and tool results."
);
