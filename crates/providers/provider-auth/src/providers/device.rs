use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use crate::engine::now_ms;
use crate::{AuthError, Credential, Engine, LoginEvent, LoginOptions, LoginUi, Poll, ResolvedAuth, poll_device, response_json};

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const META_CLIENT_ID: &str = "1031625952748946";
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const META_MINT: &str = "https://api.meta.ai/muse-code/key";
const XAI_TOKEN: &str = "https://auth.x.ai/oauth2/token";
const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

pub(super) fn required<'a>(body: &'a Value, field: &str) -> Result<&'a str, AuthError> {
    body.get(field).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or(AuthError::InvalidResponse)
}

pub(super) fn positive(body: &Value, field: &str) -> Option<u64> {
    body.get(field).and_then(Value::as_u64).filter(|n| *n > 0)
}

pub(super) fn http_url(raw: &str) -> Result<String, AuthError> {
    let url = reqwest::Url::parse(raw).map_err(|_| AuthError::InvalidResponse)?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(AuthError::InvalidResponse);
    }
    Ok(url.to_string())
}

pub(super) fn refresh_secret(credential: &Credential) -> Result<&str, AuthError> {
    credential.refresh_token.as_deref().filter(|s| !s.is_empty()).ok_or(AuthError::InvalidCredential)
}

pub(super) struct DeviceCode {
    code: String,
    interval: u64,
    expires: u64,
}

pub(super) fn announce_device(
    body: &Value,
    ui: &dyn LoginUi,
    https_only: bool,
    require_complete: bool,
    require_expiry: bool,
) -> Result<DeviceCode, AuthError> {
    let code = required(body, "device_code")?.to_owned();
    let user_code = required(body, "user_code")?.to_owned();
    let verification_uri_complete =
        body.get("verification_uri_complete").and_then(Value::as_str).filter(|s| !s.is_empty()).map(http_url).transpose()?;
    let verification_uri = required(body, "verification_uri")
        .and_then(http_url)
        .or_else(|_| verification_uri_complete.clone().ok_or(AuthError::InvalidResponse))?;
    if require_complete && verification_uri_complete.is_none() {
        return Err(AuthError::InvalidResponse);
    }
    if https_only
        && (!verification_uri.starts_with("https://") || verification_uri_complete.as_ref().is_some_and(|url| !url.starts_with("https://")))
    {
        return Err(AuthError::InvalidResponse);
    }
    let interval = positive(body, "interval").unwrap_or(5);
    let expires = match positive(body, "expires_in") {
        Some(expires) => expires,
        None if !require_expiry => 15 * 60,
        None => return Err(AuthError::InvalidResponse),
    };
    ui.event(LoginEvent::DeviceCode { user_code, verification_uri, verification_uri_complete, expires_in: expires, interval });
    Ok(DeviceCode { code, interval, expires })
}

pub(super) async fn poll_token(
    engine: &Engine,
    token_url: &str,
    client_id: &str,
    device: &DeviceCode,
    wait_before_first: bool,
    copilot: bool,
) -> Result<Value, AuthError> {
    poll_device(Duration::from_secs(device.interval), Duration::from_secs(device.expires), wait_before_first, || async {
        let mut request = engine.request(Method::POST, token_url).header("Accept", "application/json").form(&[
            ("client_id", client_id),
            ("device_code", device.code.as_str()),
            ("grant_type", DEVICE_GRANT),
        ]);
        if copilot {
            request = request.header("User-Agent", COPILOT_HEADERS[0].1);
        }
        let response = engine.send(request).await?;
        let status = response.status();
        let body = response_json(response).await?;
        if status.is_success() && body.get("access_token").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
            return Ok(Poll::Complete(body));
        }
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(Poll::Pending),
            Some("slow_down") => Ok(Poll::SlowDown(positive(&body, "interval").map(Duration::from_secs))),
            Some("access_denied" | "authorization_denied") => Err(AuthError::AccessDenied),
            Some("expired_token") => Err(AuthError::Timeout),
            _ if !status.is_success() => Err(AuthError::Http(status.as_u16())),
            _ => Err(AuthError::InvalidResponse),
        }
    })
    .await
}

pub(super) fn oauth_credential(
    body: &Value,
    previous_refresh: Option<&str>,
    default_expiry: Option<u64>,
    skew_seconds: u64,
) -> Result<Credential, AuthError> {
    let access_token = required(body, "access_token")?.to_owned();
    let refresh_token = match body.get("refresh_token") {
        None => previous_refresh.filter(|s| !s.is_empty()).ok_or(AuthError::InvalidResponse)?.to_owned(),
        Some(_) => required(body, "refresh_token")?.to_owned(),
    };
    let expires = match body.get("expires_in") {
        None => default_expiry.ok_or(AuthError::InvalidResponse)?,
        Some(_) => positive(body, "expires_in").ok_or(AuthError::InvalidResponse)?,
    };
    let mut metadata = BTreeMap::new();
    if let Some(scope) = body.get("scope").and_then(Value::as_str) {
        metadata.insert("scope".into(), json!(scope));
    }
    Ok(Credential {
        access_token,
        refresh_token: Some(refresh_token),
        expires_at: Some(now_ms().saturating_add(expires.saturating_mul(1000)).saturating_sub(skew_seconds * 1000)),
        metadata,
    })
}

