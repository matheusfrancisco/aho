use super::test_server::{Server, Ui};
use super::*;

fn device() -> Value {
    json!({"device_code":"device-secret", "user_code":"ABCD-EFGH", "verification_uri":"https://login.example/verify",
        "verification_uri_complete":"https://login.example/verify?code=ABCD-EFGH", "interval":1, "expires_in":120})
}

fn stored(access: &str, refresh: &str) -> Credential {
    Credential { access_token: access.into(), refresh_token: Some(refresh.into()), expires_at: Some(1), metadata: BTreeMap::new() }
}

#[tokio::test]
async fn copilot_exchanges_github_identity_and_rediscovers_proxy_on_refresh() {
    let server = Server::start(|_| {
        vec![
            (200, device()),
            (200, json!({"error":"authorization_pending"})),
            (200, json!({"access_token":"github-identity"})),
            (200, json!({"token":"tid=one;proxy-ep=proxy.business.githubcopilot.com;exp=9999999999", "expires_at":9999999999_u64})),
            (
                200,
                json!({"data":[
                    {"id":"visible", "model_picker_enabled":true, "policy":{"state":"enabled"}},
                    {"id":"blocked", "model_picker_enabled":true, "policy":{"state":"disabled"}},
                    {"id":"hidden", "model_picker_enabled":false, "policy":{"state":"enabled"}},
                    {"id":"no-tools", "model_picker_enabled":true, "capabilities":{"supports":{"tool_calls":false}}}
                ]}),
            ),
            (200, json!({"token":"tid=two;proxy-ep=proxy.enterprise.githubcopilot.com;exp=9999999999", "expires_at":9999999999_u64})),
            (200, json!({"data":[{"id":"new-model", "model_picker_enabled":true, "policy":{"state":"enabled"}}]})),
        ]
    })
    .await;
    let engine = server.engine(&[
        ("https://github.com/login/device/code", "/device"),
        ("https://github.com/login/oauth/access_token", "/token"),
        ("https://api.github.com/copilot_internal/v2/token", "/exchange"),
        ("https://api.business.githubcopilot.com/models", "/business-models"),
        ("https://api.enterprise.githubcopilot.com/models", "/enterprise-models"),
    ]);
    let credential = login("github-copilot", &LoginOptions::default(), &Ui::default(), &engine).await.expect("login");
    assert_eq!(credential.refresh_token.as_deref(), Some("github-identity"));
    assert_eq!(credential.metadata["available_model_ids"], json!(["visible"]));
    assert_eq!(
        resolve("github-copilot", &credential).expect("resolve").base_url.as_deref(),
        Some("https://api.business.githubcopilot.com")
    );
    let next = refresh("github-copilot", &credential, &engine).await.expect("refresh");
    assert_eq!(next.metadata["available_model_ids"], json!(["new-model"]));
    assert_eq!(resolve("github-copilot", &next).expect("resolve").base_url.as_deref(), Some("https://api.enterprise.githubcopilot.com"));
    let requests = server.requests().await;
    assert_eq!(requests[0].form()["scope"], "read:user");
    assert_eq!(requests[1].form()["grant_type"], DEVICE_GRANT);
    assert_eq!(requests[3].headers["authorization"], "Bearer github-identity");
    assert_eq!(requests[4].headers["authorization"], format!("Bearer {}", credential.access_token));
    assert_eq!(requests[4].headers["x-github-api-version"], "2026-06-01");
    assert_eq!(requests[5].method, "GET");
    assert_eq!(requests[5].headers["authorization"], "Bearer github-identity");
}

#[tokio::test]
async fn copilot_individual_policy_fallback_does_not_expose_disabled_models() {
    let server = Server::start(|_| {
        vec![
            (
                200,
                json!({"data":[
                    {"id":"allowed", "model_picker_enabled":false, "policy":{"state":"enabled"}},
                    {"id":"enable-me", "model_picker_enabled":false, "policy":{"state":"unconfigured"}},
                    {"id":"disabled", "model_picker_enabled":false, "policy":{"state":"disabled"}}
                ]}),
            ),
            (200, json!({"state":"enabled"})),
        ]
    })
    .await;
    let engine = server.engine(&[
        ("https://api.individual.githubcopilot.com/models", "/models"),
        ("https://api.individual.githubcopilot.com/models/enable-me/policy", "/policy"),
    ]);
    let mut credential = stored("copilot-access", "github-identity");
    copilot_catalog(&engine, &mut credential, true, None).await.expect("catalog");
    assert_eq!(credential.metadata["available_model_ids"], json!(["allowed", "enable-me"]));
    let requests = server.requests().await;
    assert_eq!(requests[1].headers["openai-intent"], "chat-policy");
    assert_eq!(serde_json::from_str::<Value>(&requests[1].body).expect("policy body"), json!({"state":"enabled"}));
}

