//! Operator-only account login. OAuth tokens stay in auth.json, never in configuration or logs.

mod terminal;

use aho_types::Value;
use provider_auth::{LoginKind, LoginOptions, LoginUi, PROVIDERS, Store};
use surface_rpc::caps;

use crate::cli::LoginArgs;
use crate::ctx::{Ctx, Fail, err};
use terminal::Terminal;

pub async fn login(ctx: &Ctx, args: &LoginArgs) -> Result<(), Fail> {
    if args.list {
        for p in PROVIDERS {
            let hint = match p.kind {
                LoginKind::OAuth => String::new(),
                LoginKind::ApiKey { instructions_url } => format!(" (paste API key from {instructions_url})"),
            };
            println!("{:<18} {}{hint}", p.id, p.name);
        }
        return Ok(());
    }
    let ui = Terminal::new(!args.no_browser);
    let id = choose(args.provider.as_deref(), PROVIDERS.iter().map(|p| p.id).collect(), &ui).await?;
    let provider = provider_auth::provider(&id).ok_or_else(|| err(format!("unknown provider `{id}`; use `aho login --list`")))?;
    if matches!(provider.kind, LoginKind::ApiKey { .. }) {
        return api_key_login(ctx, args, &ui, &id, provider.kind).await;
    }
    if args.gateway.is_some() && id != "radius" {
        return Err(err("--gateway is only supported for radius"));
    }
    if args.enterprise_domain.is_some() && id != "github-copilot" {
        return Err(err("--enterprise-domain is only supported for github-copilot"));
    }
    if args.device_code && matches!(id.as_str(), "anthropic" | "openrouter") {
        return Err(err(format!("`{id}` supports browser login only; omit --device-code")));
    }
    let options = LoginOptions {
        device_code: args.device_code,
        gateway: args.gateway.clone(),
        enterprise_domain: args.enterprise_domain.clone(),
        ..LoginOptions::default()
    };
    let inputs = ctx.inputs(&[], false)?;
    let store_path = inputs.home.join(".aho").join("auth.json");
    let key = format!("provider.{id}.key");
    let reference = format!("oauth:{}", store_path.display());
    let changes = vec![(key, Value::String(reference))];
    // Validate configuration before starting browser/network work. Login is explicit consent
    // to store this account; unlike a general config mutation, it needs no second apply prompt.
    let proposal = caps::propose_in(&ctx.comp, &inputs, inputs.home_config(), changes)?;
    if id == "anthropic" {
        println!(
            "Anthropic account authentication does not guarantee Claude plan coverage; check the provider's extra-usage billing policy."
        );
    }
    let credential = tokio::select! {
        result = provider_auth::login(&id, &options, &ui) => result.map_err(|e| err(e.to_string()))?,
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|e| err(format!("could not listen for cancellation: {e}")))?;
            return Err(err("login cancelled; no credentials were stored"));
        }
    };
    Store::new(store_path.clone()).set(&id, credential).await.map_err(|e| err(e.to_string()))?;
    let written = proposal
        .commit()
        .map_err(|e| err(format!("account saved, but route configuration could not be written: {e}; run `aho login {id}` again")))?;
    println!("Logged in to {}. Credentials saved to {}", provider.name, store_path.display());
    for line in written {
        println!("{line}");
    }
    crate::record::record_change(ctx, &proposal.after, None, "login", vec![id.clone()]).await?;
    println!("Select this route with `aho set model.provider {id}` and a supported model with `aho set model.name <model-id>`.");
    Ok(())
}

pub async fn logout(ctx: &Ctx, requested: Option<&str>) -> Result<(), Fail> {
    if let Some(id) = requested
        && let Some(provider) = provider_auth::provider(id)
        && matches!(provider.kind, LoginKind::ApiKey { .. })
    {
        return api_key_logout(ctx, id).await;
    }
    let inputs = ctx.inputs(&[], false)?;
    let path = inputs.home.join(".aho").join("auth.json");
    let store = Store::new(path.clone());
    let stored = store.provider_ids().await.map_err(|e| err(e.to_string()))?;
    let ui = Terminal::new(false);
    let id = choose(requested, stored.iter().map(String::as_str).collect(), &ui).await?;
    if provider_auth::provider(&id).is_none() {
        return Err(err(format!("unknown provider `{id}`; use `aho login --list`")));
    }
    let home = inputs.home_config();
    let home_text = inputs.files.iter().find(|f| f.path == home.display().to_string()).map(|f| f.text.as_str()).unwrap_or("");
    let home_value: toml::Value = toml::from_str(home_text).map_err(|e| err(format!("invalid home configuration: {e}")))?;
    let reference = format!("oauth:{}", path.display());
    let owns_reference = home_value.get("provider").and_then(|p| p.get(&id)).and_then(|p| p.get("key")).and_then(toml::Value::as_str)
        == Some(reference.as_str());
    // An empty home key disables only this login reference; explicit API keys elsewhere remain.
    let proposal = if owns_reference {
        Some(caps::propose_in(&ctx.comp, &inputs, home, vec![(format!("provider.{id}.key"), Value::String(String::new()))])?)
    } else {
        None
    };
    let removed = store.remove(&id).await.map_err(|e| err(e.to_string()))?;
    if let Some(proposal) = proposal {
        for line in proposal.commit()? {
            println!("{line}");
        }
        crate::record::record_change(ctx, &proposal.after, None, "logout", vec![id.clone()]).await?;
    }
    if removed {
        println!("Logged out of {id} locally. Provider grants are not revoked; configured API keys are unchanged.");
    } else {
        println!("No stored account for {id}. Configured API keys are unchanged.");
    }
    Ok(())
}

