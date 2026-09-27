//! Reload recovery: before exec, each affected session gets a
//! `SessionRecoveryIntent{role, directive}`; the new process closes the turn that was cut short
//! and appends one continuation prompt per intent. The entry that references an intent
//! *consumes* it (a `UserMessage` whose `client_key` is `recovery:<intent entry id>`), so there
//! are no side files, and `Session::prompt`'s `client_key` dedup keeps it exactly-once even when
//! the process dies between the two.

use std::collections::BTreeSet;
use std::sync::Arc;

use aho_log::{
    ApprovalOutcome, CancelReason, CustomFolds, Entry, EntryId, EntryKind, GrantScope, LogStore, OpStatus, RecoveryRole, ToolOutcome,
    Transition, fold,
};
use aho_session::Session;
use aho_types::*;

/// The model-facing core of every continuation prompt.
pub const CONTINUATION: &str = "The harness reloaded while this session was working. The turn that was running was closed and the tool calls it had in flight were cancelled; their results say so. Pick the task up where it stopped: do not ask what to do next and do not summarize the reload.";

const BY: &str = "daemon";

/// The continuation prompt for `role`, listing still-running jobs.
pub fn continuation(role: RecoveryRole, running_jobs: &[String]) -> String {
    let lead = match role {
        RecoveryRole::Initiator => "You asked for this reload and it has finished.",
        RecoveryRole::InterruptedPeer => "Another session asked for this reload; yours was interrupted in the middle of a turn.",
        RecoveryRole::Headless => "This session was working with no client attached when the harness reloaded.",
    };
    let jobs = if running_jobs.is_empty() {
        "No background jobs are still running.".to_string()
    } else {
        format!("Still running: {}. Their results arrive as they finish.", running_jobs.join("; "))
    };
    format!("{lead} {CONTINUATION} {jobs}")
}

/// Key of the prompt that consumes intent `id`.
pub fn consume_key(id: EntryId) -> String {
    format!("recovery:{}", id.0)
}

pub fn persist_intent(log: &dyn LogStore, role: RecoveryRole, directive: &str) -> Result<EntryId, String> {
    log.append(None, Principal::system(BY), EntryKind::SessionRecoveryIntent { role, directive: directive.to_string() })
        .map(|e| e.id)
        .map_err(|e| {
            format!(
                "could not record the reload intent in the session log ({e}); this session will not continue by itself after the reload"
            )
        })
}

fn branch(log: &dyn LogStore) -> Result<Vec<Entry>, String> {
    match log.head() {
        Some(h) => log.read_branch(h).map_err(|e| format!("the session log could not be read ({e}); we changed nothing")),
        None => Ok(Vec::new()),
    }
}

/// Intents on the current branch that no entry in the log references yet, oldest first.
pub fn unconsumed(log: &dyn LogStore) -> Vec<(EntryId, RecoveryRole, String)> {
    let Ok(entries) = branch(log) else {
        return Vec::new();
    };
    // Same scope as `Session::prompt`'s dedup: a consuming prompt anywhere in the file counts.
    let keys: BTreeSet<String> = log
        .read_from(None)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::UserMessage { client_key: Some(k), .. } => Some(k),
            _ => None,
        })
        .collect();
    entries
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::SessionRecoveryIntent { role, directive } if !keys.contains(&consume_key(e.id)) => Some((e.id, role, directive)),
            _ => None,
        })
        .collect()
}

/// Closes a turn left open by the previous process: every open approval is cancelled, every open
/// op gets `OpDone{Cancelled{reason}}`, every open call an error `ToolResult{Cancelled{reason}}`,
/// then `StepEnd` and `TurnEnd`. Returns whether a turn was open. Run it before `Session::boot` on
/// that log: a second `TurnStart` inside an open turn breaks the fold.
pub fn close_open_turn(log: &dyn LogStore, reason: CancelReason) -> Result<bool, String> {
    let state = fold(&branch(log)?, &CustomFolds::new())
        .map_err(|e| format!("the session log does not fold ({e}); we did not touch it. Inspect it before resuming the session"))?;
    let Some(turn) = state.turn else {
        return Ok(false);
    };
    let by = || Principal::system(BY);
    let write = |kind: EntryKind| {
        log.append(None, by(), kind).map(|_| ()).map_err(|e| {
            format!("could not close the interrupted turn ({e}); the log is left partly closed. Fix the log directory and restart")
        })
    };
    for req in state.pending_approvals.keys() {
        write(EntryKind::ApprovalDecided { req: *req, outcome: ApprovalOutcome::Cancelled, scope: GrantScope::Once })?;
    }
    let mut external_calls = BTreeSet::new();
    for (op, (call, _, external)) in &state.open_ops {
        if *external {
            external_calls.insert(call.clone());
        }
        write(EntryKind::OpDone { op: *op, status: OpStatus::Cancelled { reason } })?;
    }
    for call in &state.open_calls {
        let text = cancelled_text(reason, external_calls.contains(call));
        write(EntryKind::ToolResult {
            call: call.clone(),
            outcome: ToolOutcome::Cancelled { reason },
            value: Value::Null,
            content: vec![ContentBlock::text(text)],
            spill_ref: None,
            meta: None,
            late: false,
        })?;
    }
    let transition = match reason {
        CancelReason::Reload => Transition::Reload,
        other => Transition::Aborted(format!("turn closed by the daemon ({other:?})")),
    };
    write(EntryKind::StepEnd { step: StepId(0), transition })?;
    write(EntryKind::TurnEnd { turn })?;
    Ok(true)
}

fn cancelled_text(reason: CancelReason, external: bool) -> String {
    let why = match reason {
        CancelReason::Reload => "The harness reloaded while this call was running",
        _ => "The harness stopped this session while this call was running",
    };
    if external {
        format!(
            "{why}, so it was cancelled mid-flight. It has an external effect and may or may not have happened: check its effect before running it again."
        )
    } else {
        format!("{why}, so it was cancelled and did not finish. Run it again if you still need its result.")
    }
}

/// Runs one continuation turn per unconsumed intent, keyed so the prompt consumes the intent.
/// Returns how many ran.
pub async fn consume(session: &Arc<Session>, running_jobs: &[String]) -> Result<usize, String> {
    let pending = unconsumed(&**session.log());
    for (id, role, _) in &pending {
        session
            .prompt(continuation(*role, running_jobs), Some(consume_key(*id)))
            .await
            .map_err(|e| format!("the continuation after the reload could not run ({e}); prompt the session to carry on"))?;
    }
    Ok(pending.len())
}