#[allow(clippy::disallowed_methods, reason = "authority: auth-kimi-host-env")]
fn kimi_host() -> Result<String, AuthError> {
    let raw = std::env::var("KIMI_CODE_OAUTH_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("KIMI_OAUTH_HOST").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "https://auth.kimi.com".into());
    Ok(http_url(&raw).map_err(|_| AuthError::InvalidInput)?.trim_end_matches('/').to_owned())
}

fn enterprise_domain(input: Option<&str>) -> Result<Option<String>, AuthError> {
    let Some(input) = input.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    let raw = if input.contains("://") { input.to_owned() } else { format!("https://{input}") };
    let parsed = reqwest::Url::parse(&http_url(&raw).map_err(|_| AuthError::InvalidInput)?).map_err(|_| AuthError::InvalidInput)?;
    Ok(Some(parsed.host_str().ok_or(AuthError::InvalidInput)?.to_owned()))
}

fn copilot_request(engine: &Engine, method: Method, url: &str, token: &str) -> reqwest::RequestBuilder {
    let mut request = engine.request(method, url).header("Accept", "application/json").bearer_auth(token);
    for (key, value) in COPILOT_HEADERS {
        request = request.header(key, value);
    }
    request
}

fn copilot_base(token: &str, enterprise: Option<&str>) -> Result<String, AuthError> {
    if let Some(host) = token.split(';').find_map(|part| part.strip_prefix("proxy-ep=")) {
        let host = host.strip_prefix("proxy.").map(|rest| format!("api.{rest}")).unwrap_or_else(|| host.to_owned());
        let url = http_url(&format!("https://{host}"))?;
        let parsed = reqwest::Url::parse(&url).map_err(|_| AuthError::InvalidResponse)?;
        if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(AuthError::InvalidResponse);
        }
        return Ok(url.trim_end_matches('/').into());
    }
    Ok(enterprise
        .map(|domain| format!("https://copilot-api.{domain}"))
        .unwrap_or_else(|| "https://api.individual.githubcopilot.com".into()))
}

async fn copilot_exchange(engine: &Engine, github_token: &str, enterprise: Option<&str>) -> Result<Credential, AuthError> {
    let domain = enterprise.unwrap_or("github.com");
    let body = engine
        .send_json(copilot_request(engine, Method::GET, &format!("https://api.{domain}/copilot_internal/v2/token"), github_token))
        .await?;
    let access_token = required(&body, "token")?.to_owned();
    let expires = positive(&body, "expires_at").ok_or(AuthError::InvalidResponse)?;
    let base = copilot_base(&access_token, enterprise)?;
    let mut metadata = BTreeMap::from([("base_url".into(), json!(base))]);
    if let Some(enterprise) = enterprise {
        metadata.insert("enterprise_domain".into(), json!(enterprise));
    }
    Ok(Credential {
        access_token,
        refresh_token: Some(github_token.into()),
        expires_at: Some(expires.saturating_mul(1000).saturating_sub(300_000)),
        metadata,
    })
}

