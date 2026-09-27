//! JSON projections the API and the mux serve. Every payload is derived from the fold, the plan
//! or the registry; none carries a live object. Frame shapes match surface-rpc (`{v, reply_to,
//! event, payload}`).

use aho_cap::{CapabilityId, Registry};
use aho_compose::Planned;
use aho_fs::SnapshotHandle;
use aho_llm::StreamEvent;
use aho_log::{CapState, CheckpointCause, Entry, EntryKind, RewindMode, State, Verdict};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_proc::OpTable;
use aho_session::Session;
use aho_types::*;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u16 = 1;

/// One server frame, serialized.
pub fn frame(event: &str, payload: Value) -> String {
    json!({ "v": PROTOCOL_VERSION, "reply_to": Value::Null, "event": event, "payload": payload }).to_string()
}

pub fn stream_json(ev: &StreamEvent) -> Value {
    match ev {
        StreamEvent::Text(t) => json!({ "kind": "text", "text": t }),
        StreamEvent::Thinking(t) => json!({ "kind": "thinking", "text": t }),
        StreamEvent::ToolUseStart { id, name } => json!({ "kind": "tool_use_start", "id": id, "name": name }),
        StreamEvent::ToolUseEnd { id, name, args } => json!({ "kind": "tool_use_end", "id": id, "name": name, "args": args }),
        StreamEvent::Usage(u) => json!({ "kind": "usage", "usage": u }),
    }
}

pub fn entry_json(e: &Entry) -> Value {
    serde_json::to_value(e).unwrap_or_else(|err| json!({ "id": e.id, "unserializable": err.to_string() }))
}

pub fn reason_label(r: &DecisionReason) -> &'static str {
    match r {
        DecisionReason::Rule { .. } => "Rule",
        DecisionReason::Preset(_) => "Preset",
        DecisionReason::Hook(_) => "Hook",
        DecisionReason::Guard(_) => "Guard",
        DecisionReason::SafetyCheck(_) => "SafetyCheck",
        DecisionReason::Classifier => "Classifier",
        DecisionReason::WorkingDir => "WorkingDir",
        DecisionReason::AsyncAgent => "AsyncAgent",
        DecisionReason::Delegation => "Delegation",
        DecisionReason::Escalation => "Escalation",
        DecisionReason::Unavailable => "Unavailable",
        DecisionReason::Passthrough => "Passthrough",
        DecisionReason::ToolCheck(_) => "ToolCheck",
    }
}

/// The capability that registered `tool` (static or runtime-registered).
pub fn tool_cap(reg: &Registry, tool: &str) -> Option<String> {
    reg.tools
        .iter()
        .find(|(_, d, _)| d.name == tool)
        .map(|(c, _, _)| c.name.to_string())
        .or_else(|| reg.dynamic_tools.iter().find(|(_, s, _)| s.name == tool).map(|(c, _, _)| c.name.to_string()))
}

/// The approval card as data: the same fields the TUI modal shows.
pub fn approval_json(id: u64, card: &ApprovalCard, cap: Option<String>, state: &State) -> Value {
    let now = Timestamp::now();
    let leases: Vec<&String> = state.leases.values().filter(|(_, exp)| *exp > now).map(|(s, _)| s).collect();
    let preset_cut = card.suggested_cut.as_deref().and_then(|c| c.strip_prefix("preset ")).map(str::trim);
    json!({
        "id": id,
        "tool": card.tool,
        "cap": cap,
        "class": card.class,
        "args": card.args,
        "reason": reason_label(&card.reason),
        "why": card.rendered,
        "world": card.world.name,
        "enforced_by": card.world.enforced_by,
        "lease": if leases.is_empty() { "none yet".to_string() } else { format!("{} active", leases.len()) },
        "suggested_cut": card.suggested_cut,
        "preset_cut": preset_cut,
        "epoch_next": state.policy.epoch.0.max(1) + 1,
    })
}

fn cause_text(c: &CheckpointCause, calls: &std::collections::BTreeMap<CallId, String>) -> String {
    match c {
        CheckpointCause::ToolResult(call) => format!("after {}", calls.get(call).cloned().unwrap_or_else(|| format!("call {}", call.0))),
        CheckpointCause::TurnEnd => "turn end".into(),
        CheckpointCause::Manual => "manual".into(),
        CheckpointCause::PreRewind => "before rewind".into(),
    }
}

/// Checkpoint markers of the current branch with the turn and prompt they belong to.
pub fn checkpoints_json(branch: &[Entry]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut turn = 0u64;
    let mut prompt = String::new();
    let mut calls = std::collections::BTreeMap::new();
    for e in branch {
        match &e.kind {
            EntryKind::TurnStart { turn: t } => turn = t.0,
            EntryKind::UserMessage { form: Form::Prompt, text, .. } => prompt = text.clone(),
            EntryKind::ToolCall { call, tool, .. } => {
                calls.insert(call.clone(), tool.clone());
            }
            EntryKind::WorkspaceCheckpoint { id, tree, cause, .. } => out.push(json!({
                "id": id,
                "entry": e.id,
                "tree": tree,
                "turn": turn,
                "prompt": prompt,
                "cause": cause_text(cause, &calls),
            })),
            _ => {}
        }
    }
    out
}

