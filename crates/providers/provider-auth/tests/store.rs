#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used)] // unit-test fixtures only

use std::collections::BTreeMap;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use provider_auth::{AuthError, AuthSource, Credential, Engine, LoginKind, LoginOptions, LoginUi, PROVIDERS, Store, login};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

fn credential(access: &str, refresh: &str, expired: bool) -> Credential {
    Credential {
        access_token: access.into(),
        refresh_token: Some(refresh.into()),
        expires_at: if expired { Some(1) } else { None },
        metadata: BTreeMap::new(),
    }
}

fn engine(url: String) -> Engine {
    Engine::with_endpoint_overrides(reqwest::Client::new(), BTreeMap::from([(TOKEN_URL.into(), url)]))
}

async fn request(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + len {
                let body: serde_json::Value = serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap();
                assert_eq!(body["grant_type"], "refresh_token");
                assert_eq!(body["refresh_token"], "old-refresh");
                return stream;
            }
        }
    }
}

async fn reply(mut stream: TcpStream, status: &str, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

const ROTATED: &str = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;

#[tokio::test]
async fn failed_refresh_preserves_credentials_and_redacts_server_body() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    store.set("anthropic", credential("old-access", "old-refresh", true)).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = engine(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move {
        reply(request(&listener).await, "400 Bad Request", r#"{"error":"old-refresh secret echoed"}"#).await;
    });
    let error = store.resolve_with_engine("anthropic", &engine).await.unwrap_err();
    assert_eq!(error, AuthError::Http(400));
    assert!(!format!("{error:?} {error}").contains("old-refresh"));
    let saved = store.get("anthropic").await.unwrap().unwrap();
    assert_eq!(saved.access_token, "old-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("old-refresh"));
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_after_exchange_still_commits_rotating_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    store.set("anthropic", credential("old-access", "old-refresh", true)).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = engine(format!("http://{}", listener.local_addr().unwrap()));
    let worker_store = store.clone();
    let worker = tokio::spawn(async move { worker_store.resolve_with_engine("anthropic", &engine).await });
    let stream = request(&listener).await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    reply(stream, "200 OK", ROTATED).await;
    // get obtains the same process-safe lock and cannot observe an uncommitted refresh.
    let saved = store.get("anthropic").await.unwrap().unwrap();
    assert_eq!(saved.access_token, "new-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
}

#[test]
fn process_refresh_worker() {
    let Some(path) = std::env::var_os("AHO_AUTH_TEST_STORE") else {
        return;
    };
    let url = std::env::var("AHO_AUTH_TEST_ENDPOINT").unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let resolved = Store::new(PathBuf::from(path)).resolve_with_engine("anthropic", &engine(url)).await.unwrap();
        assert_eq!(resolved.token, "new-access");
    });
}

#[tokio::test]
async fn concurrent_processes_exchange_each_refresh_token_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = Store::new(path.clone());
    store.set("anthropic", credential("old-access", "old-refresh", true)).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        reply(request(&listener).await, "200 OK", ROTATED).await;
    });
    let launch = || {
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_refresh_worker", "--nocapture"])
            .env("AHO_AUTH_TEST_STORE", &path)
            .env("AHO_AUTH_TEST_ENDPOINT", &endpoint)
            .spawn()
            .unwrap()
    };
    let mut a = launch();
    let mut b = launch();
    let (a, b) = tokio::join!(a.wait(), b.wait());
    assert!(a.unwrap().success());
    assert!(b.unwrap().success());
    server.await.unwrap();
    let saved = store.get("anthropic").await.unwrap().unwrap();
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
}

#[tokio::test]
async fn independent_writers_preserve_other_provider_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    let (a, b) = tokio::join!(store.set("anthropic", credential("a", "r", false)), store.set("openrouter", credential("b", "r", false)),);
    a.unwrap();
    b.unwrap();
    assert_eq!(store.provider_ids().await.unwrap(), ["anthropic", "openrouter"]);
    store.remove("anthropic").await.unwrap();
    assert_eq!(store.get("openrouter").await.unwrap().unwrap().access_token, "b");
}

