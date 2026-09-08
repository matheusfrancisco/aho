use std::time::Duration;

use aho_llm::sse::SseDecoder;
use aho_llm::{AdapterInfo, Llm, LlmError, Request, Response, Sink};
use aho_types::{BoxFuture, ContentBlock, Role};

use crate::config::COPILOT_BASE_URL;
use crate::{Flavor, LATE_RESULTS, PROTOCOL, RouteConfig, StreamDecoder, classify, encode_request};

pub struct ResponsesRoute {
    cfg: RouteConfig,
    client: reqwest::Client,
}

#[allow(clippy::disallowed_methods, reason = "authority: llm-responses-egress")]
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(600))
        .build()
        .map_err(|_| "could not initialise the Responses HTTP client".to_string())
}

impl ResponsesRoute {
    pub fn new(cfg: RouteConfig, client: reqwest::Client) -> Self {
        Self { cfg, client }
    }

    pub(crate) fn name(&self) -> &str {
        &self.cfg.name
    }

    #[allow(clippy::disallowed_methods, reason = "authority: llm-responses-send")]
    async fn run(&self, req: Request, mut sink: Sink<'_>) -> Result<Response, LlmError> {
        let auth = match &self.cfg.auth {
            Some(source) => Some(source.resolve(&self.client).await.map_err(|error| LlmError::Auth(error.to_string()))?),
            None => None,
        };
        let base = if self.cfg.flavor == Flavor::Copilot && self.cfg.base_url == COPILOT_BASE_URL {
            auth.as_ref().and_then(|resolved| resolved.base_url.as_deref()).unwrap_or(&self.cfg.base_url)
        } else {
            &self.cfg.base_url
        };
        let name = &self.cfg.name;
        let has_images = req.messages.iter().any(|message| message.content.iter().any(has_image));
        let body = encode_request(&req, self.cfg.flavor, name);
        let mut request = self.client.post(endpoint(base, self.cfg.flavor)).json(&body).header("accept", "text/event-stream");
        if self.cfg.flavor == Flavor::Codex {
            request = request
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "aho")
                .header("user-agent", concat!("aho/", env!("CARGO_PKG_VERSION")));
        }
        if self.cfg.flavor == Flavor::Copilot {
            let agent = req.messages.last().is_some_and(|message| {
                message.role != Role::User
                    || (message.content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
                        && message
                            .content
                            .iter()
                            .all(|block| matches!(block, ContentBlock::ToolResult { .. } | ContentBlock::ProviderMetadata { .. })))
            });
            request = request.header("X-Initiator", if agent { "agent" } else { "user" }).header("Openai-Intent", "conversation-edits");
            if has_images {
                request = request.header("Copilot-Vision-Request", "true");
            }
        }
        if let Some(key) = &req.cache_key {
            request = request
                .header(if self.cfg.flavor == Flavor::Codex { "session-id" } else { "session_id" }, key)
                .header("x-client-request-id", key);
        }
        if let Some(auth) = auth {
            request = request.bearer_auth(&auth.token);
            for (name, value) in auth.headers {
                request = request.header(name, value);
            }
        }
        let mut response = request.send().await.map_err(|error| transport_error(name, &error))?;
        let status = response.status();
        if !status.is_success() {
            let header = |name| response.headers().get(name).and_then(|value| value.to_str().ok());
            let retry = aho_llm::retry_after_ms(header("retry-after-ms"), header("retry-after"));
            let body = response.text().await.map_err(|error| transport_error(name, &error))?;
            return Err(classify(Some(status.as_u16()), retry, &body, has_images, name));
        }
        let mut sse = SseDecoder::new();
        let mut decoder = StreamDecoder::with_model(name, &req.model);
        while !decoder.is_done() {
            match response.chunk().await.map_err(|error| transport_error(name, &error))? {
                Some(bytes) => {
                    for event in sse.push(&bytes) {
                        decoder.on_data(&event.data, &mut sink)?;
                        if decoder.is_done() {
                            break;
                        }
                    }
                }
                None => {
                    if let Some(event) = sse.finish() {
                        decoder.on_data(&event.data, &mut sink)?;
                    }
                    break;
                }
            }
        }
        decoder.finish(&mut sink)
    }
}

fn has_image(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Image { .. } => true,
        ContentBlock::ToolResult { content, .. } => content.iter().any(has_image),
        _ => false,
    }
}

fn endpoint(base: &str, flavor: Flavor) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else if flavor == Flavor::Codex && !base.ends_with("/codex") {
        format!("{base}/codex/responses")
    } else {
        format!("{base}/responses")
    }
}

fn transport_error(route: &str, error: &reqwest::Error) -> LlmError {
    // Do not format reqwest::Error: it may include a credential-bearing URL.
    if error.is_builder() {
        LlmError::Fatal(format!("route `{route}` has an invalid endpoint or HTTP header; check provider.{route}.base_url and credentials"))
    } else if error.is_timeout() {
        LlmError::Transient(format!("route `{route}` timed out waiting for the Responses server"))
    } else {
        LlmError::Transient(format!("route `{route}` could not complete the Responses connection"))
    }
}

impl Llm for ResponsesRoute {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { protocol: PROTOCOL, late_results: LATE_RESULTS, route: self.cfg.name.clone() }
    }

    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(self.run(req, sink))
    }
}
