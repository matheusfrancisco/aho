//! Responses streaming for Codex, Meta, xAI and protocol-compatible routes.
#![forbid(unsafe_code)]

mod config;
mod encode;
mod error;
mod stream;
mod transport;

use std::sync::Arc;

use aho_cap::*;
use aho_llm::{LateResults, LlmRoute, Protocol};

pub use config::{CODEX_BASE_URL, META_BASE_URL, ResponsesConfig, RouteConfig, XAI_BASE_URL};
pub use encode::{Flavor, encode_request};
pub use error::classify;
pub use stream::StreamDecoder;
pub use transport::{ResponsesRoute, http_client};

pub const PROTOCOL: Protocol = Protocol::OpenAiResponses;
pub const LATE_RESULTS: LateResults = LateResults::SameCallId;

pub struct OpenAiResponses {
    routes: Vec<Arc<ResponsesRoute>>,
}

impl Capability for OpenAiResponses {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ResponsesConfig;

    fn build(_: (), cfg: ResponsesConfig) -> Result<Self, BuildError> {
        if cfg.routes.is_empty() {
            return Err(BuildError::Probe(
                "no Responses route is configured; use `aho login openai-codex`, `aho login meta`, `aho login xai`, or configure a provider.<name> row with protocol = \"openai-responses\" and a base_url".into(),
            ));
        }
        let client = http_client().map_err(BuildError::Probe)?;
        Ok(Self { routes: cfg.routes.into_iter().map(|cfg| Arc::new(ResponsesRoute::new(cfg, client.clone()))).collect() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for route in &self.routes {
            r.service(aho_llm::service_name(route.name()), LlmRoute(route.clone()));
        }
    }
}

declare_capability!(
    OpenAiResponses,
    "llm-openai-responses",
    kind = Kind::Provider,
    provides = [SeamId(aho_llm::SEAM)],
    requires = [Requirement::AnyOf(&[
        Requirement::Credential("openai-codex"),
        Requirement::Credential("meta"),
        Requirement::Credential("xai"),
        Requirement::Setting { key: "provider.responses.enabled", pred: Pred::True },
    ])],
    tools = &[],
    doc = "OpenAI Responses protocol: Codex, Meta, xAI and custom openai-responses rows; late results retain the original call id."
);
