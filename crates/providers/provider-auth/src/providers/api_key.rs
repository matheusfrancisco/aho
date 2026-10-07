use crate::{AuthError, Credential, LoginEvent, LoginUi};

pub(super) async fn login(ui: &dyn LoginUi) -> Result<Credential, AuthError> {
    ui.event(LoginEvent::Progress { message: "Create an API key at https://ollama.com/settings/keys, then paste it below.".into() });
    let key = ui.input("API key: ").await?;
    let key = key.trim();
    if key.is_empty() {
        return Err(AuthError::InvalidInput);
    }
    Ok(Credential { access_token: key.to_owned(), refresh_token: None, expires_at: None, metadata: Default::default() })
}