/// `POST /api/session/fold`: a snapshot of the folded head (`fold(at)`).
pub fn fold_json(session: &Session, state: &State, branch: &[Entry]) -> Value {
    let caps: serde_json::Map<String, Value> = state.caps.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let code = state.checkpoints.last().map(|c| c.1);
    let invariants: Vec<Value> = state
        .invariants
        .iter()
        .map(|(id, (v, h))| {
            let stale = code.is_some_and(|c| c != *h);
            json!({ "id": id, "verdict": if stale { json!(Verdict::Stale) } else { json!(v) }, "recorded": v, "stale": stale })
        })
        .collect();
    json!({
        "session": format!("{:x}", session.id.0),
        "head": state.head,
        "turn": state.turn,
        "turn_open": state.turn.is_some(),
        "turns": state.turns,
        "policy": { "epoch": state.policy.epoch, "preset": state.policy.preset, "allowed": state.policy.allowed },
        "usage": state.usage,
        "open_calls": state.open_calls,
        "pending_approvals": state.pending_approvals.len(),
        "last_transition": state.last_transition,
        "caps": caps,
        "checkpoints": checkpoints_json(branch),
        "invariants": invariants,
    })
}

/// Tree nodes (the same JSON `aho tree --json` prints) with the fold's runtime states applied.
pub fn tree_nodes(planned: &Planned, state: &State) -> Vec<aho_inspect::TreeNode> {
    let mut nodes = aho_inspect::tree(&planned.plan);
    let config = if state.config.is_empty() { &planned.loadout.config } else { &state.config };
    aho_inspect::govern(&mut nodes, config);
    for n in nodes.iter_mut() {
        if let Some(st) = state.caps.get(&n.id)
            && matches!(n.state, CapState::Active)
            && !matches!(st, CapState::Active)
        {
            n.state = st.clone();
        }
    }
    nodes
}

pub fn tree_json(planned: &Planned, state: &State, label: &str) -> Value {
    let nodes = tree_nodes(planned, state);
    json!({
        "text": aho_inspect::render_tree(label, &nodes, &planned.plan),
        "preset": planned.plan.preset,
        "world": planned.plan.world.name,
        "plan": planned.plan.hash.short(),
        "nodes": nodes,
    })
}

pub fn jobs_json(ops: &OpTable) -> Vec<Value> {
    ops.list()
        .into_iter()
        .map(|i| json!({ "op": i.op, "call": i.call, "tool": i.tool, "kind": i.kind, "status": i.status, "started": i.started }))
        .collect()
}

/// Panels whose capability is Active in the fold (a disabled capability ships no panel).
pub fn active_slots(panels: &[(CapabilityId, &'static str, &'static [&'static str])], state: &State) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    panels
        .iter()
        .filter(|(cap, _, _)| state.caps.get(cap.name).is_none_or(|s| matches!(s, CapState::Active)))
        .filter(|(_, slot, _)| seen.insert(*slot))
        .map(|(cap, slot, projections)| json!({ "slot": slot, "cap": cap.name, "projections": projections }))
        .collect()
}

pub fn parse_mode(s: &str) -> Option<RewindMode> {
    match s {
        "both" => Some(RewindMode::Both),
        "chat" | "chat_only" => Some(RewindMode::ChatOnly),
        "code" | "code_only" => Some(RewindMode::CodeOnly),
        _ => None,
    }
}

/// The effect of a rewind; nothing is applied (plan-then-apply).
pub async fn rewind_plan(session: &Session, checkpoint: CheckpointId, mode: RewindMode) -> Result<Value, String> {
    let state = session.fold().map_err(|e| e.to_string())?;
    let (_, tree, at) = state
        .checkpoints
        .iter()
        .find(|c| c.0 == checkpoint)
        .copied()
        .ok_or_else(|| format!("checkpoint c{} is not on the current branch", checkpoint.0))?;
    let restore = if mode == RewindMode::ChatOnly {
        Value::Null
    } else {
        let snap = session
            .registry()
            .handle::<SnapshotHandle>()
            .ok_or_else(|| "this world has no Snapshot provider, so files cannot be restored; `chat` mode still works".to_string())?;
        let now = snap.0.tree().await.map_err(|e| format!("could not hash the current workspace: {e}"))?;
        let d = snap.0.diff(now, tree).await.map_err(|e| format!("could not diff against c{}: {e}", checkpoint.0))?;
        json!({ "changed": d.changed, "deleted": d.deleted })
    };
    let kill: Vec<String> = session
        .ops()
        .running()
        .iter()
        .filter_map(|(op, _, _)| session.ops().info(*op))
        .map(|i| format!("op{} {}", i.op.0, i.tool))
        .collect();
    Ok(json!({
        "checkpoint": checkpoint,
        "mode": mode,
        "fork_at": if mode == RewindMode::CodeOnly { Value::Null } else { json!(at) },
        "restore": restore,
        "kill": kill,
        "pre_rewind_checkpoint": true,
    }))
}
