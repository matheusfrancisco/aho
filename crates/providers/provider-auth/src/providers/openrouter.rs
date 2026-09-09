use std::time::Duration;

use reqwest::Method;
use serde_json::json;

use crate::browser::{Callback, Pkce, random_token};
use crate::engine::required_string;
use crate::{AuthError, Credential, Engine, LoginEvent, LoginUi};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

pub(super) async fn login(ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    tokio::time::timeout(Duration::from_secs(300), flow(ui, engine)).await.map_err(|_| AuthError::Timeout)?
}

async fn flow(ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    let pkce = Pkce::new()?;
    // OpenRouter does not echo OAuth state; the unguessable callback path binds the browser response.
    let callback = Callback::bind(0, format!("/oauth/callback/{}", random_token()?), None).await?;
    let mut url = reqwest::Url::parse(&engine.url(AUTHORIZE_URL)).map_err(|_| AuthError::InvalidInput)?;
    url.query_pairs_mut().extend_pairs([
        ("callback_url", callback.redirect_uri().as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    ui.event(LoginEvent::AuthUrl {
        url: url.into(),
        instructions: "Sign in with OpenRouter in your browser, or paste the redirect URL.".into(),
    });
    let code = callback.code(ui).await?;
    let body = engine
        .send_json(engine.request(Method::POST, TOKEN_URL).json(&json!({
            "code": code, "code_verifier": pkce.verifier, "code_challenge_method": "S256",
        })))
        .await?;
    Ok(Credential {
        access_token: required_string(&body, "key")?.to_owned(),
        refresh_token: None,
        expires_at: None,
        metadata: Default::default(),
    })
}