#[tokio::test]
async fn kimi_requires_rotated_refresh_token_and_preserves_oauth_host() {
    let server = Server::start(|_| {
        vec![
            (200, device()),
            (200, json!({"access_token":"kimi-access", "refresh_token":"kimi-refresh", "expires_in":3600})),
            (200, json!({"access_token":"kimi-next", "refresh_token":"kimi-rotated", "expires_in":3600})),
            (200, json!({"access_token":"kimi-broken", "expires_in":3600})),
        ]
    })
    .await;
    let host = kimi_host().expect("configured OAuth host");
    let engine =
        server.engine(&[(&format!("{host}/api/oauth/device_authorization"), "/device"), (&format!("{host}/api/oauth/token"), "/token")]);
    let credential = login("kimi-coding", &LoginOptions::default(), &Ui::default(), &engine).await.expect("login");
    let next = refresh("kimi-coding", &credential, &engine).await.expect("refresh");
    assert_eq!(next.refresh_token.as_deref(), Some("kimi-rotated"));
    assert_eq!(next.metadata["oauth_host"], json!(host));
    assert_eq!(refresh("kimi-coding", &next, &engine).await.expect_err("missing rotation"), AuthError::InvalidResponse);
    let requests = server.requests().await;
    assert_eq!(requests[0].form()["client_id"], KIMI_CLIENT_ID);
    assert_eq!(requests[2].form()["grant_type"], "refresh_token");
    assert_eq!(requests[2].form()["refresh_token"], "kimi-refresh");
    assert_eq!(requests[3].form()["refresh_token"], "kimi-rotated");
}

#[tokio::test]
async fn meta_mints_and_remints_api_keys_instead_of_refreshing_identity() {
    let server = Server::start(|_| {
        vec![
            (200, device()),
            (200, json!({"access_token":"meta-identity"})),
            (200, json!({"api_key":"meta-key-one"})),
            (200, json!({"api_key":"meta-key-two"})),
            (401, json!({"error_description":"meta-identity must never appear in error output"})),
        ]
    })
    .await;
    let engine = server.engine(&[
        ("https://auth.meta.com/oidc/device/authorization/", "/device"),
        ("https://auth.meta.com/oidc/device/token/", "/token"),
        (META_MINT, "/mint"),
    ]);
    let credential = login("meta", &LoginOptions::default(), &Ui::default(), &engine).await.expect("login");
    assert_eq!(credential.access_token, "meta-key-one");
    assert_eq!(credential.refresh_token.as_deref(), Some("meta-identity"));
    let next = refresh("meta", &credential, &engine).await.expect("remint");
    assert_eq!(next.access_token, "meta-key-two");
    assert_eq!(next.refresh_token.as_deref(), Some("meta-identity"));
    let error = refresh("meta", &next, &engine).await.expect_err("expired identity");
    assert_eq!(error, AuthError::InvalidCredential);
    assert!(!error.to_string().contains("meta-identity"));
    let requests = server.requests().await;
    for request in &requests[2..] {
        assert_eq!(request.path, "/mint");
        assert_eq!(request.headers["authorization"], "Bearer meta-identity");
        assert_eq!(request.headers["x-api-version"], "1.0.0");
        assert_eq!(request.body, "{}");
    }
}

#[tokio::test]
async fn xai_retains_unrotated_refresh_and_rejects_insecure_verification() {
    let server = Server::start(|_| {
        let mut insecure = device();
        insecure["verification_uri"] = json!("http://login.example/verify");
        vec![
            (200, device()),
            (200, json!({"access_token":"xai-access", "refresh_token":"xai-refresh"})),
            (200, json!({"access_token":"xai-next"})),
            (200, insecure),
        ]
    })
    .await;
    let engine = server.engine(&[("https://auth.x.ai/oauth2/device/code", "/device"), (XAI_TOKEN, "/token")]);
    let credential = login("xai", &LoginOptions::default(), &Ui::default(), &engine).await.expect("login");
    let next = refresh("xai", &credential, &engine).await.expect("refresh");
    assert_eq!(next.access_token, "xai-next");
    assert_eq!(next.refresh_token.as_deref(), Some("xai-refresh"));
    assert!(next.expires_at.expect("expiry") > now_ms() + 3_000_000);
    assert_eq!(
        login("xai", &LoginOptions::default(), &Ui::default(), &engine).await.expect_err("insecure URL"),
        AuthError::InvalidResponse
    );
    let requests = server.requests().await;
    assert_eq!(requests[0].form()["scope"], "openid profile email offline_access grok-cli:access api:access");
    assert_eq!(requests[2].form()["refresh_token"], "xai-refresh");
}

#[tokio::test]
async fn device_denial_does_not_become_a_credential_or_leak_description() {
    let server = Server::start(|_| vec![(400, json!({"error":"access_denied", "error_description":"sensitive vendor detail"}))]).await;
    let engine = server.engine(&[("https://example.test/token", "/token")]);
    let device = DeviceCode { code: "secret".into(), interval: 1, expires: 20 };
    let error = poll_token(&engine, "https://example.test/token", "client", &device, false, false).await.expect_err("denied");
    assert_eq!(error, AuthError::AccessDenied);
    assert!(!error.to_string().contains("sensitive vendor detail"));
    server.requests().await;
}

#[tokio::test]
async fn meta_missing_key_surfaces_only_safe_setup_url() {
    let server =
        Server::start(|_| vec![(200, json!({"action_url":"https://meta.example/setup"})), (200, json!({"action_url":"file:///secret"}))])
            .await;
    let engine = server.engine(&[(META_MINT, "/mint")]);
    let ui = Ui::default();
    assert_eq!(mint_meta(&engine, "identity", Some(&ui)).await.expect_err("setup required"), AuthError::InvalidResponse);
    let event = ui.0.lock().expect("UI events").pop().expect("setup URL");
    assert!(matches!(event, LoginEvent::AuthUrl { url, .. } if url == "https://meta.example/setup"));
    assert_eq!(mint_meta(&engine, "identity", Some(&ui)).await.expect_err("unsafe setup URL"), AuthError::InvalidResponse);
    assert!(ui.0.lock().expect("UI events").is_empty());
    server.requests().await;
}
