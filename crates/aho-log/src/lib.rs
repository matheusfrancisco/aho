//! `06` — the session log. Config, activation, transcript, tool I/O, approvals, compaction,
//! checkpoints, operations all live here. Everything else is a fold over a branch.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aho_config::Sourced;
use aho_types::*;
use serde::{Deserialize, Serialize};

pub use aho_types::EntryId;

mod index;
mod store;
#[cfg(test)]
mod tests;

pub use index::{IndexError, IndexHit, IndexIssue, IndexPage, IndexQuery, IndexReport, IndexStatus, IndexedLog, SessionIndex};
pub use store::{JsonlLog, LogError, LogStore, MemoryLog};

// ───────────────────────────────────── entries ─────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    /// Tree: a branch is a leaf pointing at an older parent. `None` only for the first entry.
    pub parent: Option<EntryId>,
    /// Chain over the branch, not the file: `hash = chain(prev_hash, body)`.
    pub prev_hash: Hash,
    pub hash: Hash,
    pub at: Timestamp,
    pub by: Principal,
    pub kind: EntryKind,
}

impl Entry {
    fn body_hash(prev: &Hash, id: EntryId, parent: Option<EntryId>, at: Timestamp, by: &Principal, kind: &EntryKind) -> Hash {
        let body = serde_json::to_vec(&(id, parent, at, by, kind)).unwrap_or_default();
        Hash::chain(prev, &body)
    }
    pub fn verify(&self) -> bool {
        self.hash == Entry::body_hash(&self.prev_hash, self.id, self.parent, self.at, &self.by, &self.kind)
    }
}