async fn copilot_catalog(engine: &Engine, credential: &mut Credential, enable: bool, ui: Option<&dyn LoginUi>) -> Result<(), AuthError> {
    let enterprise = credential.metadata.get("enterprise_domain").and_then(Value::as_str);
    let base = copilot_base(&credential.access_token, enterprise)?;
    let response = engine
        .send(
            copilot_request(engine, Method::GET, &format!("{base}/models"), &credential.access_token)
                .header("X-GitHub-Api-Version", "2026-06-01"),
        )
        .await?;
    if !response.status().is_success() {
        return Err(AuthError::Http(response.status().as_u16()));
    }
    let body = response_json(response).await?;
    let models = body.get("data").and_then(Value::as_array).ok_or(AuthError::InvalidResponse)?;
    let candidates: Vec<&Value> = models
        .iter()
        .filter(|model| {
            model.get("id").and_then(Value::as_str).is_some()
                && model.pointer("/capabilities/supports/tool_calls").and_then(Value::as_bool) != Some(false)
        })
        .collect();
    let mut available: Vec<String> = candidates
        .iter()
        .filter(|model| {
            model.get("model_picker_enabled").and_then(Value::as_bool) == Some(true)
                && model.pointer("/policy/state").and_then(Value::as_str) != Some("disabled")
        })
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let fallback = available.is_empty() && base == "https://api.individual.githubcopilot.com";
    if fallback {
        available.extend(
            candidates
                .iter()
                .filter(|model| model.pointer("/policy/state").and_then(Value::as_str) == Some("enabled"))
                .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_owned)),
        );
    }
    if enable {
        let policies: Vec<&str> = candidates
            .iter()
            .filter(|model| {
                model.pointer("/policy/state").and_then(Value::as_str) == Some("unconfigured")
                    && (fallback || model.get("model_picker_enabled").and_then(Value::as_bool) == Some(true))
            })
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .collect();
        if !policies.is_empty() {
            if let Some(ui) = ui {
                ui.event(LoginEvent::Progress { message: "Enabling Copilot models...".into() });
            }
        }
        for id in policies {
            let mut url = reqwest::Url::parse(&format!("{base}/models/")).map_err(|_| AuthError::InvalidResponse)?;
            url.path_segments_mut().map_err(|_| AuthError::InvalidResponse)?.pop_if_empty().push(id).push("policy");
            let response = engine
                .send(
                    copilot_request(engine, Method::POST, url.as_str(), &credential.access_token)
                        .header("openai-intent", "chat-policy")
                        .header("x-interaction-type", "chat-policy")
                        .json(&json!({"state":"enabled"})),
                )
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    if !available.iter().any(|item| item == id) {
                        available.push(id.into());
                    }
                }
                Ok(response) if response.status().as_u16() == 429 => break,
                _ => {}
            }
        }
    }
    credential.metadata.insert("available_model_ids".into(), json!(available));
    credential.metadata.insert("catalog".into(), body);
    Ok(())
}

async fn mint_meta(engine: &Engine, identity: &str, ui: Option<&dyn LoginUi>) -> Result<Credential, AuthError> {
    let response = engine
        .send(
            engine
                .request(Method::POST, META_MINT)
                .bearer_auth(identity)
                .header("Accept", "application/json")
                .header("x-api-version", "1.0.0")
                .json(&json!({})),
        )
        .await?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(AuthError::InvalidCredential);
    }
    if !status.is_success() {
        return Err(AuthError::Http(status.as_u16()));
    }
    let body = response_json(response).await?;
    if required(&body, "api_key").is_err() {
        if let (Some(ui), Some(url)) = (ui, body.get("action_url").and_then(Value::as_str).and_then(|url| http_url(url).ok())) {
            ui.event(LoginEvent::AuthUrl { url, instructions: "Complete Meta Model API setup, then run aho login meta again.".into() });
        }
        return Err(AuthError::InvalidResponse);
    }
    Ok(Credential {
        access_token: required(&body, "api_key")?.into(),
        refresh_token: Some(identity.into()),
        expires_at: Some(now_ms().saturating_add(86_400_000)),
        metadata: BTreeMap::from([("base_url".into(), json!("https://api.meta.ai/v1"))]),
    })
}

