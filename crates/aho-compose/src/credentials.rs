//! Credential presence (what `Requirement::Credential` checks) and the guarantee that a
//! credential value never reaches the log, even when no capability declared its key secret.

use std::collections::BTreeMap;

use aho_config::{Loadout, Origin, Secret, Where};
use aho_types::Value;
use serde::Serialize;

use crate::{CRED_ENV_PREFIX, Inputs};

/// Well-known provider variables, by the credential name requirements use.
const WELL_KNOWN_ENV: &[(&str, &str)] = &[("OPENAI_API_KEY", "openai"), ("ANTHROPIC_API_KEY", "anthropic"), ("GITHUB_TOKEN", "github")];

/// The place a credential was found. Closed; the consent-gated origins (external files, CLI
/// sessions) belong to import flows, and plain loading never produces them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum CredentialOrigin {
    Env { var: String },
    AppConfigFile { key: String, origin: String },
}

/// `provider.<name>.key` and `credentials.<name>` name credential `<name>`.
pub fn credential_name(key: &str) -> Option<&str> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["provider", name, "key"] | ["credentials", name] if !name.is_empty() => Some(*name),
        _ => None,
    }
}

fn env_credential(var: &str) -> Option<String> {
    if let Some((_, name)) = WELL_KNOWN_ENV.iter().find(|(v, _)| *v == var) {
        return Some(name.to_string());
    }
    var.strip_prefix(CRED_ENV_PREFIX).filter(|n| !n.is_empty()).map(str::to_lowercase)
}

/// Every credential present, with each place it was found. Empty values do not count.
pub fn credentials(inputs: &Inputs, loadout: &Loadout) -> BTreeMap<String, Vec<CredentialOrigin>> {
    let mut out: BTreeMap<String, Vec<CredentialOrigin>> = BTreeMap::new();
    for (var, value) in &inputs.env {
        if value.is_empty() {
            continue;
        }
        if let Some(name) = env_credential(var) {
            out.entry(name).or_default().push(CredentialOrigin::Env { var: var.clone() });
        }
    }
    for (key, secret) in &loadout.secrets {
        if secret.expose().is_empty() {
            continue;
        }
        let Some(name) = credential_name(key) else { continue };
        let entry = match loadout.config.get(key).map(|s| &s.winner) {
            // `AHO_PROVIDER_OPENAI_KEY` is still an environment credential.
            Some(Origin { at: Where::Env { var }, .. }) => CredentialOrigin::Env { var: var.clone() },
            Some(o) => CredentialOrigin::AppConfigFile { key: key.clone(), origin: o.render() },
            None => CredentialOrigin::AppConfigFile { key: key.clone(), origin: String::new() },
        };
        out.entry(name.to_string()).or_default().push(entry);
    }
    out
}

fn is_digest(v: &Value) -> bool {
    matches!(v, Value::Object(m) if m.len() == 1 && m.get("digest").and_then(Value::as_str).is_some_and(|h| h.len() == 64))
}

fn plain(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn digest_of(text: String) -> Value {
    serde_json::json!({ "digest": Secret::new(text).digest.to_hex() })
}

/// Replaces credential-shaped values with `{digest}` everywhere they appear under `path`
/// (leaf keys and inline tables alike); returns `(canonical key, plaintext)` for each replaced leaf.
fn redact_value(path: &str, v: &mut Value, found: &mut Vec<(String, String)>) {
    if credential_name(path).is_some() {
        if !is_digest(v) {
            let text = plain(v);
            found.push((path.to_string(), text.clone()));
            *v = digest_of(text);
        }
        return;
    }
    if let Value::Object(m) = v {
        for (k, child) in m.iter_mut() {
            redact_value(&format!("{path}.{k}"), child, found);
        }
    }
}

/// Moves plaintext of undeclared credential keys into `loadout.secrets`; config keeps digests.
/// Keys a capability declared `.secret()` were already redacted by the resolver and are left alone.
pub(crate) fn redact(loadout: &mut Loadout) {
    let mut winners = Vec::new();
    for (key, sourced) in loadout.config.iter_mut() {
        redact_value(key, &mut sourced.value, &mut winners);
        for (_, loser) in sourced.shadowed.iter_mut() {
            redact_value(key, loser, &mut Vec::new());
        }
    }
    for (key, text) in winners {
        loadout.secrets.entry(key).or_insert_with(|| Secret::new(text));
    }
}