/// Api-key providers have no OAuth grant; the operator pastes a key from the provider's site.
/// The key goes to the route's `provider.<id>.key` in home config, not the OAuth auth.json store.
async fn api_key_login(ctx: &Ctx, args: &LoginArgs, ui: &Terminal, id: &str, kind: LoginKind) -> Result<(), Fail> {
    let LoginKind::ApiKey { instructions_url } = kind else { return Err(err("provider does not use an API key")) };
    if args.gateway.is_some() {
        return Err(err("--gateway is only supported for radius"));
    }
    if args.enterprise_domain.is_some() {
        return Err(err("--enterprise-domain is only supported for github-copilot"));
    }
    if args.device_code {
        return Err(err("api-key login takes no --device-code"));
    }
    ui.event(provider_auth::LoginEvent::Progress { message: format!("Create an API key at {instructions_url}, then paste it below.") });
    let key = ui.input("API key: ").await.map_err(|e| err(e.to_string()))?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(err("no API key was entered; nothing was written"));
    }
    let inputs = ctx.inputs(&[], false)?;
    let key_name = format!("provider.{id}.key");
    let proposal = caps::propose_in(&ctx.comp, &inputs, inputs.home_config(), vec![(key_name.clone(), Value::String(key))])?;
    let written = proposal
        .commit()
        .map_err(|e| err(format!("key accepted, but configuration could not be written: {e}; run `aho login {id}` again")))?;
    println!("Saved {key_name} to {}", inputs.home_config().display());
    for line in written {
        println!("{line}");
    }
    crate::record::record_change(ctx, &proposal.after, None, "login", vec![id.to_string()]).await?;
    println!("Select this route with `aho set model.provider {id}` and a supported model with `aho set model.name <model-id>`.");
    Ok(())
}

/// Logout clears the pasted key from home config; there is no auth.json entry and no revocation call.
async fn api_key_logout(ctx: &Ctx, id: &str) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let home = inputs.home_config();
    let home_text = inputs.files.iter().find(|f| f.path == home.display().to_string()).map(|f| f.text.as_str()).unwrap_or("");
    let home_value: toml::Value = toml::from_str(home_text).map_err(|e| err(format!("invalid home configuration: {e}")))?;
    let key_name = format!("provider.{id}.key");
    let stored = home_value.get("provider").and_then(|p| p.get(id)).and_then(|p| p.get("key")).and_then(toml::Value::as_str);
    if stored.is_none_or(str::is_empty) {
        println!("No stored API key for {id} in {}: `aho login {id}` writes {key_name}.", home.display());
        return Ok(());
    }
    let proposal = caps::propose_in(&ctx.comp, &inputs, home, vec![(key_name, Value::String(String::new()))])?;
    for line in proposal.commit()? {
        println!("{line}");
    }
    crate::record::record_change(ctx, &proposal.after, None, "logout", vec![id.to_string()]).await?;
    println!("Removed the stored {id} API key locally. Provider keys are not revoked; OAuth account logins are unchanged.");
    Ok(())
}

async fn choose(requested: Option<&str>, ids: Vec<&str>, ui: &Terminal) -> Result<String, Fail> {
    if let Some(id) = requested {
        return Ok(id.to_ascii_lowercase());
    }
    if ids.is_empty() {
        return Err(err("no stored account logins; use `aho login`"));
    }
    for (index, id) in ids.iter().enumerate() {
        let name = provider_auth::provider(id).map_or(*id, |p| p.name);
        println!("  {}. {} ({id})", index + 1, name);
    }
    let answer = ui.input("Provider (number or id): ").await.map_err(|e| err(e.to_string()))?;
    let selected = answer.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| ids.get(n));
    match selected.copied().or_else(|| ids.iter().copied().find(|id| id.eq_ignore_ascii_case(answer.trim()))) {
        Some(id) => Ok(id.to_string()),
        None => Err(err("invalid provider selection; use `aho login --list`")),
    }
}
