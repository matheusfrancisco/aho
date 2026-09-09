#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::collections::BTreeMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use provider_auth::{AuthError, Engine, InputFuture, LoginEvent, LoginOptions, LoginUi};
use ring::digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct ManualUi(Mutex<Option<reqwest::Url>>);

impl LoginUi for ManualUi {
    fn event(&self, event: LoginEvent) {
        if let LoginEvent::AuthUrl { url, .. } = event {
            *self.0.lock().unwrap() = Some(reqwest::Url::parse(&url).unwrap());
        }
    }

    fn input<'a>(&'a self, _prompt: &'a str) -> InputFuture<'a> {
        Box::pin(async move {
            let state = self.0.lock().unwrap();
            let authorize = state.as_ref().unwrap();
            let callback = authorize.query_pairs().find(|(name, _)| name == "callback_url").unwrap().1;
            Ok(format!("{callback}?code=authorized-code"))
        })
    }
}

#[tokio::test]
async fn openrouter_exchanges_manual_callback_with_matching_pkce_for_nonexpiring_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/auth/keys", listener.local_addr().unwrap());
    let engine = Engine::with_endpoint_overrides(
        reqwest::Client::new(),
        BTreeMap::from([("https://openrouter.ai/api/v1/auth/keys".into(), endpoint)]),
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let body = loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + size {
                    break serde_json::from_slice::<serde_json::Value>(&bytes[end + 4..end + 4 + size]).unwrap();
                }
            }
        };
        assert_eq!(body["code"], "authorized-code");
        assert_eq!(body["code_challenge_method"], "S256");
        let reply = r#"{"key":"permanent-user-controlled-key"}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        body["code_verifier"].as_str().unwrap().to_owned()
    });
    let ui = ManualUi(Mutex::new(None));
    let credential = engine.login("openrouter", &LoginOptions::default(), &ui).await.unwrap();
    let verifier = server.await.unwrap();
    let authorize = ui.0.lock().unwrap().clone().unwrap();
    let challenge = authorize.query_pairs().find(|(name, _)| name == "code_challenge").unwrap().1.into_owned();
    assert_eq!(URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes())), challenge);
    assert_eq!(credential.access_token, "permanent-user-controlled-key");
    assert_eq!(credential.refresh_token, None);
    assert_eq!(credential.expires_at, None);
    let callback = authorize.query_pairs().find(|(name, _)| name == "callback_url").unwrap().1.into_owned();
    let callback = reqwest::Url::parse(&callback).unwrap();
    assert!(TcpListener::bind(("127.0.0.1", callback.port().unwrap())).await.is_ok());
}

#[tokio::test]
async fn unsupported_login_options_do_not_start_authentication() {
    let engine = Engine::new().unwrap();
    let ui = ManualUi(Mutex::new(None));
    let options = LoginOptions { device_code: true, ..LoginOptions::default() };
    assert_eq!(engine.login("openrouter", &options, &ui).await.unwrap_err(), AuthError::InvalidInput);
    assert!(ui.0.lock().unwrap().is_none());
}
