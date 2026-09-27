//! Folds over a session's `State`: `aho status`, the resume catch-up brief, `aho gc --dry-run`
//! orphans, and `aho invariants`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use aho_cap::Plan;
use aho_log::{Item, OpStatus, ProcessIdentity, State, Transition, Verdict};
use aho_types::*;
use serde::Serialize;

use crate::{blast, clock, render_classes};

// ───────────────────────────── status ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpenCall {
    pub call: CallId,
    pub tool: String,
    pub class: Consequence,
    pub op: Option<OpId>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LeaseRow {
    pub lease: LeaseId,
    pub subject: String,
    pub expires: Timestamp,
    pub expired: bool,
    pub remaining_s: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub plan_hash: Hash,
    /// The plan the log was sealed with; differs from `plan_hash` when config moved since.
    pub sealed_plan: Option<Hash>,
    pub plan_matches_log: bool,
    pub preset: String,
    pub epoch: Epoch,
    pub allowed: ConsequenceSet,
    pub world: String,
    pub enforced_by: String,
    /// Active capabilities, loudest (largest blast radius) first.
    pub active: Vec<(String, ConsequenceSet)>,
    pub open_calls: Vec<OpenCall>,
    pub uncertain_ops: Vec<OpId>,
    pub leases: Vec<LeaseRow>,
    pub pending_approvals: usize,
    pub turn_open: bool,
    pub turns: u64,
    pub usage: Usage,
    pub uptime_s: Option<u64>,
}

fn loudness(c: ConsequenceSet) -> (u8, usize) {
    (c.iter().map(blast).max().unwrap_or(0), c.iter().count())
}

pub fn status(plan: &Plan, state: &State, now: Timestamp) -> Status {
    let declared = |id: &str| plan.row(id).map(|r| r.declared).unwrap_or_default();
    let mut active: Vec<(String, ConsequenceSet)> = if state.caps.is_empty() {
        plan.active().map(|r| (r.id.name.to_string(), r.declared)).collect()
    } else {
        state.active_caps().map(|id| (id.clone(), declared(id))).collect()
    };
    active.sort_by(|a, b| loudness(b.1).cmp(&loudness(a.1)).then_with(|| a.0.cmp(&b.0)));

    let op_of = |call: &CallId| state.open_ops.iter().find(|(_, (c, _, _))| c == call).map(|(op, _)| *op);
    let open_calls = state
        .open_calls
        .iter()
        .map(|call| {
            let (tool, class) = state.calls.get(call).map(|(t, c, _)| (t.clone(), *c)).unwrap_or((String::new(), Consequence::ReadOnly));
            OpenCall { call: call.clone(), tool, class, op: op_of(call) }
        })
        .collect();
    let leases = state
        .leases
        .iter()
        .map(|(lease, (subject, expires))| LeaseRow {
            lease: *lease,
            subject: subject.clone(),
            expires: *expires,
            expired: expires.0 <= now.0,
            remaining_s: expires.0.saturating_sub(now.0) / 1000,
        })
        .collect();
    let live_policy = state.policy.epoch.0 > 0;
    Status {
        plan_hash: plan.hash,
        sealed_plan: state.plan_hash,
        plan_matches_log: state.plan_hash == Some(plan.hash),
        preset: if live_policy { state.policy.preset.clone() } else { plan.preset.clone() },
        epoch: state.policy.epoch,
        allowed: if live_policy { state.policy.allowed } else { plan.allowed },
        world: plan.world.name.clone(),
        enforced_by: plan.world.enforced_by.clone(),
        active,
        open_calls,
        uncertain_ops: state.op_status.iter().filter(|(_, s)| matches!(s, OpStatus::Uncertain { .. })).map(|(op, _)| *op).collect(),
        leases,
        pending_approvals: state.pending_approvals.len(),
        turn_open: state.turn.is_some(),
        turns: state.turns,
        usage: state.usage,
        uptime_s: state.meta.as_ref().and_then(|(_, _, p)| *p).map(|p| now.0.saturating_sub(p.start_time.0) / 1000),
    }
}