pub(super) async fn login(provider: &str, options: &LoginOptions, ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    match provider {
        "github-copilot" => {
            let enterprise = enterprise_domain(options.enterprise_domain.as_deref())?;
            let domain = enterprise.as_deref().unwrap_or("github.com");
            let body = engine
                .send_json(
                    engine
                        .request(Method::POST, &format!("https://{domain}/login/device/code"))
                        .header("Accept", "application/json")
                        .header("User-Agent", COPILOT_HEADERS[0].1)
                        .form(&[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")]),
                )
                .await?;
            let device = announce_device(&body, ui, false, false, true)?;
            let body =
                poll_token(engine, &format!("https://{domain}/login/oauth/access_token"), COPILOT_CLIENT_ID, &device, true, true).await?;
            let mut credential = copilot_exchange(engine, required(&body, "access_token")?, enterprise.as_deref()).await?;
            copilot_catalog(engine, &mut credential, true, Some(ui)).await?;
            Ok(credential)
        }
        "kimi-coding" => {
            let host = kimi_host()?;
            let body = engine
                .send_json(
                    engine
                        .request(Method::POST, &format!("{host}/api/oauth/device_authorization"))
                        .header("Accept", "application/json")
                        .form(&[("client_id", KIMI_CLIENT_ID)]),
                )
                .await?;
            let device = announce_device(&body, ui, false, true, false)?;
            let body = poll_token(engine, &format!("{host}/api/oauth/token"), KIMI_CLIENT_ID, &device, true, false).await?;
            let mut credential = oauth_credential(&body, None, None, 0)?;
            credential.metadata.insert("oauth_host".into(), json!(host));
            credential.metadata.insert("base_url".into(), json!("https://api.kimi.com/coding"));
            Ok(credential)
        }
        "meta" => {
            let body = engine
                .send_json(
                    engine
                        .request(Method::POST, "https://auth.meta.com/oidc/device/authorization/")
                        .header("Accept", "application/json")
                        .form(&[("client_id", META_CLIENT_ID)]),
                )
                .await?;
            let device = announce_device(&body, ui, false, false, false)?;
            let body = poll_token(engine, "https://auth.meta.com/oidc/device/token/", META_CLIENT_ID, &device, true, false).await?;
            ui.event(LoginEvent::Progress { message: "Enabling Meta Model API access...".into() });
            mint_meta(engine, required(&body, "access_token")?, Some(ui)).await
        }
        "xai" => {
            let body = engine
                .send_json(engine.request(Method::POST, "https://auth.x.ai/oauth2/device/code").header("Accept", "application/json").form(
                    &[
                        ("client_id", XAI_CLIENT_ID),
                        ("scope", "openid profile email offline_access grok-cli:access api:access"),
                        ("referrer", "pi"),
                    ],
                ))
                .await?;
            let device = announce_device(&body, ui, true, false, true)?;
            let body = poll_token(engine, XAI_TOKEN, XAI_CLIENT_ID, &device, true, false).await?;
            let mut credential = oauth_credential(&body, None, Some(3600), 300)?;
            credential.metadata.insert("base_url".into(), json!("https://api.x.ai/v1"));
            Ok(credential)
        }
        _ => Err(AuthError::UnsupportedProvider),
    }
}

pub(super) async fn refresh(provider: &str, credential: &Credential, engine: &Engine) -> Result<Credential, AuthError> {
    let secret = refresh_secret(credential)?;
    match provider {
        "github-copilot" => {
            let enterprise = enterprise_domain(credential.metadata.get("enterprise_domain").and_then(Value::as_str))?;
            let mut next = copilot_exchange(engine, secret, enterprise.as_deref()).await?;
            copilot_catalog(engine, &mut next, false, None).await?;
            Ok(next)
        }
        "meta" => mint_meta(engine, secret, None).await,
        "kimi-coding" | "xai" => {
            let host = if provider == "kimi-coding" {
                Some(credential.metadata.get("oauth_host").and_then(Value::as_str).map(str::to_owned).map(Ok).unwrap_or_else(kimi_host)?)
            } else {
                None
            };
            let url = host.as_ref().map(|host| format!("{host}/api/oauth/token")).unwrap_or_else(|| XAI_TOKEN.into());
            let client_id = if provider == "kimi-coding" { KIMI_CLIENT_ID } else { XAI_CLIENT_ID };
            let retries = if provider == "kimi-coding" { 3 } else { 0 };
            for attempt in 0..=retries {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
                }
                let response = engine
                    .send(engine.request(Method::POST, &url).header("Accept", "application/json").form(&[
                        ("client_id", client_id),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", secret),
                    ]))
                    .await;
                let response = match response {
                    Err(AuthError::Network | AuthError::Timeout) if attempt < retries => continue,
                    response => response?,
                };
                let status = response.status();
                if matches!(status.as_u16(), 401 | 403) {
                    return Err(AuthError::InvalidCredential);
                }
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < retries {
                    continue;
                }
                let body = response_json(response).await?;
                if body.get("error").and_then(Value::as_str) == Some("invalid_grant") {
                    return Err(AuthError::InvalidCredential);
                }
                if !status.is_success() {
                    return Err(AuthError::Http(status.as_u16()));
                }
                let mut next = if provider == "kimi-coding" {
                    oauth_credential(&body, None, None, 0)?
                } else {
                    oauth_credential(&body, Some(secret), Some(3600), 300)?
                };
                for (key, value) in &credential.metadata {
                    next.metadata.entry(key.clone()).or_insert_with(|| value.clone());
                }
                return Ok(next);
            }
            Err(AuthError::Network)
        }
        _ => Err(AuthError::UnsupportedProvider),
    }
}

pub(super) fn resolve(provider: &str, credential: &Credential) -> Result<ResolvedAuth, AuthError> {
    let mut headers = BTreeMap::new();
    let base_url = match provider {
        "github-copilot" => {
            for (key, value) in COPILOT_HEADERS {
                headers.insert(key.into(), value.into());
            }
            Some(copilot_base(&credential.access_token, credential.metadata.get("enterprise_domain").and_then(Value::as_str))?)
        }
        "kimi-coding" => Some("https://api.kimi.com/coding".into()),
        "meta" => Some("https://api.meta.ai/v1".into()),
        "xai" => Some("https://api.x.ai/v1".into()),
        _ => return Err(AuthError::UnsupportedProvider),
    };
    Ok(ResolvedAuth { token: credential.access_token.clone(), headers, base_url, oauth: true })
}

#[cfg(test)]
#[path = "tests/device.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/server.rs"]
pub(super) mod test_server;
