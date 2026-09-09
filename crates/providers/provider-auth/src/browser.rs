use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{AuthError, LoginUi};

pub(crate) fn random_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| AuthError::InvalidInput)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self, AuthError> {
        let verifier = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        Ok(Self { verifier, challenge })
    }
}

/// No detached task owns this socket: dropping the login future closes it immediately.
pub(crate) struct Callback {
    listener: TcpListener,
    path: String,
    state: Option<String>,
    redirect_uri: String,
}

impl Callback {
    #[allow(clippy::disallowed_methods, reason = "authority: auth-callback-listener")]
    pub async fn bind(port: u16, path: String, state: Option<String>) -> Result<Self, AuthError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await.map_err(|_| AuthError::CallbackUnavailable)?;
        let address = listener.local_addr().map_err(|_| AuthError::CallbackUnavailable)?;
        let redirect_uri = format!("http://127.0.0.1:{}{path}", address.port());
        Ok(Self { listener, path, state, redirect_uri })
    }

    pub fn redirect_uri(&self) -> String {
        self.redirect_uri.clone()
    }

    pub async fn code(&self, ui: &dyn LoginUi) -> Result<String, AuthError> {
        tokio::select! {
            result = self.receive() => result,
            input = ui.input("Complete browser sign-in, or paste the authorization code / redirect URL:") => {
                parse_input(&input?, &self.path, self.state.as_deref())
            },
        }
    }

    async fn receive(&self) -> Result<String, AuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(|_| AuthError::CallbackUnavailable)?;
            let result = tokio::time::timeout(Duration::from_secs(5), self.read_callback(&mut stream)).await;
            if let Ok(Ok(Some(code))) = result {
                return Ok(code);
            }
            if matches!(result, Ok(Err(AuthError::AccessDenied))) {
                return Err(AuthError::AccessDenied);
            }
        }
    }

    async fn read_callback(&self, stream: &mut TcpStream) -> Result<Option<String>, AuthError> {
        let mut bytes = [0u8; 8192];
        let mut used = 0;
        let line = loop {
            if used == bytes.len() {
                respond(stream, "400 Bad Request", "Invalid callback request.").await?;
                return Ok(None);
            }
            let count = stream.read(&mut bytes[used..]).await.map_err(|_| AuthError::Network)?;
            if count == 0 {
                return Ok(None);
            }
            used += count;
            if let Some(end) = bytes[..used].windows(2).position(|w| w == b"\r\n") {
                break std::str::from_utf8(&bytes[..end]).map_err(|_| AuthError::InvalidInput)?;
            }
        };
        let mut parts = line.split_whitespace();
        if parts.next() != Some("GET") {
            respond(stream, "405 Method Not Allowed", "Expected GET.").await?;
            return Ok(None);
        }
        let Some(target) = parts.next().filter(|target| target.starts_with('/')) else {
            return Ok(None);
        };
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).map_err(|_| AuthError::InvalidInput)?;
        if url.path() != self.path {
            respond(stream, "404 Not Found", "Callback not found.").await?;
            return Ok(None);
        }
        match parse_url(&url, &self.path, self.state.as_deref()) {
            Ok(code) => {
                respond(stream, "200 OK", "Authorization received. Return to aho to finish sign-in.").await?;
                Ok(Some(code))
            }
            Err(AuthError::AccessDenied) => {
                respond(stream, "400 Bad Request", "Authorization was denied.").await?;
                Err(AuthError::AccessDenied)
            }
            Err(_) => {
                respond(stream, "400 Bad Request", "Invalid callback code or state.").await?;
                Ok(None)
            }
        }
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.map_err(|_| AuthError::Network)?;
    stream.shutdown().await.map_err(|_| AuthError::Network)
}

