use aho_cap::{Config, ConfigError};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_types::Value;
use provider_auth::AuthSource;
use serde_json::json;

use crate::{Flavor, PROTOCOL};

pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const META_BASE_URL: &str = "https://api.meta.ai/v1/responses";
pub const XAI_BASE_URL: &str = "https://api.x.ai/v1/responses";
pub(crate) const COPILOT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

/// A base URL may be either the API root or the complete Responses endpoint.
/// OAuth references are resolved on each request, never cached as access tokens here.
#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub name: String,
    pub base_url: String,
    pub flavor: Flavor,
    pub(crate) auth: Option<AuthSource>,
}

impl RouteConfig {
    pub fn new(name: &str, base_url: &str, key: Option<String>, flavor: Flavor) -> Self {
        Self::with_auth(name, base_url, key.as_deref().map(|key| AuthSource::from_key(name, key)), flavor)
    }

    pub fn with_auth(name: &str, base_url: &str, auth: Option<AuthSource>, flavor: Flavor) -> Self {
        Self { name: name.to_string(), base_url: base_url.trim_end_matches('/').to_string(), flavor, auth }
    }

    pub fn has_key(&self) -> bool {
        self.auth.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct ResponsesConfig {
    pub routes: Vec<RouteConfig>,
}

fn secret(l: &Loadout, key: &str) -> Result<Option<String>, ConfigError> {
    let mut target = key.to_string();
    let mut visited = std::collections::BTreeSet::new();
    loop {
        if !visited.insert(target.clone()) {
            return Err(ConfigError::Type { key: key.to_string(), expected: "an API key or a non-cyclic credential reference" });
        }
        let value = match l.secrets.get(&target) {
            Some(secret) => secret.expose(),
            None => match l.get(&target) {
                Some(Value::String(value)) => value,
                Some(_) => return Err(ConfigError::Type { key: target, expected: "a string" }),
                None if target == key => return Ok(None),
                None => return Err(ConfigError::MissingKey(target)),
            },
        };
        if let Some(name) = value.strip_prefix("cred:") {
            target = format!("credentials.{name}");
        } else {
            return Ok((!value.is_empty()).then(|| value.to_string()));
        }
    }
}

fn base_url(l: &Loadout, name: &str, default: Option<&str>) -> Result<String, ConfigError> {
    let key = format!("provider.{name}.base_url");
    match l.get(&key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(ConfigError::Type { key, expected: "a URL string" }),
        None => default.map(str::to_string).ok_or(ConfigError::MissingKey(key)),
    }
}

const NAMED: [(&str, &str, Flavor); 3] =
    [("openai-codex", CODEX_BASE_URL, Flavor::Codex), ("meta", META_BASE_URL, Flavor::Generic), ("xai", XAI_BASE_URL, Flavor::Generic)];

impl Config for ResponsesConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = Vec::with_capacity(7);
        for (name, base, _) in NAMED {
            keys.push(
                ConfigKey::new(
                    &format!("provider.{name}.key"),
                    Ty::String,
                    None,
                    "API key or oauth:<absolute auth.json path>; enables the named Responses route.",
                )
                .secret(),
            );
            keys.push(ConfigKey::new(
                &format!("provider.{name}.base_url"),
                Ty::String,
                Some(json!(base)),
                "Responses API root or full endpoint; an explicit override is preserved during OAuth refresh.",
            ));
        }
        keys.push(ConfigKey::new(
            "provider.responses.enabled",
            Ty::Bool,
            Some(json!(false)),
            "Enable custom openai-responses rows without a configured Codex, Meta or xAI credential.",
        ));
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let mut routes = Vec::new();
        for (name, default, flavor) in NAMED {
            if let Some(key) = secret(l, &format!("provider.{name}.key"))? {
                routes.push(RouteConfig::new(name, &base_url(l, name, Some(default))?, Some(key), flavor));
            }
        }
        for (name, row) in l.table("provider") {
            // Copilot's per-model dispatcher owns its named route, including Responses models.
            if NAMED.iter().any(|(builtin, _, _)| *builtin == name)
                || name == "github-copilot"
                || row["protocol"].as_str() != Some(PROTOCOL.tag())
            {
                continue;
            }
            routes.push(RouteConfig::new(&name, &base_url(l, &name, None)?, secret(l, &format!("provider.{name}.key"))?, Flavor::Generic));
        }
        Ok(Self { routes })
    }
}
