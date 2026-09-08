//! Copilot's native endpoint rules mirror pi's model catalog generator.

use aho_llm::Protocol;
use llm_anthropic_messages::{AnthropicConfig, MessagesRoute};
use llm_openai_responses::{Flavor, ResponsesRoute, RouteConfig};

pub(crate) struct Routes {
    pub messages: Option<MessagesRoute>,
    pub responses: ResponsesRoute,
}

impl Routes {
    pub fn new(cfg: &super::RouteConfig, client: &reqwest::Client) -> Self {
        let messages =
            cfg.auth.clone().map(|auth| MessagesRoute::new(AnthropicConfig::with_auth(&cfg.name, &cfg.base_url, auth), client.clone()));
        let responses =
            ResponsesRoute::new(RouteConfig::with_auth(&cfg.name, &cfg.base_url, cfg.auth.clone(), Flavor::Copilot), client.clone());
        Self { messages, responses }
    }
}

pub(crate) fn protocol(model: &str) -> Protocol {
    let claude = ["claude-haiku-", "claude-sonnet-", "claude-opus-", "claude-fable-"]
        .iter()
        .filter_map(|prefix| model.strip_prefix(*prefix))
        .any(|version| {
            matches!(version.as_bytes().first(), Some(b'4' | b'5'))
                && (version.len() == 1 || matches!(version.as_bytes().get(1), Some(b'.' | b'-')))
        });
    if claude {
        Protocol::AnthropicMessages
    } else if ["gpt-", "grok-", "oswe", "mai-"].iter().any(|prefix| model.starts_with(*prefix)) {
        Protocol::OpenAiResponses
    } else {
        Protocol::OpenAiCompletions
    }
}
