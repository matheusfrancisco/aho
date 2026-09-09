use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Method;
use serde_json::{Value, json};

use crate::browser::{Callback, Pkce, random_token};
use crate::engine::{required_string, token_credential};
use crate::{AuthError, Credential, Engine, LoginEvent, LoginOptions, LoginUi, Poll, poll_device, response_json};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_START: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFY: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";

pub(super) async fn login(options: &LoginOptions, ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    if options.device_code {
        return device_login(ui, engine).await;
    }
    let pkce = Pkce::new()?;
    let state = random_token()?;
    let callback = Callback::bind(1455, "/auth/callback".into(), Some(state.clone())).await?;
    let mut url = reqwest::Url::parse(&engine.url(AUTHORIZE_URL)).map_err(|_| AuthError::InvalidInput)?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "aho"),
    ]);
    ui.event(LoginEvent::AuthUrl {
        url: url.into(),
        instructions: "Complete ChatGPT sign-in in your browser, or paste the redirect URL.".into(),
    });
    let code = callback.code(ui).await?;
    exchange(&code, &pkce.verifier, REDIRECT_URI, engine).await
}

async fn device_login(ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    let body = engine.send_json(engine.request(Method::POST, DEVICE_START).json(&json!({ "client_id": CLIENT_ID }))).await?;
    let device_id = required_string(&body, "device_auth_id")?.to_owned();
    let user_code = required_string(&body, "user_code")?.to_owned();
    let interval = body
        .get("interval")
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= 900.0)
        .ok_or(AuthError::InvalidResponse)?;
    ui.event(LoginEvent::DeviceCode {
        user_code: user_code.clone(),
        verification_uri: engine.url(DEVICE_VERIFY),
        verification_uri_complete: None,
        expires_in: 900,
        interval: interval.ceil() as u64,
    });
    let (code, verifier) = poll_device(Duration::from_secs_f64(interval), Duration::from_secs(900), false, || async {
        let response = engine
            .send(engine.request(Method::POST, DEVICE_TOKEN).json(&json!({
                "device_auth_id": device_id, "user_code": user_code,
            })))
            .await?;
        let status = response.status();
        if status.as_u16() == 403 || status.as_u16() == 404 {
            return Ok(Poll::Pending);
        }
        let body = response_json(response).await?;
        if status.is_success() {
            return Ok(Poll::Complete((
                required_string(&body, "authorization_code")?.to_owned(),
                required_string(&body, "code_verifier")?.to_owned(),
            )));
        }
        let error = body.get("error").and_then(|v| v.as_str().or_else(|| v.get("code")?.as_str()));
        match error {
            Some("deviceauth_authorization_pending") => Ok(Poll::Pending),
            Some("slow_down") => Ok(Poll::SlowDown(None)),
            Some("access_denied") => Err(AuthError::AccessDenied),
            _ => Err(AuthError::Http(status.as_u16())),
        }
    })
    .await?;
    exchange(&code, &verifier, DEVICE_REDIRECT, engine).await
}

async fn exchange(code: &str, verifier: &str, redirect: &str, engine: &Engine) -> Result<Credential, AuthError> {
    let body = engine
        .send_json(engine.request(Method::POST, TOKEN_URL).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ]))
        .await?;
    with_account(token_credential(&body, None)?)
}

pub(super) async fn refresh(credential: &Credential, engine: &Engine) -> Result<Credential, AuthError> {
    let refresh = credential.refresh_token.as_deref().ok_or(AuthError::InvalidCredential)?;
    let body = engine
        .send_json(engine.request(Method::POST, TOKEN_URL).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh),
        ]))
        .await?;
    with_account(token_credential(&body, Some(credential))?)
}

fn with_account(mut credential: Credential) -> Result<Credential, AuthError> {
    let mut parts = credential.access_token.split('.');
    parts.next();
    let payload = parts.next().ok_or(AuthError::InvalidResponse)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(AuthError::InvalidResponse);
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).map_err(|_| AuthError::InvalidResponse)?;
    let claims: Value = serde_json::from_slice(&decoded).map_err(|_| AuthError::InvalidResponse)?;
    let account = claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(AuthError::InvalidResponse)?;
    credential.metadata.insert("account_id".into(), account.into());
    Ok(credential)
}
