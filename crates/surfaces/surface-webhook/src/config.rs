//! `surface.webhook.*`: the typed config slice. The HMAC secret is a credential ref
//! (`cred:<name>`, resolved through `credentials.<name>` like every other capability); a plain
//! secret pasted into a project file is refused so it cannot be committed by accident.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use aho_cap::{Config, ConfigError};
use aho_config::{ConfigKey, Loadout, Secret, Ty};
use aho_types::Value;
use serde_json::json;

use crate::github;
use crate::template::{DEFAULT_EVENTS, DEFAULT_TEMPLATES};

pub const DEFAULT_BIND: &str = "127.0.0.1:8788";
/// The credential the webhook reads when `surface.webhook.secret` is unset.
pub const DEFAULT_CREDENTIAL: &str = "github-webhook";
pub const DEFAULT_MENTION: &str = "@aho";
pub const DEFAULT_MAX_BODY: u64 = 1024 * 1024;
/// GitHub caps webhook payloads at 25 MiB; a larger limit could never be used.
pub const MAX_BODY_CEILING: u64 = 25 * 1024 * 1024;

const K_BIND: &str = "surface.webhook.bind";
const K_SESSION: &str = "surface.webhook.session";
const K_EVENTS: &str = "surface.webhook.events";
const K_TEMPLATES: &str = "surface.webhook.templates";
const K_MENTION: &str = "surface.webhook.mention";
const K_SECRET: &str = "surface.webhook.secret";
const K_MAX_BODY: &str = "surface.webhook.max_body";
const K_TLS_CERT: &str = "surface.webhook.tls_cert";
const K_TLS_KEY: &str = "surface.webhook.tls_key";
const K_INSECURE: &str = "surface.webhook.insecure_loopback";

/// The listener's transport: HTTPS with the operator's pair, or plain HTTP on loopback only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Tls {
        cert: PathBuf,
        key: PathBuf,
    },
    /// The operator's explicit acknowledgment (`surface.webhook.insecure_loopback = true`, the
    /// CLI's `--insecure-loopback`); refused off loopback exactly like surface-web.
    InsecureLoopback,
}

/// Everything `serve` needs. Built from the loadout by [`Config::from_loadout`].
#[derive(Clone, Debug)]
pub struct WebhookOptions {
    pub bind: SocketAddr,
    /// Session id (or unique prefix) the CLI binds; `None` → the CLI opens a new session.
    pub session: Option<String>,
    /// `<event>` or `<event>.<action>`; each has an entry in `templates`.
    pub events: Vec<String>,
    pub templates: BTreeMap<String, String>,
    /// `issue_comment` events prompt only when the comment mentions this; empty → every comment.
    pub mention: String,
    pub max_body: usize,
    pub secret: Secret,
    /// `None` until the operator picks one; `serve` refuses to start without it.
    pub transport: Option<Transport>,
}

impl WebhookOptions {
    /// The template for an event: `<event>.<action>` first, then the bare `<event>`.
    pub fn subscription(&self, event: &str, action: Option<&str>) -> Option<(&str, &str)> {
        let exact = action.map(|a| format!("{event}.{a}"));
        [exact.as_deref(), Some(event)]
            .into_iter()
            .flatten()
            .find_map(|k| self.events.iter().find(|e| e.as_str() == k))
            .and_then(|k| self.templates.get(k).map(|t| (k.as_str(), t.as_str())))
    }
}

fn bad(key: &str, expected: &'static str) -> ConfigError {
    ConfigError::Type { key: key.into(), expected }
}

fn valid_subscription(name: &str) -> bool {
    let mut parts = name.split('.');
    let event = parts.next().is_some_and(github::valid_event);
    let action = parts.next().is_none_or(github::valid_event);
    event && action && parts.next().is_none()
}

/// Dotted leaves of a table (`{issues: {opened: "…"}}` → `issues.opened`).
fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, String>) -> Result<(), ConfigError> {
    match v {
        Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
            Ok(())
        }
        Value::Object(m) => m.iter().try_for_each(|(k, child)| {
            let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            flatten(&key, child, out)
        }),
        _ => Err(bad(K_TEMPLATES, "a table of template strings keyed by `<event>` or `<event>.<action>`")),
    }
}

/// `cred:<name>` → the plaintext of `credentials.<name>`.
fn resolve_secret(l: &Loadout) -> Result<Secret, ConfigError> {
    let raw = match l.secrets.get(K_SECRET) {
        Some(s) => s.expose().to_string(),
        None => match l.get(K_SECRET) {
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(bad(K_SECRET, "a credential ref `cred:<name>`")),
            None => format!("cred:{DEFAULT_CREDENTIAL}"),
        },
    };
    let name = raw
        .strip_prefix("cred:")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| bad(K_SECRET, "a credential ref `cred:<name>`; store the secret itself with `aho credential set <name>`"))?;
    let target = format!("credentials.{name}");
    let plain = match l.secrets.get(&target) {
        Some(s) => s.expose().to_string(),
        None => l.str(&target).unwrap_or_default().to_string(),
    };
    if plain.is_empty() {
        return Err(ConfigError::MissingKey(target));
    }
    Ok(Secret::new(plain))
}

