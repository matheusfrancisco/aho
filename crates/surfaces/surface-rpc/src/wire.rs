//! The frame shapes: the envelope, every method's params, and the JSON projections of
//! session events. `tests/fixtures/frames.json` snapshots all of them; a diff there is a
//! protocol change and needs a version decision (minor = additive fields, major = handshake).

use aho_approval::Decided;
use aho_llm::StreamEvent;
use aho_log::{Entry, GrantScope, RewindMode, State, Transition};
use aho_policy::ApprovalCard;
use aho_types::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
pub use surface_print::{preset_cut, reason_label};

/// Lowest and highest protocol major this server speaks.
pub const VERSION_MIN: u16 = 1;
pub const VERSION_MAX: u16 = 1;

/// Every method a client may call, as advertised in `hello_ok.capabilities`.
pub const METHODS: &[&str] = &[
    "session.prompt",
    "session.steer",
    "session.abort",
    "session.subscribe",
    "session.fold",
    "session.rewind",
    "approval.answer",
    "caps.list",
    "caps.tree",
    "caps.explain",
    "caps.why",
    "caps.plan",
    "caps.set",
    "caps.enable",
    "caps.disable",
    "caps.allow",
    "caps.preset",
];

/// Client → server. `id` absent = notification (no reply). Unknown fields are ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    pub v: u16,
    #[serde(default)]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Server → client. `reply_to` names the request a `result`/`error` answers; events carry `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame {
    pub v: u16,
    pub reply_to: Option<u64>,
    pub event: String,
    pub payload: Value,
}

impl ServerFrame {
    pub fn new(v: u16, reply_to: Option<u64>, event: &str, payload: Value) -> ServerFrame {
        ServerFrame { v, reply_to, event: event.to_string(), payload }
    }
    pub fn result(v: u16, id: Option<u64>, payload: Value) -> ServerFrame {
        ServerFrame::new(v, id, "result", payload)
    }
    pub fn error(v: u16, id: Option<u64>, message: impl Into<String>) -> ServerFrame {
        ServerFrame::new(v, id, "error", json!({ "message": message.into() }))
    }
    pub fn entry(v: u16, e: &Entry) -> ServerFrame {
        ServerFrame::new(v, None, "entry", serde_json::to_value(e).unwrap_or(Value::Null))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub min: u16,
    pub max: u16,
    #[serde(default)]
    pub client_instance: String,
    #[serde(default)]
    pub has_local_history: bool,
    #[serde(default)]
    pub allow_takeover: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelloOk {
    pub version: u16,
    pub capabilities: Vec<String>,
    /// Session id, lowercase hex.
    pub session: String,
    /// Last entry on the log; resubscribe `from` here to catch up.
    pub head: Option<EntryId>,
}

/// The highest version both sides speak.
pub fn negotiate(h: &Hello) -> Result<u16, String> {
    if h.min > h.max {
        return Err(format!("hello says min {} above max {}; send the range of versions the client speaks", h.min, h.max));
    }
    let (lo, hi) = (h.min.max(VERSION_MIN), h.max.min(VERSION_MAX));
    if lo > hi {
        return Err(format!(
            "no common protocol version: the client speaks {}..={}, this server speaks {VERSION_MIN}..={VERSION_MAX}. Upgrade the older side.",
            h.min, h.max
        ));
    }
    Ok(hi)
}

// ───────────────────────────── params ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptParams {
    pub text: String,
    #[serde(default)]
    pub client_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SteerParams {
    pub text: String,
    #[serde(default)]
    pub urgent: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscribeParams {
    /// Replay entries appended after this id (`None` = from the first entry), then go live.
    #[serde(default)]
    pub from: Option<EntryId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RewindParams {
    pub checkpoint: u64,
    /// `both` (default) | `chat` | `code`.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerParams {
    pub id: u64,
    /// `allow` | `reject` | `cancel` (also `allow_once`, `allow_session`), or `preset`: write the
    /// card's `preset_cut` as `policy.preset` (plan → apply) and allow this call for the session.
    pub outcome: String,
    /// `once` (default) | `session` | `until_revoked`; only an allow carries a wider scope.
    #[serde(default)]
    pub scope: Option<String>,
}

pub fn rewind_mode(m: Option<&str>) -> Result<RewindMode, String> {
    match m.unwrap_or("both") {
        "both" => Ok(RewindMode::Both),
        "chat" | "chat_only" => Ok(RewindMode::ChatOnly),
        "code" | "code_only" => Ok(RewindMode::CodeOnly),
        other => Err(format!("rewind mode `{other}` is not one of both, chat, code")),
    }
}

pub fn decided(outcome: &str, scope: Option<&str>) -> Result<Decided, String> {
    let scope = match scope.unwrap_or("once") {
        "once" => GrantScope::Once,
        "session" => GrantScope::Session,
        "until_revoked" => GrantScope::UntilRevoked,
        other => return Err(format!("scope `{other}` is not one of once, session, until_revoked")),
    };
    match outcome {
        "allow" | "allowed_once" | "allow_once" => Ok(Decided::allowed(scope)),
        "allow_session" => Ok(Decided::allowed(GrantScope::Session)),
        "reject" | "rejected" => Ok(Decided::rejected()),
        "cancel" | "cancelled" => Ok(Decided::cancelled()),
        other => Err(format!("outcome `{other}` is not one of allow, reject, cancel")),
    }
}

// ───────────────────────────── event payloads ─────────────────────────────

pub fn stream_payload(s: &StreamEvent) -> Value {
    match s {
        StreamEvent::Text(t) => json!({ "kind": "text", "text": t }),
        StreamEvent::Thinking(t) => json!({ "kind": "thinking", "text": t }),
        StreamEvent::ToolUseStart { id, name } => json!({ "kind": "tool_use_start", "id": id, "name": name }),
        StreamEvent::ToolUseEnd { id, name, args } => json!({ "kind": "tool_use_end", "id": id, "name": name, "args": args }),
        StreamEvent::Usage(u) => json!({ "kind": "usage", "usage": u }),
    }
}

pub fn idle_payload(turn: TurnId, transition: &Transition) -> Value {
    json!({ "turn": turn.0, "transition": transition })
}

/// The approval card as data: the same fields every surface renders.
pub fn approval_payload(id: u64, card: &ApprovalCard, cap: Option<&str>, epoch_next: u32) -> Value {
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
        "suggested_cut": card.suggested_cut,
        "preset_cut": preset_cut(card),
        "epoch_next": epoch_next,
    })
}

/// `session.fold`: the folded state a thin client reduces over (surface-local state excluded).
pub fn state_payload(s: &State) -> Value {
    let config: serde_json::Map<String, Value> = s.config.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect();
    let invariants: serde_json::Map<String, Value> =
        s.invariants.iter().map(|(k, (verdict, code))| (k.clone(), json!({ "verdict": verdict, "code_hash": code }))).collect();
    json!({
        "head": s.head,
        "turns": s.turns,
        "turn": s.turn,
        "plan_hash": s.plan_hash,
        "policy": s.policy,
        "caps": s.caps,
        "config": config,
        "open_calls": s.open_calls,
        "pending_approvals": s.pending_approvals.keys().collect::<Vec<_>>(),
        "checkpoints": s.checkpoints.iter().map(|(id, tree, at)| json!({ "id": id, "tree": tree, "entry": at })).collect::<Vec<_>>(),
        "usage": s.usage,
        "last_transition": s.last_transition,
        "invariants": invariants,
        "custom": s.custom,
    })
}
