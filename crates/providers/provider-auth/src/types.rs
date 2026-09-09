use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A single provider account. Expiry is milliseconds since the Unix epoch.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
            .field("expires_at", &self.expires_at)
            .field("metadata", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
pub struct ResolvedAuth {
    pub token: String,
    pub headers: BTreeMap<String, String>,
    pub base_url: Option<String>,
    pub oauth: bool,
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedAuth")
            .field("token", &"[REDACTED]")
            .field("headers", &"[REDACTED]")
            .field("base_url", &self.base_url.as_ref().map(|_| "[REDACTED]"))
            .field("oauth", &self.oauth)
            .finish()
    }
}

/// Errors intentionally exclude response bodies, request URLs and credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    UnsupportedProvider,
    MissingCredential,
    InvalidCredential,
    InvalidResponse,
    InvalidInput,
    StateMismatch,
    AccessDenied,
    Cancelled,
    Timeout,
    Network,
    Http(u16),
    Storage,
    InsecureStorage,
    CallbackUnavailable,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProvider => f.write_str("unsupported OAuth provider"),
            Self::MissingCredential => f.write_str("not logged in; run aho login"),
            Self::InvalidCredential => f.write_str("invalid OAuth credential; run aho login again"),
            Self::InvalidResponse => f.write_str("authentication server returned an invalid response"),
            Self::InvalidInput => f.write_str("invalid authentication input"),
            Self::StateMismatch => f.write_str("OAuth state mismatch"),
            Self::AccessDenied => f.write_str("authorization was denied"),
            Self::Cancelled => f.write_str("login cancelled"),
            Self::Timeout => f.write_str("authentication timed out"),
            Self::Network => f.write_str("authentication network request failed"),
            Self::Http(status) => write!(f, "authentication request failed (HTTP {status})"),
            Self::Storage => f.write_str("could not read or persist authentication store"),
            Self::InsecureStorage => f.write_str("authentication store must be a private regular file in a private directory"),
            Self::CallbackUnavailable => f.write_str("could not bind OAuth loopback callback listener"),
        }
    }
}

impl std::error::Error for AuthError {}

#[derive(Clone, Debug)]
pub struct LoginOptions {
    pub device_code: bool,
    pub gateway: Option<String>,
    pub enterprise_domain: Option<String>,
    pub timeout: Duration,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self { device_code: false, gateway: None, enterprise_domain: None, timeout: Duration::from_secs(15 * 60) }
    }
}

/// These events intentionally carry authorization URLs/codes for display, not logging.
#[derive(Clone)]
pub enum LoginEvent {
    AuthUrl { url: String, instructions: String },
    DeviceCode { user_code: String, verification_uri: String, verification_uri_complete: Option<String>, expires_in: u64, interval: u64 },
    Progress { message: String },
}

pub type InputFuture<'a> = Pin<Box<dyn Future<Output = Result<String, AuthError>> + Send + 'a>>;

/// Input futures must be cancellation-safe: browser completion drops the pending input.
pub trait LoginUi: Send + Sync {
    fn event(&self, event: LoginEvent);
    fn input<'a>(&'a self, prompt: &'a str) -> InputFuture<'a>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    PiMessages,
}

#[derive(Clone, Copy, Debug)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    pub default_base_url: &'static str,
}