fn transport(l: &Loadout) -> Result<Option<Transport>, ConfigError> {
    let path = |k: &str| l.str(k).filter(|s| !s.is_empty()).map(PathBuf::from);
    match (path(K_TLS_CERT), path(K_TLS_KEY)) {
        (Some(cert), Some(key)) => Ok(Some(Transport::Tls { cert, key })),
        (Some(_), None) => Err(ConfigError::MissingKey(K_TLS_KEY.into())),
        (None, Some(_)) => Err(ConfigError::MissingKey(K_TLS_CERT.into())),
        (None, None) => Ok(aho_cap::cfg_bool(l, K_INSECURE, false).then_some(Transport::InsecureLoopback)),
    }
}

impl Config for WebhookOptions {
    const PREFIX: &'static str = "surface.webhook";

    fn keys() -> Vec<ConfigKey> {
        let events: Vec<Value> = DEFAULT_EVENTS.iter().map(|e| json!(e)).collect();
        vec![
            ConfigKey::new(
                K_BIND,
                Ty::String,
                Some(json!(DEFAULT_BIND)),
                "Address of the webhook listener (a second listener, separate from the web surface).",
            ),
            ConfigKey::new(K_SESSION, Ty::String, None, "Session id (or unique prefix) webhook prompts go to; unset → a new session."),
            ConfigKey::new(
                K_EVENTS,
                Ty::Array,
                Some(Value::Array(events)),
                "Subscribed GitHub events, `<event>` or `<event>.<action>` (e.g. `issues.opened`, `push`); others answer 204.",
            ),
            ConfigKey::new(
                K_TEMPLATES,
                Ty::Table,
                None,
                "Prompt templates keyed like `events`, with `{{payload.path}}` placeholders; built-ins cover issues.opened, issue_comment.created, pull_request.opened, pull_request.synchronize, push.",
            ),
            ConfigKey::new(
                K_MENTION,
                Ty::String,
                Some(json!(DEFAULT_MENTION)),
                "`issue_comment` events prompt only when the comment mentions this; empty → every comment.",
            ),
            ConfigKey::new(
                K_SECRET,
                Ty::String,
                None,
                "Credential ref `cred:<name>` holding the GitHub webhook secret; unset → `cred:github-webhook`.",
            )
            .secret(),
            ConfigKey::new(
                K_MAX_BODY,
                Ty::Int,
                Some(json!(DEFAULT_MAX_BODY)),
                "Largest accepted payload in bytes; larger requests answer 413.",
            ),
            ConfigKey::new(K_TLS_CERT, Ty::String, None, "PEM certificate for HTTPS (`aho cert` writes a pair)."),
            ConfigKey::new(K_TLS_KEY, Ty::String, None, "PEM private key for HTTPS."),
            ConfigKey::new(
                K_INSECURE,
                Ty::Bool,
                Some(json!(false)),
                "Serve plain HTTP; allowed only on a loopback bind (development, or behind a local TLS proxy).",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let bind = aho_cap::cfg_str(l, K_BIND, DEFAULT_BIND).parse().map_err(|_| bad(K_BIND, "an address like 127.0.0.1:8788"))?;
        let mut templates: BTreeMap<String, String> = DEFAULT_TEMPLATES.iter().map(|(k, t)| (k.to_string(), t.to_string())).collect();
        flatten("", &Value::Object(l.table(K_TEMPLATES)), &mut templates)?;
        let events = match l.get(K_EVENTS) {
            None => DEFAULT_EVENTS.iter().map(|e| e.to_string()).collect(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().filter(|s| valid_subscription(s)).map(str::to_string))
                .collect::<Option<Vec<String>>>()
                .ok_or_else(|| bad(K_EVENTS, "an array of `<event>` or `<event>.<action>` names (lowercase, `_`)"))?,
            Some(_) => return Err(bad(K_EVENTS, "an array of event names")),
        };
        if let Some(e) = events.iter().find(|e| !templates.contains_key(e.as_str())) {
            return Err(ConfigError::MissingKey(format!("{K_TEMPLATES}.{e}")));
        }
        let max_body = match l.get(K_MAX_BODY) {
            None => DEFAULT_MAX_BODY,
            Some(v) => v
                .as_u64()
                .filter(|n| (1..=MAX_BODY_CEILING).contains(n))
                .ok_or_else(|| bad(K_MAX_BODY, "a byte count between 1 and 26214400 (GitHub's payload cap)"))?,
        };
        Ok(WebhookOptions {
            bind,
            session: l.str(K_SESSION).filter(|s| !s.is_empty()).map(str::to_string),
            events,
            templates,
            mention: aho_cap::cfg_str(l, K_MENTION, DEFAULT_MENTION),
            max_body: usize::try_from(max_body).map_err(|_| bad(K_MAX_BODY, "a byte count this host can address"))?,
            secret: resolve_secret(l)?,
            transport: transport(l)?,
        })
    }
}
