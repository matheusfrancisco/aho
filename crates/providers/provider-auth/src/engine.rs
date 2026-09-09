use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::Value;

use crate::{AuthError, Credential, LoginOptions, LoginUi};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Native authentication transport. Overrides are a library-only testing seam;
/// production callers use `new`, and the CLI does not expose endpoint overrides.
#[derive(Clone)]
pub struct Engine {
    client: Client,
    endpoints: BTreeMap<String, String>,
}

impl Engine {
    #[allow(clippy::disallowed_methods, reason = "authority: auth-http-client")]
    pub fn new() -> Result<Self, AuthError> {
        static CLIENT: LazyLock<Result<Client, AuthError>> = LazyLock::new(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| AuthError::Network)
        });
        let client = CLIENT.clone()?;
        Ok(Self::with_endpoint_overrides(client, BTreeMap::new()))
    }

    pub fn with_endpoint_overrides(client: Client, endpoints: BTreeMap<String, String>) -> Self {
        Self { client, endpoints }
    }

    pub fn url(&self, canonical: &str) -> String {
        self.endpoints.get(canonical).map_or_else(|| canonical.to_owned(), Clone::clone)
    }

    pub fn request(&self, method: Method, canonical: &str) -> RequestBuilder {
        self.client.request(method, self.url(canonical)).timeout(REQUEST_TIMEOUT)
    }

    #[allow(clippy::disallowed_methods, reason = "authority: auth-http-send")]
    pub async fn send(&self, request: RequestBuilder) -> Result<Response, AuthError> {
        request.send().await.map_err(network_error)
    }

    pub async fn send_json(&self, request: RequestBuilder) -> Result<Value, AuthError> {
        let response = self.send(request).await?;
        if !response.status().is_success() {
            return Err(AuthError::Http(response.status().as_u16()));
        }
        response_json(response).await
    }

    /// Dropping this future cancels HTTP requests, polling, and callback listeners.
    pub async fn login(&self, provider: &str, options: &LoginOptions, ui: &dyn LoginUi) -> Result<Credential, AuthError> {
        if crate::provider(provider).is_none() {
            return Err(AuthError::UnsupportedProvider);
        }
        if (options.device_code && matches!(provider, "anthropic" | "openrouter"))
            || (options.gateway.is_some() && provider != "radius")
            || (options.enterprise_domain.is_some() && provider != "github-copilot")
        {
            return Err(AuthError::InvalidInput);
        }
        let timeout = options.timeout.min(LOGIN_TIMEOUT);
        tokio::time::timeout(timeout, crate::providers::login(provider, options, ui, self)).await.map_err(|_| AuthError::Timeout)?
    }

    pub async fn refresh(&self, provider: &str, credential: &Credential) -> Result<Credential, AuthError> {
        tokio::time::timeout(Duration::from_secs(90), crate::providers::refresh(provider, credential, self))
            .await
            .map_err(|_| AuthError::Timeout)?
    }
}

pub(crate) fn network_error(error: reqwest::Error) -> AuthError {
    if error.is_timeout() { AuthError::Timeout } else { AuthError::Network }
}

pub async fn response_json(mut response: Response) -> Result<Value, AuthError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(AuthError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| AuthError::InvalidResponse)
}

pub enum Poll<T> {
    Complete(T),
    Pending,
    SlowDown(Option<Duration>),
}

/// RFC 8628 polling with one bounded deadline covering sleeps and requests.
pub async fn poll_device<T, F, Fut>(interval: Duration, expires: Duration, wait_before_first: bool, mut poll: F) -> Result<T, AuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>, AuthError>>,
{
    let mut interval = interval.max(Duration::from_secs(1));
    tokio::time::timeout(expires.min(LOGIN_TIMEOUT), async {
        if wait_before_first {
            tokio::time::sleep(interval).await;
        }
        loop {
            match poll().await? {
                Poll::Complete(value) => return Ok(value),
                Poll::Pending => {}
                Poll::SlowDown(server_interval) => {
                    interval = server_interval.unwrap_or_else(|| interval.saturating_add(Duration::from_secs(5))).max(interval);
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .map_err(|_| AuthError::Timeout)?
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().try_into().unwrap_or(u64::MAX)
}

pub(crate) fn required_string<'a>(body: &'a Value, name: &str) -> Result<&'a str, AuthError> {
    body.get(name).and_then(Value::as_str).filter(|v| !v.is_empty()).ok_or(AuthError::InvalidResponse)
}

pub(crate) fn token_credential(body: &Value, previous: Option<&Credential>) -> Result<Credential, AuthError> {
    let access_token = required_string(body, "access_token")?.to_owned();
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| previous.and_then(|old| old.refresh_token.clone()));
    if refresh_token.is_none() {
        return Err(AuthError::InvalidResponse);
    }
    let lifetime = body.get("expires_in").and_then(Value::as_u64).filter(|seconds| *seconds > 0).ok_or(AuthError::InvalidResponse)?;
    Ok(Credential {
        access_token,
        refresh_token,
        expires_at: Some(now_ms().saturating_add(lifetime.saturating_mul(1000))),
        metadata: previous.map(|old| old.metadata.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refresh_rotation_preserves_old_token_only_when_server_omits_replacement() {
        let previous = Credential {
            access_token: "old".into(),
            refresh_token: Some("refresh-old".into()),
            expires_at: Some(1),
            metadata: BTreeMap::from([("account_id".into(), json!("account"))]),
        };
        let retained = token_credential(&json!({"access_token":"new","expires_in":3600}), Some(&previous)).unwrap();
        assert_eq!(retained.refresh_token.as_deref(), Some("refresh-old"));
        assert_eq!(retained.metadata["account_id"], "account");
        let rotated =
            token_credential(&json!({"access_token":"new","refresh_token":"refresh-new","expires_in":3600}), Some(&previous)).unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(previous.refresh_token.as_deref(), Some("refresh-old"));
        assert!(token_credential(&json!({"refresh_token":"secret","expires_in":3600}), Some(&previous)).is_err());
        assert!(token_credential(&json!({"access_token":"new","refresh_token":"secret","expires_in":-1}), None).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn slow_down_honors_server_minimum_and_deadline() {
        let start = tokio::time::Instant::now();
        let mut polls = 0;
        let value = poll_device(Duration::from_secs(1), Duration::from_secs(30), true, || {
            polls += 1;
            let result = match polls {
                1 => Poll::SlowDown(Some(Duration::from_secs(8))),
                2 => Poll::SlowDown(Some(Duration::from_secs(2))),
                _ => Poll::Complete("authorized"),
            };
            std::future::ready(Ok(result))
        })
        .await
        .unwrap();
        assert_eq!(value, "authorized");
        assert_eq!(start.elapsed(), Duration::from_secs(17));
        let mut polls = 0;
        let error = poll_device::<(), _, _>(Duration::from_secs(1), Duration::from_secs(5), true, || {
            polls += 1;
            std::future::ready(Ok(Poll::SlowDown(None)))
        })
        .await
        .unwrap_err();
        assert_eq!(error, AuthError::Timeout);
        assert_eq!(polls, 1);
    }
}
