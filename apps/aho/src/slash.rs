//! `05`: the slash commands `aho tui` and `aho web` answer in their prompt box (`/help` is built
//! into the registry). Each is the in-session twin of a CLI verb.

use std::sync::Arc;

use aho_session::Session;
use surface_tui::Commands;

/// Narrows the session to plan mode as the operator (`/plan [reason]`, the twin of
/// `aho plan-mode on`): `PolicyEpoch` by `System(plan-mode)` plus `plan/Entered` by `Operator`.
pub fn plan_mode(session: &Session, reason: Option<String>) -> Result<String, String> {
    let ceiling = session.registry().service::<aho_control::CeilingHandle>(aho_control::CEILING_SERVICE).ok_or_else(|| {
        "this session exposes no policy ceiling, so plan mode cannot narrow it and nothing was changed. Narrow it with `aho preset read-only` instead"
            .to_string()
    })?;
    let announce = |kind| session.append_entry(aho_types::Principal::Operator, kind).map(|_| ()).map_err(|e| e.to_string());
    tool_plan_mode::enter_by_operator(&*ceiling.0, &**session.log(), &announce, reason)
}

/// The registry both interactive surfaces serve.
pub fn commands() -> Result<Commands, String> {
    let mut c = Commands::new();
    c.register(
        "preset",
        "<name>",
        "switch the session policy: read-only, workspace, dev, lab, full, or spec",
        Arc::new(|s: &Arc<Session>, args: &str, run| {
            let view = s.set_preset(&crate::boot::operator(), args, run).map_err(|e| e.to_string())?;
            Ok(format!("preset {args} · epoch {} · allowed {}", view.epoch.0, view.allowed))
        }),
    )?;
    c.register(
        "plan",
        "[reason]",
        "enter plan mode: only read-only tools and questions until you approve the agent's plan",
        Arc::new(|s: &Arc<Session>, args: &str, _| plan_mode(s, (!args.is_empty()).then(|| args.to_string()))),
    )?;
    Ok(c)
}