#[tokio::test]
async fn private_atomic_files_reject_symlinks_and_preserve_corrupt_stores() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("auth.json");
    let store = Store::new(path.clone());
    store.set("anthropic", credential("a", "r", false)).await.unwrap();
    assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    std::fs::write(&path, b"corrupt").unwrap();
    assert_eq!(store.set("openrouter", credential("b", "r", false)).await, Err(AuthError::Storage));
    assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
    let link = dir.path().join("linked.json");
    symlink(&path, &link).unwrap();
    assert!(Store::new(link).get("anthropic").await.is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.get("anthropic").await.unwrap_err(), AuthError::InsecureStorage);
}

#[tokio::test]
async fn long_lived_source_reads_updated_store_and_debug_redacts_all_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = Store::new(path.clone());
    let source = AuthSource::from_key("openrouter", &format!("oauth:{}", path.display()));
    let client = reqwest::Client::new();
    store.set("openrouter", credential("first-secret", "refresh-secret", false)).await.unwrap();
    assert_eq!(source.resolve(&client).await.unwrap().token, "first-secret");
    store.set("openrouter", credential("second-secret", "refresh-secret", false)).await.unwrap();
    let mut resolved = source.resolve(&client).await.unwrap();
    resolved.headers.insert("private".into(), "header-secret".into());
    let debug = format!("{:?} {:?} {:?}", source, resolved, store.get("openrouter").await.unwrap());
    for secret in ["second-secret", "refresh-secret", "header-secret"] {
        assert!(!debug.contains(secret));
    }
    assert_eq!(resolved.token, "second-secret");
    store.remove("openrouter").await.unwrap();
    assert_eq!(source.resolve(&client).await.unwrap_err(), AuthError::MissingCredential);
}

#[test]
fn ollama_cloud_is_an_api_key_provider_without_oauth_calls() {
    let provider = PROVIDERS.iter().find(|p| p.id == "ollama-cloud").expect("ollama-cloud registered");
    let LoginKind::ApiKey { instructions_url } = provider.kind else { panic!("expected api-key login") };
    assert!(instructions_url.starts_with("https://ollama.com/"));
    assert_eq!(provider.default_base_url, "https://ollama.com/v1");
}

struct PastesKey(String);

impl LoginUi for PastesKey {
    fn event(&self, _: provider_auth::LoginEvent) {}
    fn input<'a>(&'a self, _: &'a str) -> provider_auth::InputFuture<'a> {
        let answer = self.0.clone();
        Box::pin(async move { Ok(answer) })
    }
}

#[tokio::test]
async fn ollama_cloud_login_pastes_key_into_credential_and_needs_no_refresh() {
    let ui = PastesKey("  ok-pasted \n".into());
    let engine = Engine::new().unwrap();
    let credential = login("ollama-cloud", &LoginOptions::default(), &ui).await.unwrap();
    assert_eq!(credential.access_token, "ok-pasted");
    assert_eq!(credential.refresh_token, None);
    assert!(!format!("{credential:?}").contains("ok-pasted"), "debug never prints the key");
    let empty = login("ollama-cloud", &LoginOptions::default(), &PastesKey("  \n".into())).await.unwrap_err();
    assert_eq!(empty, AuthError::InvalidInput);
    // A pasted key never expires, so store resolution must not attempt a refresh call.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    store.set("ollama-cloud", credential).await.unwrap();
    let resolved = store.resolve_with_engine("ollama-cloud", &engine).await.unwrap();
    assert_eq!(resolved.token, "ok-pasted");
    assert!(!resolved.oauth);
    assert_eq!(resolved.base_url, None);
    assert_eq!(store.get("ollama-cloud").await.unwrap().unwrap().access_token, "ok-pasted");
}