fn parse_url(url: &reqwest::Url, path: &str, expected_state: Option<&str>) -> Result<String, AuthError> {
    if url.path() != path {
        return Err(AuthError::InvalidInput);
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    if let Some(expected) = expected_state {
        let mut states = pairs.iter().filter(|(name, _)| name == "state");
        if states.next().map(|(_, value)| value.as_ref()) != Some(expected) || states.next().is_some() {
            return Err(AuthError::StateMismatch);
        }
    }
    if pairs.iter().any(|(name, _)| name == "error") {
        return Err(AuthError::AccessDenied);
    }
    let mut codes = pairs.iter().filter(|(name, _)| name == "code");
    let code = codes.next().map(|(_, code)| code.as_ref()).filter(|v| !v.is_empty()).ok_or(AuthError::InvalidInput)?;
    if codes.next().is_some() {
        return Err(AuthError::InvalidInput);
    }
    Ok(code.to_owned())
}

fn parse_input(input: &str, path: &str, expected_state: Option<&str>) -> Result<String, AuthError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(AuthError::InvalidInput);
    }
    if let Ok(url) = reqwest::Url::parse(input) {
        return parse_url(&url, path, expected_state);
    }
    if input.contains("code=") {
        let url = reqwest::Url::parse(&format!("http://localhost{path}?{}", input.trim_start_matches('?')))
            .map_err(|_| AuthError::InvalidInput)?;
        return parse_url(&url, path, expected_state);
    }
    if let Some((code, state)) = input.split_once('#') {
        if expected_state.is_some_and(|expected| expected != state) {
            return Err(AuthError::StateMismatch);
        }
        return if code.is_empty() { Err(AuthError::InvalidInput) } else { Ok(code.to_owned()) };
    }
    // Explicit manual code entry is trusted user input; PKCE still binds its exchange.
    Ok(input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_required_and_duplicate_parameters_are_rejected() {
        assert_eq!(parse_input("http://localhost/callback?code=x", "/callback", Some("s")), Err(AuthError::StateMismatch));
        assert_eq!(parse_input("code=x&state=wrong", "/callback", Some("s")), Err(AuthError::StateMismatch));
        assert_eq!(parse_input("code=x&state=s&state=s", "/callback", Some("s")), Err(AuthError::StateMismatch));
        assert_eq!(parse_input("code=x&code=y&state=s", "/callback", Some("s")), Err(AuthError::InvalidInput));
        assert_eq!(parse_input("x#s", "/callback", Some("s")), Ok("x".into()));
        assert_eq!(parse_input("code=x&state=s", "/callback", Some("s")), Ok("x".into()));
    }

    #[tokio::test]
    async fn callback_ignores_invalid_state_and_releases_listener_on_drop() {
        let callback = Callback::bind(0, "/callback".into(), Some("expected".into())).await.unwrap();
        let address = callback.listener.local_addr().unwrap();
        let client = reqwest::Client::new();
        let receive = callback.receive();
        let requests = async {
            let bad = client.get(format!("http://{address}/callback?code=bad&state=wrong")).send().await.unwrap();
            assert_eq!(bad.status(), 400);
            let good = client.get(format!("http://{address}/callback?code=good&state=expected")).send().await.unwrap();
            assert_eq!(good.status(), 200);
        };
        let (result, ()) = tokio::join!(receive, requests);
        assert_eq!(result.unwrap(), "good");
        drop(callback);
        assert!(TcpListener::bind(address).await.is_ok());
    }

    #[tokio::test]
    async fn cancellation_drops_listener_while_waiting_for_browser_and_manual_input() {
        struct PendingUi;
        impl LoginUi for PendingUi {
            fn event(&self, _event: crate::LoginEvent) {}
            fn input<'a>(&'a self, _prompt: &'a str) -> crate::InputFuture<'a> {
                Box::pin(std::future::pending())
            }
        }
        let callback = Callback::bind(0, "/callback".into(), Some("state".into())).await.unwrap();
        let address = callback.listener.local_addr().unwrap();
        let result = tokio::time::timeout(Duration::from_millis(10), async move { callback.code(&PendingUi).await }).await;
        assert!(result.is_err());
        assert!(TcpListener::bind(address).await.is_ok());
    }
}