/// Closed core kinds plus a namespaced escape hatch. Capabilities add kinds under their own
/// namespace via `Custom`; they never add fields to these.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    // ── bootstrap (03) ──
    ConfigResolved {
        key: String,
        sourced: Sourced<Value>,
    },
    CapActivated {
        id: String,
        manifest_hash: Hash,
        deps: Vec<String>,
        config_hash: Hash,
    },
    PlanSealed {
        plan_hash: Hash,
        fingerprint: Hash,
        schema_hash: Hash,
    },
    CapDelta {
        id: String,
        op: CapDeltaOp,
        reason: String,
    },
    LoadoutDiagnostic {
        lint: String,
        severity: String,
        key: String,
        detail: String,
    },

    // ── authority (04) ──
    PolicyEpoch {
        epoch: Epoch,
        allowed: ConsequenceSet,
        preset: String,
        approval_ref: Option<EntryId>,
    },
    ApprovalRequested {
        wants: ConsequenceSet,
        for_cap: String,
        call: Option<CallId>,
        reason: String,
    },
    ApprovalDecided {
        req: EntryId,
        outcome: ApprovalOutcome,
        scope: GrantScope,
    },
    LeaseGranted {
        lease: LeaseId,
        subject: String,
        expires: Timestamp,
    },
    LeaseRevoked {
        lease: LeaseId,
        proof_next_attempt: Option<EntryId>,
    },

    // ── turn (01, 16) ──
    TurnStart {
        turn: TurnId,
    },
    TurnEnd {
        turn: TurnId,
    },
    StepStart {
        step: StepId,
    },
    StepEnd {
        step: StepId,
        transition: Transition,
    },
    SystemMessage {
        sections: Vec<(String, Hash)>,
        cache_break_at: usize,
    },
    UserMessage {
        form: Form,
        text: String,
        client_key: Option<String>,
    },
    AssistantMessage {
        content: Vec<ContentBlock>,
        usage: Usage,
        stop: StopReason,
    },
    AssistantAttempt {
        error: String,
    },
    RequestHeader {
        model: String,
        tools_visible: Vec<String>,
        policy_epoch: Epoch,
        plan_hash: Hash,
        section_hashes: Vec<Hash>,
        open_calls: Vec<CallId>,
        request_hash: Hash,
    },
    ToolCall {
        call: CallId,
        tool: String,
        args: Value,
        class: Consequence,
    },
    ToolResult {
        call: CallId,
        outcome: ToolOutcome,
        value: Value,
        content: Vec<ContentBlock>,
        spill_ref: Option<Hash>,
        meta: Option<Card>,
        late: bool,
    },
    OpStarted {
        op: OpId,
        call: CallId,
        kind: OpKind,
        rerunnable: bool,
        external_effect: bool,
    },
    OpDone {
        op: OpId,
        status: OpStatus,
    },

    // ── compaction (10) ──
    SurfaceReplace {
        start: EntryId,
        end: EntryId,
        shadow_price: u64,
        summary: Option<String>,
        by_cap: String,
    },
    CompactionStart {
        id: u64,
    },
    CompactionEnd {
        id: u64,
        error: Option<String>,
    },
    CompactionFailed {
        consecutive: u32,
    },

    // ── hooks, commands, surfaces (10, 12) ──
    HookInvoked {
        point: String,
        cap: String,
        matcher: String,
    },
    HookResult {
        decision: String,
        exit: Option<i32>,
        stderr_summary: String,
        ms: u64,
    },
    CommandRun {
        name: String,
        args: Vec<String>,
        key: Option<String>,
    },
    CommandDone {
        run: EntryId,
        kind: String,
    },
    SurfaceAttached {
        client_instance: String,
        has_local_history: bool,
        takeover: Takeover,
    },
    SessionRecoveryIntent {
        role: RecoveryRole,
        directive: String,
    },
    SessionSeen,
    /// `pid` alone is not identity — pids recycle. `(pid, start_time)` lets `aho gc` reap orphans across a crash.
    SessionMeta {
        title: Option<String>,
        tags: Vec<String>,
        last_prompt: Option<String>,
        process: Option<ProcessIdentity>,
    },

    // ── subagents (10, 12) ──
    SubagentStarted {
        child: SessionId,
        provider: String,
        depth: u32,
    },
    SubagentEnded {
        child: SessionId,
        stop: StopReason,
        last_message: Option<String>,
    },
    SubagentDescriptor {
        provider: String,
        persona: Option<String>,
        tool_filter: Vec<String>,
        parent: SessionId,
        depth: u32,
    },

    // ── time travel (11) ──
    WorkspaceCheckpoint {
        id: CheckpointId,
        tree: Hash,
        parent: Option<CheckpointId>,
        cause: CheckpointCause,
    },
    WorkspaceRestored {
        from: CheckpointId,
        to: CheckpointId,
        mode: RewindMode,
        changed: Vec<String>,
        deleted: Vec<String>,
    },

    // ── formal (13) ──
    InvariantRegistered {
        id: String,
        kind: FormalKind,
        statement: String,
        covers: Vec<String>,
    },
    InvariantChecked {
        id: String,
        mode: CheckMode,
        verdict: Verdict,
        code_hash: Hash,
    },
    InvariantMutated {
        id: String,
        mutant: String,
        caught: bool,
    },

    // ── capability-owned, namespaced (`Vm/ConsoleChunk`, `Task/Seeded`, `Memory/*`, `Verdict/*`) ──
    Custom {
        ns: String,
        name: String,
        body: Value,
    },

    Note {
        text: String,
    },
}

