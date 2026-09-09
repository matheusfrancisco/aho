use std::fmt;
use std::path::PathBuf;

use crate::{AuthError, ResolvedAuth, Store};

/// Configuration holds only `oauth:/absolute/path/auth.json`, never OAuth tokens.
#[derive(Clone)]
pub struct AuthSource {
    provider: String,
    value: Source,
}

#[derive(Clone)]
enum Source {
    Key(String),
    OAuth(PathBuf),
}

impl AuthSource {
    pub fn from_key(provider: &str, value: &str) -> Self {
        let value = match value.strip_prefix("oauth:") {
            Some(path) => Source::OAuth(PathBuf::from(path)),
            None => Source::Key(value.to_owned()),
        };
        Self { provider: provider.to_owned(), value }
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self.value, Source::OAuth(_))
    }

    pub async fn resolve(&self, client: &reqwest::Client) -> Result<ResolvedAuth, AuthError> {
        match &self.value {
            Source::Key(token) => Ok(ResolvedAuth { token: token.clone(), headers: Default::default(), base_url: None, oauth: false }),
            Source::OAuth(path) => Store::new(path.clone()).resolve(&self.provider, client).await,
        }
    }
}

impl fmt::Debug for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthSource")
            .field("provider", &self.provider)
            .field("value", &"[REDACTED]")
            .field("oauth", &self.is_oauth())
            .finish()
    }
}