pub fn render_status(s: &Status) -> String {
    let sealed = match s.sealed_plan {
        Some(h) if s.plan_matches_log => format!(" (sealed {})", h.short()),
        Some(h) => format!(" (log sealed {}; config changed since)", h.short()),
        None => " (no session sealed)".into(),
    };
    let mut out = format!(
        "plan {}{sealed}   preset: {} (epoch {})   world: {} (enforced by: {})\n",
        s.plan_hash.short(),
        s.preset,
        s.epoch.0,
        s.world,
        s.enforced_by
    );
    out.push_str(&format!("allowed: {}\n", render_classes(s.allowed)));
    if !s.active.is_empty() {
        out.push_str("active (loudest first):\n");
        for (id, c) in &s.active {
            out.push_str(format!("  {id:<24}{}\n", render_classes(*c)).trim_end());
            out.push('\n');
        }
    }
    if !s.open_calls.is_empty() {
        out.push_str("open calls:\n");
        for c in &s.open_calls {
            let op = c.op.map(|o| format!("  op {}", o.0)).unwrap_or_default();
            out.push_str(&format!("  {}  {}  {}{op}\n", c.call.0, c.tool, c.class.name()));
        }
    }
    for op in &s.uncertain_ops {
        out.push_str(&format!("  ⚠ op {} may or may not have happened (the process died mid-dispatch)\n", op.0));
    }
    if !s.leases.is_empty() {
        out.push_str("leases:\n");
        for l in &s.leases {
            let when = if l.expired { "expired".to_string() } else { format!("expires in {}s", l.remaining_s) };
            out.push_str(&format!("  lease {}  {}  {when}\n", l.lease.0, l.subject));
        }
    }
    if s.pending_approvals > 0 {
        out.push_str(&format!("{} approval(s) waiting for you\n", s.pending_approvals));
    }
    let turn = if s.turn_open { "   (a turn is open)" } else { "" };
    out.push_str(&format!("turns: {}   tokens: in {} / out {}{turn}\n", s.turns, s.usage.input_tokens, s.usage.output_tokens));
    out
}

// ───────────────────────────── catch-up ─────────────────────────────

/// Closed: derived from `StepEnd{transition}`, the open turn, and whether the owner lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SessionStatus {
    Active,
    Closed,
    Crashed,
    Reloaded,
    Compacted,
    RateLimited,
    Error,
}

