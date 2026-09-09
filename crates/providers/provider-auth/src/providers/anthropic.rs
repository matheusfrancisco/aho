use reqwest::Method;
use serde_json::json;

use crate::browser::{Callback, Pkce};
use crate::engine::token_credential;
use crate::{AuthError, Credential, Engine, LoginEvent, LoginUi};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

pub(super) async fn login(ui: &dyn LoginUi, engine: &Engine) -> Result<Credential, AuthError> {
    let pkce = Pkce::new()?;
    let callback = Callback::bind(53692, "/callback".into(), Some(pkce.verifier.clone())).await?;
    let mut url = reqwest::Url::parse(&engine.url(AUTHORIZE_URL)).map_err(|_| AuthError::InvalidInput)?;
    url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", pkce.verifier.as_str()),
    ]);
    ui.event(LoginEvent::AuthUrl {
        url: url.into(),
        instructions: "Complete sign-in in your browser; remote sessions may paste the redirect URL.".into(),
    });
    let code = callback.code(ui).await?;
    let body = engine
        .send_json(engine.request(Method::POST, TOKEN_URL).json(&json!({
            "grant_type": "authorization_code", "client_id": CLIENT_ID,
            "code": code, "state": pkce.verifier, "code_verifier": pkce.verifier,
            "redirect_uri": REDIRECT_URI,
        })))
        .await?;
    token_credential(&body, None)
}

pub(super) async fn refresh(credential: &Credential, engine: &Engine) -> Result<Credential, AuthError> {
    let refresh = credential.refresh_token.as_deref().ok_or(AuthError::InvalidCredential)?;
    let body = engine
        .send_json(engine.request(Method::POST, TOKEN_URL).json(&json!({
            "grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh,
        })))
        .await?;
    token_credential(&body, Some(credential))
}
