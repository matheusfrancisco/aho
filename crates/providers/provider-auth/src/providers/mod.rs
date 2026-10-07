mod anthropic;
mod api_key;
mod codex;
mod device;
mod openrouter;
mod radius;

use crate::{AuthError, Credential, Engine, LoginKind, LoginOptions, LoginUi, Protocol, Provider, ResolvedAuth};

pub static PROVIDERS: &[Provider] = &[
    Provider {
        id: "anthropic",
        name: "Anthropic (Claude Pro/Max)",
        protocol: Protocol::AnthropicMessages,
        default_base_url: "https://api.anthropic.com",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "openai-codex",
        name: "OpenAI (ChatGPT Plus/Pro)",
        protocol: Protocol::OpenAiResponses,
        default_base_url: "https://chatgpt.com/backend-api/codex/responses",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "github-copilot",
        name: "GitHub Copilot",
        protocol: Protocol::OpenAiCompletions,
        default_base_url: "https://api.individual.githubcopilot.com",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "kimi-coding",
        name: "Kimi Coding",
        protocol: Protocol::AnthropicMessages,
        default_base_url: "https://api.kimi.com/coding",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "meta",
        name: "Meta",
        protocol: Protocol::OpenAiResponses,
        default_base_url: "https://api.meta.ai/v1/responses",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "openrouter",
        name: "OpenRouter",
        protocol: Protocol::OpenAiCompletions,
        default_base_url: "https://openrouter.ai/api/v1",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "radius",
        name: "Radius",
        protocol: Protocol::PiMessages,
        default_base_url: "https://radius.pi.dev",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "xai",
        name: "xAI",
        protocol: Protocol::OpenAiResponses,
        default_base_url: "https://api.x.ai/v1/responses",
        kind: LoginKind::OAuth,
    },
    Provider {
        id: "ollama-cloud",
        name: "Ollama Cloud",
        protocol: Protocol::OpenAiCompletions,
        default_base_url: "https://ollama.com/v1",
        kind: LoginKind::ApiKey { instructions_url: "https://ollama.com/settings/keys" },
    },
];

pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

pub(crate) async fn login(id: &str, options: &LoginOptions, ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    match id {
        "anthropic" => anthropic::login(ui, engine).await,
        "openai-codex" => codex::login(options, ui, engine).await,
        "openrouter" => openrouter::login(ui, engine).await,
        "github-copilot" | "kimi-coding" | "meta" | "xai" => device::login(id, options, ui, engine).await,
        "radius" => radius::login(options, ui, engine).await,
        "ollama-cloud" => api_key::login(ui).await,
        _ => Err(AuthError::UnsupportedProvider),
    }
}

pub(crate) async fn refresh(id: &str, credential: &Credential, engine: &Engine) -> Result<Credential, AuthError> {
    match id {
        "anthropic" => anthropic::refresh(credential, engine).await,
        "openai-codex" => codex::refresh(credential, engine).await,
        "openrouter" => Ok(credential.clone()),
        "github-copilot" | "kimi-coding" | "meta" | "xai" => device::refresh(id, credential, engine).await,
        "radius" => radius::refresh(credential, engine).await,
        "ollama-cloud" => Ok(credential.clone()),
        _ => Err(AuthError::UnsupportedProvider),
    }
}

pub(crate) fn resolve(id: &str, credential: &Credential) -> Result<ResolvedAuth, AuthError> {
    match id {
        // Pasted keys are not account grants, so no OAuth headers; openrouter keys are minted by OAuth and stay there.
        "ollama-cloud" => {
            return Ok(ResolvedAuth { token: credential.access_token.clone(), headers: Default::default(), base_url: None, oauth: false });
        }
        "github-copilot" | "kimi-coding" | "meta" | "xai" => return device::resolve(id, credential),
        "radius" => return radius::resolve(credential),
        _ => {}
    }
    let mut resolved = ResolvedAuth { token: credential.access_token.clone(), headers: Default::default(), base_url: None, oauth: true };
    match id {
        "anthropic" => {
            for (name, value) in [
                ("anthropic-beta", "claude-code-20250219,oauth-2025-04-20"),
                ("accept", "application/json"),
                ("anthropic-dangerous-direct-browser-access", "true"),
                ("user-agent", "claude-cli/2.1.280"),
                ("x-app", "cli"),
            ] {
                resolved.headers.insert(name.into(), value.into());
            }
        }
        "openai-codex" => {
            let account = credential
                .metadata
                .get("account_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or(AuthError::InvalidCredential)?;
            resolved.headers.insert("chatgpt-account-id".into(), account.into());
        }
        "openrouter" => {}
        _ => return Err(AuthError::UnsupportedProvider),
    }
    Ok(resolved)
}