/// `owner_alive`: whether the process recorded in `SessionMeta` still runs (the caller probes it).
pub fn session_status(state: &State, owner_alive: bool) -> (SessionStatus, String) {
    if matches!(state.last_transition, Some(Transition::Reload)) {
        return (SessionStatus::Reloaded, "the harness reloaded in place; the session continues on the new binary".into());
    }
    if let Some(turn) = state.turn {
        return if owner_alive {
            (SessionStatus::Active, format!("turn {} is running", turn.0))
        } else {
            (SessionStatus::Crashed, format!("the process stopped during turn {} before the turn ended", turn.0))
        };
    }
    match &state.last_transition {
        None if state.turns == 0 => (SessionStatus::Closed, "no turn has run yet".into()),
        None | Some(Transition::Completed) => (SessionStatus::Closed, "the last turn completed".into()),
        Some(Transition::Aborted(why)) if why.to_lowercase().contains("rate") => {
            (SessionStatus::RateLimited, format!("the provider rate-limited the last turn: {why}"))
        }
        Some(Transition::Aborted(why)) => (SessionStatus::Error, format!("the last turn stopped: {why}")),
        Some(Transition::ReactiveCompactRetry) => (SessionStatus::Compacted, "the context was compacted to continue".into()),
        Some(Transition::PromptTooLong) => {
            (SessionStatus::Error, "the conversation no longer fits the model's context; compact or fork it".into())
        }
        Some(Transition::MaxTurns) => (SessionStatus::Error, "the turn stopped at `session.max_turns`".into()),
        Some(Transition::WaitingOnOps) if owner_alive && !state.open_ops.is_empty() => {
            (SessionStatus::Active, format!("waiting on {} background operation(s)", state.open_ops.len()))
        }
        Some(_) => (SessionStatus::Closed, "the last turn ended".into()),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CatchUp {
    pub status: SessionStatus,
    pub reason: String,
    /// Attention status and something happened after the operator's last `SessionSeen`.
    pub needs_catch_up: bool,
    pub last_prompt: Option<String>,
    pub last_response: Option<String>,
    pub files_touched: Vec<String>,
    pub tool_counts: Vec<(String, u32)>,
    pub needs_from_user: Vec<String>,
}

const PATH_ARGS: &[&str] = &["path", "file_path", "file"];

/// The brief rendered on resume. Resuming means the previous owner is gone, so an open turn
/// reads as `Crashed`.
pub fn catch_up(state: &State) -> CatchUp {
    let (status, reason) = session_status(state, false);
    let last_prompt = state.items.iter().rev().find_map(|(_, it)| match it {
        Item::User { form: Form::Prompt, text } => Some(text.clone()),
        _ => None,
    });
    let last_response = state.items.iter().rev().find_map(|(_, it)| match it {
        Item::Assistant { content } => {
            // Thinking is not something the operator was told.
            let text: String =
                content.iter().filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }).collect();
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    });

    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for (tool, class, args) in state.calls.values() {
        *counts.entry(tool.clone()).or_default() += 1;
        if matches!(class, Consequence::WorkspaceWrite | Consequence::HostWrite) {
            for k in PATH_ARGS {
                if let Some(p) = args.get(*k).and_then(Value::as_str) {
                    files.insert(p.to_string());
                }
            }
            if let Some(ps) = args.get("paths").and_then(Value::as_array) {
                files.extend(ps.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
    }

    let mut needs = Vec::new();
    for call in state.pending_approvals.values() {
        let tool = call.as_ref().and_then(|c| state.calls.get(c)).map(|(t, _, _)| t.as_str()).unwrap_or("a capability");
        needs.push(format!("decide the pending approval for `{tool}`"));
    }
    for (op, s) in &state.op_status {
        if matches!(s, OpStatus::Uncertain { .. }) {
            needs.push(format!("check whether operation {} happened: the process died while it was running", op.0));
        }
    }
    match status {
        SessionStatus::Crashed => {
            if !state.open_calls.is_empty() {
                needs.push(format!("{} tool call(s) had no result when the session stopped", state.open_calls.len()));
            }
            needs.push("resume to continue from the last entry, or fork to start over from an earlier one".into());
        }
        SessionStatus::RateLimited => needs.push("wait for the rate limit to clear, or switch `model.provider`".into()),
        SessionStatus::Error => needs.push(format!("decide how to continue: {reason}")),
        _ => {}
    }
    if let Some(q) = last_response.as_deref().and_then(|t| t.trim_end().lines().last()).filter(|l| l.trim_end().ends_with('?')) {
        needs.push(format!("answer the agent's question: {}", q.trim()));
    }

    let updated_after_seen = match (state.head, state.seen) {
        (Some(h), Some(s)) => h > s,
        (Some(_), None) => true,
        (None, _) => false,
    };
    CatchUp {
        status,
        reason,
        needs_catch_up: status != SessionStatus::Active && updated_after_seen,
        last_prompt,
        last_response,
        files_touched: files.into_iter().collect(),
        tool_counts: counts.into_iter().collect(),
        needs_from_user: needs,
    }
}

/// Markdown, identical in every surface.
pub fn render_catch_up(c: &CatchUp) -> String {
    let mut out = format!("## Catch-up: {:?}\n\n_{}_\n\n", c.status, c.reason);
    if let Some(p) = &c.last_prompt {
        out.push_str(&format!("**Last prompt:** {p}\n\n"));
    }
    if let Some(r) = &c.last_response {
        out.push_str(&format!("**Last response:** {r}\n\n"));
    }
    if !c.files_touched.is_empty() {
        let files: Vec<String> = c.files_touched.iter().map(|f| format!("`{f}`")).collect();
        out.push_str(&format!("**Files touched:** {}\n\n", files.join(", ")));
    }
    if !c.tool_counts.is_empty() {
        let tools: Vec<String> = c.tool_counts.iter().map(|(t, n)| format!("{t} ×{n}")).collect();
        out.push_str(&format!("**Tools:** {}\n\n", tools.join(", ")));
    }
    if !c.needs_from_user.is_empty() {
        out.push_str("**Needs from you:**\n");
        for n in &c.needs_from_user {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out
}

// ───────────────────────────── orphans (aho gc) ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrphanKind {
    OpenTurn { turn: TurnId },
    Operation { op: OpId, call: CallId, external_effect: bool },
    ToolCall { call: CallId, tool: String },
    Lease { lease: LeaseId, expires: Timestamp },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Orphan {
    pub owner: SessionId,
    pub identity: ProcessIdentity,
    pub subject: String,
    pub kind: OrphanKind,
    pub age_s: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OrphanScan {
    /// Leftovers of owners missing on this scan and the previous one.
    pub orphans: Vec<Orphan>,
    /// Every owner missing on this scan: pass it back as `previous_missing` next time.
    pub missing: HashSet<ProcessIdentity>,
    /// Owners missing for the first time: not judged until the next scan (liveness flaps).
    pub unconfirmed: Vec<(SessionId, ProcessIdentity)>,
}

fn leftovers(state: &State) -> Vec<(String, OrphanKind)> {
    let mut out = Vec::new();
    if let Some(turn) = state.turn {
        out.push((format!("turn {} never ended", turn.0), OrphanKind::OpenTurn { turn }));
    }
    for (op, (call, _, external)) in &state.open_ops {
        let effect = if *external { ", has an external effect" } else { "" };
        out.push((
            format!("operation {} (call {}) still open{effect}", op.0, call.0),
            OrphanKind::Operation { op: *op, call: call.clone(), external_effect: *external },
        ));
    }
    for call in &state.open_calls {
        if state.open_ops.values().any(|(c, _, _)| c == call) {
            continue;
        }
        let tool = state.calls.get(call).map(|(t, _, _)| t.clone()).unwrap_or_default();
        out.push((format!("tool call {} ({tool}) has no result", call.0), OrphanKind::ToolCall { call: call.clone(), tool }));
    }
    for (lease, (subject, expires)) in &state.leases {
        out.push((format!("lease {} on {subject}", lease.0), OrphanKind::Lease { lease: *lease, expires: *expires }));
    }
    out
}

/// `aho gc --dry-run`. `alive` holds the owner identities that answered this scan's liveness
/// probe; an owner counts as gone only when it is missing now *and* was in `previous_missing`.
pub fn orphans(
    sessions: &[(SessionId, State)],
    alive: &HashSet<ProcessIdentity>,
    previous_missing: &HashSet<ProcessIdentity>,
    now: Timestamp,
) -> OrphanScan {
    let mut scan = OrphanScan::default();
    for (sid, state) in sessions {
        let Some(identity) = state.meta.as_ref().and_then(|(_, _, p)| *p) else { continue };
        if alive.contains(&identity) {
            continue;
        }
        scan.missing.insert(identity);
        if !previous_missing.contains(&identity) {
            scan.unconfirmed.push((*sid, identity));
            continue;
        }
        let age_s = now.0.saturating_sub(identity.start_time.0) / 1000;
        for (subject, kind) in leftovers(state) {
            scan.orphans.push(Orphan { owner: *sid, identity, subject, kind, age_s });
        }
    }
    scan
}

pub fn render_orphans(scan: &OrphanScan) -> String {
    let mut out = String::new();
    let mut last: Option<SessionId> = None;
    for o in &scan.orphans {
        if last != Some(o.owner) {
            out.push_str(&format!(
                "session {:032x}  pid {} (started {})  gone on two scans\n",
                o.owner.0,
                o.identity.pid,
                clock(o.identity.start_time)
            ));
            last = Some(o.owner);
        }
        out.push_str(&format!("  {}   age {}s\n", o.subject, o.age_s));
    }
    if !scan.unconfirmed.is_empty() {
        out.push_str(&format!("{} owner(s) missing for the first time; re-run to confirm before reclaiming\n", scan.unconfirmed.len()));
    }
    if out.is_empty() {
        out.push_str("nothing to reclaim\n");
    }
    out
}

// ───────────────────────────── invariants ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InvariantRow {
    pub id: String,
    /// The verdict the operator should believe now: `Stale` whenever the check predates the code.
    pub verdict: Verdict,
    /// The checker's last verdict.
    pub recorded: Verdict,
    pub code_hash: Hash,
    pub stale: bool,
}

/// Latest `InvariantChecked` per id against the current code; older than the code is `Stale`, never `Pass`.
pub fn invariants(state: &State, current_code_hash: Hash) -> Vec<InvariantRow> {
    state
        .invariants
        .iter()
        .map(|(id, (verdict, code_hash))| {
            let stale = *code_hash != current_code_hash;
            InvariantRow {
                id: id.clone(),
                verdict: if stale { Verdict::Stale } else { verdict.clone() },
                recorded: verdict.clone(),
                code_hash: *code_hash,
                stale,
            }
        })
        .collect()
}

/// `aho invariants` folds the latest `Checked` per id against the current checkpoint's
/// `code_hash`. The current code is the tree of the latest `Workspace/Checkpoint` (tool-formal
/// binds every verdict to the workspace tree it checked); before the first checkpoint there is
/// nothing to be older than, so the recorded verdicts stand. `aho invariants`, the TUI, the
/// web view and a swarm's verify gate all show this.
pub fn current_invariants(state: &State) -> Vec<InvariantRow> {
    match state.checkpoints.last() {
        Some((_, tree, _)) => invariants(state, *tree),
        None => state
            .invariants
            .iter()
            .map(|(id, (v, h))| InvariantRow { id: id.clone(), verdict: v.clone(), recorded: v.clone(), code_hash: *h, stale: false })
            .collect(),
    }
}

fn verdict_text(v: &Verdict) -> String {
    match v {
        Verdict::Pass => "Pass".into(),
        Verdict::Fail { counterexample } => format!("Fail ✕ (counterexample {})", counterexample.short()),
        Verdict::Unknown { reason } => format!("Unknown ({reason})"),
        Verdict::Stale => "Stale".into(),
        Verdict::Vacuous => "Vacuous ⚠".into(),
    }
}

pub fn render_invariants(rows: &[InvariantRow]) -> String {
    let mut out = format!("{:<28}{:<34}{:<10}note\n", "id", "verdict", "code");
    for r in rows {
        let note = if r.stale { format!("code changed since the check (was {})", verdict_text(&r.recorded)) } else { String::new() };
        let line = format!("{:<28}{:<34}{:<10}{note}", r.id, verdict_text(&r.verdict), r.code_hash.short());
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
