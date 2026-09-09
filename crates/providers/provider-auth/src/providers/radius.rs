use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::{Value, json};

use crate::browser::{Callback, Pkce, random_token};
use crate::{AuthError, Credential, Engine, LoginEvent, LoginOptions, LoginUi, ResolvedAuth, response_json};

use super::device::{announce_device, http_url, oauth_credential, poll_token, refresh_secret, required};

const DEFAULT_GATEWAY: &str = "https://radius.pi.dev";
const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";

fn normalize_gateway(raw: &str) -> Result<String, AuthError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AuthError::InvalidInput);
    }
    let qualified = if raw.contains("://") { raw.into() } else { format!("https://{raw}") };
    let value = http_url(&qualified).map_err(|_| AuthError::InvalidInput)?;
    let url = reqwest::Url::parse(&value).map_err(|_| AuthError::InvalidInput)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AuthError::InvalidInput);
    }
    Ok(value.trim_end_matches('/').into())
}

fn endpoint(gateway: &str, path: &str) -> Result<String, AuthError> {
    reqwest::Url::parse(gateway)
        .map_err(|_| AuthError::InvalidInput)?
        .join(path)
        .map(|url| url.to_string())
        .map_err(|_| AuthError::InvalidInput)
}

async fn token(engine: &Engine, gateway: &str, fields: &[(&str, &str)], previous_refresh: Option<&str>) -> Result<Credential, AuthError> {
    let response = engine
        .send(engine.request(Method::POST, &endpoint(gateway, "/v1/oauth/token")?).header("Accept", "application/json").form(fields))
        .await?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(AuthError::InvalidCredential);
    }
    let body = response_json(response).await?;
    if body.get("error").and_then(Value::as_str) == Some("invalid_grant") {
        return Err(AuthError::InvalidCredential);
    }
    if !status.is_success() {
        return Err(AuthError::Http(status.as_u16()));
    }
    oauth_credential(&body, previous_refresh, None, 60)
}

async fn browser_login(engine: &Engine, gateway: &str, ui: &dyn LoginUi, callback_port: u16) -> Result<Credential, AuthError> {
    let discovery =
        engine.send_json(engine.request(Method::GET, &endpoint(gateway, "/v1/oauth")?).header("Accept", "application/json")).await?;
    let authorization_endpoint = http_url(required(&discovery, "authorizationEndpoint")?)?;
    let pkce = Pkce::new()?;
    let state = random_token()?;
    let callback = Callback::bind(callback_port, "/oauth/callback".into(), Some(state.clone())).await?;
    let redirect = callback.redirect_uri();
    let mut authorize = reqwest::Url::parse(&authorization_endpoint).map_err(|_| AuthError::InvalidResponse)?;
    authorize.set_query(None);
    authorize.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect.as_str()),
        ("scope", SCOPE),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("handoff", "url"),
        ("state", state.as_str()),
    ]);
    ui.event(LoginEvent::Progress { message: format!("Listening for OAuth callback on {redirect}") });
    ui.event(LoginEvent::AuthUrl { url: authorize.to_string(), instructions: "Continue in your browser.".into() });
    let code = callback.code(ui).await?;
    token(
        engine,
        gateway,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect.as_str()),
            ("code", code.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
        ],
        None,
    )
    .await
}

async fn device_login(engine: &Engine, gateway: &str, ui: &dyn LoginUi) -> Result<Credential, AuthError> {
    let body = engine
        .send_json(
            engine
                .request(Method::POST, &endpoint(gateway, "/v1/oauth/device")?)
                .header("Accept", "application/json")
                .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]),
        )
        .await?;
    let device = announce_device(&body, ui, false, false, true)?;
    let body = poll_token(engine, &endpoint(gateway, "/v1/oauth/token")?, CLIENT_ID, &device, false, false).await?;
    oauth_credential(&body, None, None, 60)
}

fn gateway_model(value: &Value) -> bool {
    value.get("id").and_then(Value::as_str).is_some()
        && value.get("name").and_then(Value::as_str).is_some()
        && value.get("reasoning").and_then(Value::as_bool).is_some()
        && value.get("input").and_then(Value::as_array).is_some()
        && value.get("cost").and_then(Value::as_object).is_some()
        && value.get("contextWindow").and_then(Value::as_u64).is_some()
        && value.get("maxTokens").and_then(Value::as_u64).is_some()
}

async fn gateway_config(engine: &Engine, gateway: &str, credential: &mut Credential) -> Result<(), AuthError> {
    let mut body = engine
        .send_json(
            engine
                .request(Method::GET, &endpoint(gateway, "/v1/config")?)
                .header("Accept", "application/json")
                .bearer_auth(&credential.access_token),
        )
        .await?;
    let base = http_url(required(&body, "baseUrl")?)?;
    let models = body.get_mut("models").and_then(Value::as_array_mut).ok_or(AuthError::InvalidResponse)?;
    models.retain(gateway_model);
    credential.metadata.insert("gateway".into(), json!(gateway));
    credential.metadata.insert("base_url".into(), json!(base));
    credential.metadata.insert("gateway_config".into(), body);
    Ok(())
}

pub(super) async fn login(options: &LoginOptions, ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    let gateway = normalize_gateway(options.gateway.as_deref().unwrap_or(DEFAULT_GATEWAY))?;
    let mut credential =
        if options.device_code { device_login(engine, &gateway, ui).await? } else { browser_login(engine, &gateway, ui, 1456).await? };
    gateway_config(engine, &gateway, &mut credential).await?;
    Ok(credential)
}

pub(super) async fn refresh(credential: &Credential, engine: &Engine) -> Result<Credential, AuthError> {
    let gateway = normalize_gateway(credential.metadata.get("gateway").and_then(Value::as_str).unwrap_or(DEFAULT_GATEWAY))?;
    let secret = refresh_secret(credential)?;
    let mut next =
        token(engine, &gateway, &[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", secret)], Some(secret))
            .await?;
    gateway_config(engine, &gateway, &mut next).await?;
    Ok(next)
}

pub(super) fn resolve(credential: &Credential) -> Result<ResolvedAuth, AuthError> {
    let base = credential
        .metadata
        .get("base_url")
        .and_then(Value::as_str)
        .or_else(|| credential.metadata.get("gateway_config").and_then(|config| config.get("baseUrl")).and_then(Value::as_str))
        .ok_or(AuthError::InvalidCredential)?;
    Ok(ResolvedAuth {
        token: credential.access_token.clone(),
        headers: BTreeMap::new(),
        base_url: Some(http_url(base).map_err(|_| AuthError::InvalidCredential)?),
        oauth: true,
    })
}

#[cfg(test)]
#[path = "tests/radius.rs"]
mod tests;