impl EntryKind {
    pub fn tag(&self) -> &'static str {
        macro_rules! t { ($($v:ident),*) => { match self { $(EntryKind::$v { .. } => stringify!($v),)* } } }
        t!(
            ConfigResolved,
            CapActivated,
            PlanSealed,
            CapDelta,
            LoadoutDiagnostic,
            PolicyEpoch,
            ApprovalRequested,
            ApprovalDecided,
            LeaseGranted,
            LeaseRevoked,
            TurnStart,
            TurnEnd,
            StepStart,
            StepEnd,
            SystemMessage,
            UserMessage,
            AssistantMessage,
            AssistantAttempt,
            RequestHeader,
            ToolCall,
            ToolResult,
            OpStarted,
            OpDone,
            SurfaceReplace,
            CompactionStart,
            CompactionEnd,
            CompactionFailed,
            HookInvoked,
            HookResult,
            CommandRun,
            CommandDone,
            SurfaceAttached,
            SessionRecoveryIntent,
            SessionSeen,
            SessionMeta,
            SubagentStarted,
            SubagentEnded,
            SubagentDescriptor,
            WorkspaceCheckpoint,
            WorkspaceRestored,
            InvariantRegistered,
            InvariantChecked,
            InvariantMutated,
            Custom,
            Note
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CapDeltaOp {
    Activated,
    Deactivated,
    Broken { err: String },
    Replaced { by: String },
    Rejected { reference: EntryId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    Once,
    Session,
    UntilRevoked,
}

/// `10`: closed step transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transition", content = "reason", rename_all = "snake_case")]
pub enum Transition {
    Completed,
    ToolUse,
    WaitingOnOps,
    ReactiveCompactRetry,
    MaxOutputRecovery,
    StopHookBlocking,
    QueuedCommand,
    BudgetContinue,
    PromptTooLong,
    MaxTurns,
    Reload,
    Aborted(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok,
    Err { code: String, reason: String },
    Cancelled { reason: CancelReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Aborted,
    BeforeDispatch,
    SiblingFailed,
    UserInterrupted,
    Rewind,
    Reload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Process,
    Io,
    Timer,
    Remote,
    Compute,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OpStatus {
    Completed {
        code: i32,
    },
    Failed {
        err: String,
    },
    Cancelled {
        reason: CancelReason,
    },
    /// The process died while this side-effecting op was mid-dispatch and it is not re-runnable.
    /// The model is told "may or may not have happened"; never silently `Failed`, never re-dispatched.
    Uncertain {
        last_seen: Timestamp,
    },
}

/// Crash-honest process identity: pids recycle, `(pid, start_time)` does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "takeover", content = "reason", rename_all = "snake_case")]
pub enum Takeover {
    Allowed,
    Refused(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryRole {
    Initiator,
    InterruptedPeer,
    Headless,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cause", content = "call", rename_all = "snake_case")]
pub enum CheckpointCause {
    ToolResult(CallId),
    TurnEnd,
    Manual,
    PreRewind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    Both,
    ChatOnly,
    CodeOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormalKind {
    Tla,
    Lean,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "n", rename_all = "snake_case")]
pub enum CheckMode {
    Bounded(u32),
    Inductive,
    Proof,
    Trace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail { counterexample: Hash },
    Unknown { reason: String },
    Stale,
    Vacuous,
}

// ───────────────────────────────────── fold ─────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapState {
    Active,
    Disabled { why: String },
    Locked { missing: Vec<String> },
    Broken { err: String },
    Unlocked,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyState {
    pub epoch: Epoch,
    pub allowed: ConsequenceSet,
    pub preset: String,
}

/// One model-visible item in log order; `derive_messages` turns these into messages.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    User {
        form: Form,
        text: String,
    },
    Assistant {
        content: Vec<ContentBlock>,
    },
    Result {
        call: CallId,
        tool: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    /// A request was made here (`RequestHeader`) or a turn began; results after it are late (16).
    Boundary,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub config: BTreeMap<String, Sourced<Value>>,
    pub caps: BTreeMap<String, CapState>,
    pub plan_hash: Option<Hash>,
    pub policy: PolicyState,
    pub leases: BTreeMap<LeaseId, (String, Timestamp)>,
    pub items: Vec<(EntryId, Item)>,
    pub calls: BTreeMap<CallId, (String, Consequence, Value)>,
    pub open_calls: Vec<CallId>,
    pub open_ops: BTreeMap<OpId, (CallId, bool, bool)>,
    pub op_status: BTreeMap<OpId, OpStatus>,
    pub turn: Option<TurnId>,
    pub turns: u64,
    pub pending_approvals: BTreeMap<EntryId, Option<CallId>>,
    pub replaces: Vec<(EntryId, EntryId, Option<String>)>,
    pub checkpoints: Vec<(CheckpointId, Hash, EntryId)>,
    pub invariants: BTreeMap<String, (Verdict, Hash)>,
    pub usage: Usage,
    pub meta: Option<(Option<String>, Vec<String>, Option<ProcessIdentity>)>,
    pub last_transition: Option<Transition>,
    pub seen: Option<EntryId>,
    pub head: Option<EntryId>,
    /// Capability-owned projections, keyed by namespace.
    pub custom: BTreeMap<String, Value>,
}

impl State {
    pub fn active_caps(&self) -> impl Iterator<Item = &String> {
        self.caps.iter().filter(|(_, s)| matches!(s, CapState::Active)).map(|(k, _)| k)
    }
}

/// A capability-owned fold step for its own `Custom` kinds (02: `Registrar::entry_kind`).
pub type CustomFold = fn(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String>;
pub type CustomFolds = BTreeMap<String, CustomFold>;

pub fn fold(entries: &[Entry], custom: &CustomFolds) -> Result<State, LogError> {
    entries.iter().try_fold(State::default(), |s, e| step(s, e, custom))
}

fn inv(rule: &'static str, at: EntryId, detail: impl Into<String>) -> LogError {
    LogError::Invariant { rule, at, detail: detail.into() }
}

/// One step of the fold. Invariants are checked here and never repaired.
pub fn step(mut s: State, e: &Entry, custom: &CustomFolds) -> Result<State, LogError> {
    s.head = Some(e.id);
    match &e.kind {
        EntryKind::ConfigResolved { key, sourced } => {
            s.config.insert(key.clone(), sourced.clone());
        }
        EntryKind::CapActivated { id, .. } => {
            s.caps.insert(id.clone(), CapState::Active);
        }
        EntryKind::PlanSealed { plan_hash, .. } => s.plan_hash = Some(*plan_hash),
        EntryKind::CapDelta { id, op, reason } => {
            let st = match op {
                CapDeltaOp::Activated | CapDeltaOp::Replaced { .. } => CapState::Active,
                CapDeltaOp::Deactivated => CapState::Disabled { why: reason.clone() },
                CapDeltaOp::Broken { err } => CapState::Broken { err: err.clone() },
                CapDeltaOp::Rejected { .. } => return Ok(s),
            };
            s.caps.insert(id.clone(), st);
        }
        EntryKind::PolicyEpoch { epoch, allowed, preset, approval_ref } => {
            let widens = !allowed.is_subset(&s.policy.allowed);
            if widens && s.policy.epoch.0 > 0 && (approval_ref.is_none() || e.by != Principal::Operator) {
                return Err(inv("policy.widen_requires_operator_approval", e.id, format!("{} → {}", s.policy.allowed, allowed)));
            }
            if epoch.0 < s.policy.epoch.0 {
                return Err(inv("policy.epoch_monotone", e.id, format!("{} < {}", epoch.0, s.policy.epoch.0)));
            }
            s.policy = PolicyState { epoch: *epoch, allowed: *allowed, preset: preset.clone() };
        }
        EntryKind::ApprovalRequested { call, .. } => {
            if s.turn.is_none() {
                return Err(inv("approval.inside_turn", e.id, "approval requested with no open turn"));
            }
            s.pending_approvals.insert(e.id, call.clone());
        }
        EntryKind::ApprovalDecided { req, .. } => {
            if s.turn.is_none() {
                return Err(inv("approval.inside_turn", e.id, "decision with no open turn"));
            }
            if s.pending_approvals.remove(req).is_none() {
                return Err(inv("approval.decided_matches_requested", e.id, format!("no open request {}", req.0)));
            }
        }
        EntryKind::LeaseGranted { lease, subject, expires } => {
            s.leases.insert(*lease, (subject.clone(), *expires));
        }
        EntryKind::LeaseRevoked { lease, .. } => {
            s.leases.remove(lease);
        }
        EntryKind::TurnStart { turn } => {
            if s.turn.is_some() {
                return Err(inv("turn.no_nesting", e.id, "turn started inside an open turn"));
            }
            s.items.push((e.id, Item::Boundary));
            s.turn = Some(*turn);
            s.turns += 1;
        }
        EntryKind::TurnEnd { turn } => {
            if s.turn != Some(*turn) {
                return Err(inv("turn.end_matches_start", e.id, format!("{:?} vs {:?}", s.turn, turn)));
            }
            if !s.pending_approvals.is_empty() {
                return Err(inv("approval.inside_turn", e.id, "turn ended with an undecided approval"));
            }
            s.turn = None;
        }
        EntryKind::StepEnd { transition, .. } => s.last_transition = Some(transition.clone()),
        EntryKind::RequestHeader { .. } => s.items.push((e.id, Item::Boundary)),
        EntryKind::UserMessage { form, text, .. } => s.items.push((e.id, Item::User { form: *form, text: text.clone() })),
        EntryKind::AssistantMessage { content, usage, .. } => {
            s.usage = s.usage + *usage;
            s.items.push((e.id, Item::Assistant { content: content.clone() }));
        }
        EntryKind::ToolCall { call, tool, args, class } => {
            if s.calls.contains_key(call) {
                return Err(inv("tool.call_id_unique", e.id, call.0.clone()));
            }
            s.calls.insert(call.clone(), (tool.clone(), *class, args.clone()));
            s.open_calls.push(call.clone());
        }
        EntryKind::ToolResult { call, outcome, content, .. } => {
            let pos = s.open_calls.iter().position(|c| c == call).ok_or_else(|| inv("tool.at_most_one_result", e.id, call.0.clone()))?;
            s.open_calls.remove(pos);
            let tool = s.calls.get(call).map(|c| c.0.clone()).unwrap_or_default();
            let is_error = !matches!(outcome, ToolOutcome::Ok);
            s.items.push((e.id, Item::Result { call: call.clone(), tool, content: content.clone(), is_error }));
        }
        EntryKind::OpStarted { op, call, rerunnable, external_effect, .. } => {
            s.open_ops.insert(*op, (call.clone(), *rerunnable, *external_effect));
        }
        EntryKind::OpDone { op, status } => {
            if s.open_ops.remove(op).is_none() {
                return Err(inv("op.done_matches_started", e.id, format!("op {}", op.0)));
            }
            s.op_status.insert(*op, status.clone());
        }
        EntryKind::SurfaceReplace { start, end, summary, .. } => {
            let splits = s.items.iter().any(|(id, it)| {
                if let Item::Result { call, .. } = it {
                    let result_in = *id >= *start && *id <= *end;
                    let call_in = s.items.iter().any(|(aid, a)| {
                        matches!(a, Item::Assistant { content } if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id: cid, .. } if cid == call)))
                            && *aid >= *start && *aid <= *end
                    });
                    result_in != call_in
                } else {
                    false
                }
            });
            if splits {
                return Err(inv("replace.never_splits_call_result", e.id, format!("{}..{}", start.0, end.0)));
            }
            s.replaces.push((*start, *end, summary.clone()));
        }
        EntryKind::WorkspaceCheckpoint { id, tree, .. } => s.checkpoints.push((*id, *tree, e.id)),
        EntryKind::InvariantChecked { id, verdict, code_hash, .. } => {
            s.invariants.insert(id.clone(), (verdict.clone(), *code_hash));
        }
        EntryKind::SessionMeta { title, tags, process, .. } => s.meta = Some((title.clone(), tags.clone(), *process)),
        EntryKind::SessionSeen => s.seen = Some(e.id),
        EntryKind::Custom { ns, name, body } => {
            if let Some(f) = custom.get(ns) {
                let next = f(s.custom.get(ns), name, body).map_err(|d| inv("custom.fold", e.id, format!("{ns}/{name}: {d}")))?;
                s.custom.insert(ns.clone(), next);
            }
        }
        _ => {}
    }
    Ok(s)
}

/// Fixed placeholder rendered for a `ToolCall` with no `ToolResult` at request time (16).
pub const TOOL_CALL_RUNNING: &str =
    "Tool call is still running. Its result arrives in a later turn: continue with independent work, or end your turn to wait for it.";

/// `06`/`16`: fixed pipeline → frozen request.
/// 1. apply every `SurfaceReplace` in log order
/// 2. placeholders for calls still open; late results at their log position
/// 3. empty-output marker
/// 4. dedupe consecutive identical snapshots
pub fn derive_messages(state: &State) -> Vec<Message> {
    // 1. replaces
    let mut items: Vec<(EntryId, Item)> = Vec::new();
    let mut emitted_summary: BTreeSet<usize> = BTreeSet::new();
    for (id, it) in &state.items {
        match state.replaces.iter().position(|(a, b, _)| id >= a && id <= b) {
            Some(i) => {
                if emitted_summary.insert(i)
                    && let Some(sum) = &state.replaces[i].2
                {
                    items.push((
                        *id,
                        Item::User { form: Form::Snapshot, text: format!("<compact-checkpoint>\n{sum}\n</compact-checkpoint>") },
                    ));
                }
            }
            None => items.push((*id, it.clone())),
        }
    }

    // 2. attach results; placeholders for results not yet adjacent
    let mut out: Vec<Message> = Vec::new();
    let mut answered: BTreeSet<CallId> = BTreeSet::new();
    let mut i = 0;
    while i < items.len() {
        match &items[i].1 {
            Item::User { form, text } => {
                // 4. dedupe consecutive identical snapshots
                let dup = *form == Form::Snapshot && out.last().map(|m| m.role == Role::User && m.text() == *text).unwrap_or(false);
                if !dup {
                    out.push(Message::user(text.clone()));
                }
                i += 1;
            }
            Item::Assistant { content } => {
                out.push(Message::assistant(content.clone()));
                let uses: Vec<CallId> =
                    content.iter().filter_map(|b| if let ContentBlock::ToolUse { id, .. } = b { Some(id.clone()) } else { None }).collect();
                i += 1;
                if uses.is_empty() {
                    continue;
                }
                // results that directly follow this assistant message, before the next assistant message
                let mut adjacent: BTreeMap<CallId, ContentBlock> = BTreeMap::new();
                let mut j = i;
                while j < items.len() {
                    match &items[j].1 {
                        Item::Result { call, tool, content, is_error } if uses.contains(call) => {
                            adjacent.insert(call.clone(), result_block(call, tool, content, *is_error));
                            j += 1;
                        }
                        Item::Assistant { .. } | Item::Boundary => break,
                        _ => j += 1,
                    }
                }
                let mut blocks = Vec::new();
                for u in &uses {
                    match adjacent.remove(u) {
                        Some(b) => {
                            answered.insert(u.clone());
                            blocks.push(b);
                        }
                        None => blocks.push(ContentBlock::ToolResult {
                            id: u.clone(),
                            content: vec![ContentBlock::text(TOOL_CALL_RUNNING)],
                            is_error: false,
                        }),
                    }
                }
                out.push(Message { role: Role::User, content: blocks });
            }
            Item::Boundary => i += 1,
            Item::Result { call, tool, content, is_error } => {
                if !answered.contains(call) {
                    // late result: a second result for a call that showed a placeholder
                    answered.insert(call.clone());
                    out.push(Message { role: Role::User, content: vec![result_block(call, tool, content, *is_error)] });
                }
                i += 1;
            }
        }
    }
    out
}

fn result_block(call: &CallId, tool: &str, content: &[ContentBlock], is_error: bool) -> ContentBlock {
    // 3. empty-output marker
    let content = if content.iter().all(|b| b.as_text().map(|t| t.trim().is_empty()).unwrap_or(false)) {
        vec![ContentBlock::text(format!("({tool} completed with no output)"))]
    } else {
        content.to_vec()
    };
    ContentBlock::ToolResult { id: call.clone(), content, is_error }
}

/// Hash of a frozen request's message list; recorded in `RequestHeader.request_hash` for replay equality.
pub fn request_hash(messages: &[Message], tools: &[String]) -> Hash {
    Hash::of_json(&(messages, tools))
}
