#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only

mod browser;
mod engine;
mod providers;
mod source;
mod store;
mod types;

pub use engine::{Engine, Poll, poll_device, response_json};
pub use providers::{PROVIDERS, provider};
pub use source::AuthSource;
pub use store::Store;
pub use types::{AuthError, Credential, InputFuture, LoginEvent, LoginKind, LoginOptions, LoginUi, Protocol, Provider, ResolvedAuth};

/// Login does not modify the store; callers commit only after successful authorization.
pub async fn login(provider: &str, options: &LoginOptions, ui: &dyn LoginUi) -> Result<Credential, AuthError> {
    Engine::new()?.login(provider, options, ui).await
}
