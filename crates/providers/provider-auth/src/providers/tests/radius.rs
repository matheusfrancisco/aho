use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest;

use super::super::device::test_server::{Server, Ui};
use super::*;
use crate::InputFuture;

fn config(base: &str, suffix: &str) -> Value {
    json!({"baseUrl":format!("{base}/{suffix}"), "models":[
        {"id":"anthropic/claude-sonnet", "name":"Sonnet", "reasoning":true,
            "thinkingLevelMap":{"high":16384}, "input":["text","image"],
            "cost":{"input":3,"output":15,"cacheRead":0.3,"cacheWrite":3.75},
            "contextWindow":200000, "maxTokens":32000},
        {"id":"malformed-entry"}
    ]})
}

#[tokio::test]
async fn device_login_keeps_gateway_origin_and_refreshes_inference_metadata() {
    let server = Server::start(|base| vec![
        (200, json!({"device_code":"radius-device", "user_code":"RADIUS", "verification_uri":"https://login.example/device", "interval":1, "expires_in":60})),
        (200, json!({"access_token":"radius-one", "refresh_token":"radius-refresh", "expires_in":3600, "scope":"gateway offline_access"})),
        (200, config(base, "api/first")),
        (200, json!({"access_token":"radius-two", "refresh_token":"radius-rotated", "expires_in":3600, "scope":"gateway offline_access"})),
        (200, config(base, "api/second")),
    ]).await;
    let engine = server.engine(&[]);
    let options = LoginOptions { gateway: Some(format!("{}/some/path/", server.base)), device_code: true, ..LoginOptions::default() };
    let credential = login(&options, &Ui::default(), &engine).await.expect("device login");
    assert_eq!(credential.metadata["gateway"], json!(format!("{}/some/path", server.base)));
    assert_eq!(credential.metadata["scope"], "gateway offline_access");
    assert_eq!(credential.metadata["gateway_config"]["models"], json!([config(&server.base, "unused")["models"][0].clone()]));
    assert_eq!(resolve(&credential).expect("resolve").base_url, Some(format!("{}/api/first", server.base)));
    let next = refresh(&credential, &engine).await.expect("refresh");
    assert_eq!(next.refresh_token.as_deref(), Some("radius-rotated"));
    assert_eq!(resolve(&next).expect("resolve after refresh").base_url, Some(format!("{}/api/second", server.base)));
    let requests = server.requests().await;
    assert_eq!(
        requests.iter().map(|request| request.path.as_str()).collect::<Vec<_>>(),
        ["/v1/oauth/device", "/v1/oauth/token", "/v1/config", "/v1/oauth/token", "/v1/config"]
    );
    assert_eq!(requests[0].form()["scope"], SCOPE);
    assert_eq!(requests[1].form()["device_code"], "radius-device");
    assert_eq!(requests[2].headers["authorization"], "Bearer radius-one");
    assert_eq!(requests[3].form()["refresh_token"], "radius-refresh");
    assert_eq!(requests[4].headers["authorization"], "Bearer radius-two");
}

#[derive(Default)]
struct BrowserUi {
    parameters: Mutex<Option<BTreeMap<String, String>>>,
    callback: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LoginUi for BrowserUi {
    fn event(&self, event: LoginEvent) {
        if let LoginEvent::AuthUrl { url, .. } = event {
            let url = reqwest::Url::parse(&url).expect("authorization URL");
            assert_eq!(url.host_str(), Some("identity.example"));
            let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
            let mut callback = reqwest::Url::parse(&params["redirect_uri"]).expect("redirect URL");
            let state = params["state"].clone();
            *self.parameters.lock().expect("authorization params") = Some(params);
            *self.callback.lock().expect("callback task") = Some(tokio::spawn(async move {
                let client = reqwest::Client::builder().no_proxy().build().expect("callback client");
                callback.query_pairs_mut().extend_pairs([("code", "bad-code"), ("state", "wrong-state")]);
                assert_eq!(client.get(callback.clone()).send().await.expect("wrong-state callback").status(), 400);
                callback.set_query(None);
                callback.query_pairs_mut().extend_pairs([("code", "radius-code"), ("state", state.as_str())]);
                assert_eq!(client.get(callback).send().await.expect("valid callback").status(), 200);
            }));
        }
    }
    fn input<'a>(&'a self, _: &'a str) -> InputFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn browser_discovery_uses_pkce_and_rejects_wrong_state_before_exchange() {
    let server = Server::start(|base| {
        vec![
            (200, json!({"authorizationEndpoint":"https://identity.example/authorize?stale=removed"})),
            (200, json!({"access_token":"browser-access", "refresh_token":"browser-refresh", "expires_in":3600})),
            (200, config(base, "api")),
        ]
    })
    .await;
    let engine = server.engine(&[]);
    let ui = BrowserUi::default();
    let mut credential = tokio::time::timeout(Duration::from_secs(10), browser_login(&engine, &server.base, &ui, 0))
        .await
        .expect("login deadline")
        .expect("browser login");
    gateway_config(&engine, &server.base, &mut credential).await.expect("gateway config");
    let callback = ui.callback.lock().expect("callback").take().expect("callback task");
    callback.await.expect("callback assertions");
    let params = ui.parameters.lock().expect("parameters").take().expect("authorization parameters");
    assert_eq!(params["client_id"], CLIENT_ID);
    assert_eq!(params["scope"], SCOPE);
    assert_eq!(params["handoff"], "url");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(!params.contains_key("stale"));
    let requests = server.requests().await;
    assert_eq!(requests[0].path, "/v1/oauth");
    let exchange = requests[1].form();
    assert_eq!(exchange["code"], "radius-code");
    assert_eq!(exchange["redirect_uri"], params["redirect_uri"]);
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, exchange["code_verifier"].as_bytes())), params["code_challenge"]);
    assert_eq!(credential.access_token, "browser-access");
}

#[tokio::test]
async fn gateway_config_rejects_non_http_inference_endpoint() {
    let server = Server::start(|_| vec![(200, json!({"baseUrl":"file:///secret", "models":[]}))]).await;
    let engine = server.engine(&[]);
    let mut credential = Credential {
        access_token: "radius-access".into(),
        refresh_token: Some("radius-refresh".into()),
        expires_at: Some(1),
        metadata: BTreeMap::new(),
    };
    assert_eq!(gateway_config(&engine, &server.base, &mut credential).await.expect_err("unsafe inference URL"), AuthError::InvalidResponse);
    assert!(credential.metadata.get("base_url").is_none());
    server.requests().await;
}
