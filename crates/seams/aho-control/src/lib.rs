//! What drivers and session modes may ask of a live session (`01` "an engineer with a different
//! runtime writes a driver, not a fork", `04` authority, `10` goals / plan mode).
//!
//! * [`PromptSink`] — run one turn with a prompt, exactly once per key. Schedules and ralph loops
//!   (`00` agent management) fire through it; [`ChannelSink`] routes the same requests over a real
//!   channel to whoever runs the turns (an embedder's loop, a test).
//! * [`Clock`] — wall time for drivers: [`SystemClock`] in production, [`SteppedClock`] when the
//!   embedder drives time itself (simulations, game loops, deterministic tests).
//! * [`SessionCeiling`] — the live policy a mode (plan mode, `04`) may narrow by a meet, and may
//!   ask the operator to widen. The implementation raises the approval card itself, so a caller
//!   cannot answer for the operator: tools never mint authority.
//! * [`turn_report`] / [`record`] — read a keyed turn's outcome off the log; append a driver's
//!   own `Custom` records as `System(<driver>)`.

#![forbid(unsafe_code)]

mod ceiling;
mod clock;
mod sink;

pub use ceiling::{CEILING_SERVICE, CeilingHandle, SessionCeiling, Widen, Widened};
pub use clock::{Clock, SteppedClock, SystemClock};
pub use sink::{ChannelSink, DriveError, PromptSink, SinkRequest, TurnEnd};

use aho_log::{Entry, EntryKind, LogStore, Transition};
use aho_types::*;

/// The outcome of the turn whose prompt carried inbox key `key`, read off `entries` (one branch,
/// oldest first): the last non-empty assistant text before that turn's `TurnEnd`, and the
/// failure if the turn ended `Aborted`. `None` when no prompt with that key was admitted.
pub fn turn_report(entries: &[Entry], key: &str) -> Option<TurnEnd> {
    let start = entries.iter().position(|e| matches!(&e.kind, EntryKind::UserMessage { client_key: Some(k), .. } if k == key))?;
    let mut text = String::new();
    let mut failed = None;
    for e in &entries[start + 1..] {
        match &e.kind {
            EntryKind::AssistantMessage { content, .. } => {
                let t: Vec<&str> = content.iter().filter_map(|b| b.as_text()).filter(|t| !t.trim().is_empty()).collect();
                if !t.is_empty() {
                    text = t.join("\n");
                }
            }
            EntryKind::StepEnd { step: StepId(0), transition } => {
                failed = match transition {
                    Transition::Aborted(why) => Some(why.clone()),
                    _ => None,
                };
            }
            EntryKind::TurnEnd { .. } => break,
            _ => {}
        }
    }
    Some(TurnEnd { final_text: text, failed, ran: true })
}

/// True when a prompt keyed `key` is already in `entries` (the session would dedupe it).
pub fn admitted(entries: &[Entry], key: &str) -> bool {
    entries.iter().any(|e| matches!(&e.kind, EntryKind::UserMessage { client_key: Some(k), .. } if k == key))
}

/// The current branch of `log`, oldest first (empty for a new log).
pub fn branch(log: &dyn LogStore) -> Result<Vec<Entry>, String> {
    match log.head() {
        Some(h) => log.read_branch(h).map_err(|e| format!("the session log could not be read ({e}); nothing was changed")),
        None => Ok(Vec::new()),
    }
}

/// Appends a driver's own record `Custom{ns, name, body}` as `System(by)`. Drivers are hosted code
/// (daemon, CLI), not tools; they append between turns, like the daemon's recovery intents.
pub fn record(log: &dyn LogStore, by: &str, ns: &str, name: &str, body: Value) -> Result<EntryId, String> {
    log.append(None, Principal::system(by), EntryKind::Custom { ns: ns.to_string(), name: name.to_string(), body }).map(|e| e.id).map_err(
        |e| format!("could not write the {ns}/{name} record to the session log ({e}); the driver stopped so it does not act twice"),
    )
}

#[cfg(test)]
mod tests;
