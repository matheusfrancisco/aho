//! Typed views of what the server sends: events, and the replies the typed client methods
//! decode. Log entries, transitions, and policy state are the aho-log types themselves; only the
//! payloads surface-rpc projects with `json!` (stream chunks, approval cards, the fold) have a
//! type here, and `tests/schema.rs` holds each to the frames the server sends.

use std::collections::BTreeMap;

use aho_llm::StreamEvent;
use aho_log::{CapState, DetachCause, Entry, GrantScope, OpStatus, PolicyState, Transition, Verdict};
use aho_types::*;
use serde::{Deserialize, Serialize};
use surface_rpc::ServerFrame;

/// A server-initiated frame, in wire order. Events that precede a reply on the wire are in the
/// stream before that reply resolves its request.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A log entry: replayed after `subscribe`, then live.
    Entry(Box<Entry>),
    /// A live model stream chunk (never durable; the log has the final message).
    Stream(StreamEvent),
    /// The session went idle after a turn.
    Idle(Idle),
    /// A tool call waits for a human; answer it with [`crate::Client::approve`].
    Approval(Box<ApprovalCard>),
    /// A session job finished while no turn ran; the next turn delivers its outcome.
    JobFinished(OpId),
    /// A failure outside any request (a lagging client, a log write that did not land).
    Error(String),
    /// A frame this client cannot type: a newer server's event kind, or a payload that does not
    /// decode. Kept raw; the wire rule is that readers skip what they do not know.
    Unknown { event: String, payload: Value },
}

impl Event {
    pub fn from_frame(f: ServerFrame) -> Event {
        let typed = match f.event.as_str() {
            "entry" => serde_json::from_value::<Entry>(f.payload.clone()).map(|e| Event::Entry(Box::new(e))),
            "stream" => serde_json::from_value::<StreamWire>(f.payload.clone()).map(|s| Event::Stream(s.into())),
            "idle" => serde_json::from_value::<Idle>(f.payload.clone()).map(Event::Idle),
            "approval" => serde_json::from_value::<ApprovalCard>(f.payload.clone()).map(|c| Event::Approval(Box::new(c))),
            "error" => serde_json::from_value::<ErrorPayload>(f.payload.clone()).map(|e| Event::Error(e.message)),
            "job_finished" => serde_json::from_value::<JobFinishedWire>(f.payload.clone()).map(|j| Event::JobFinished(j.op)),
            _ => return Event::Unknown { event: f.event, payload: f.payload },
        };
        typed.unwrap_or(Event::Unknown { event: f.event, payload: f.payload })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Idle {
    pub turn: TurnId,
    pub transition: Transition,
}

/// The approval card as the server sends it: the fields every surface renders.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCard {
    /// Answer with this id.
    pub id: u64,
    pub tool: String,
    /// The capability that registered the tool.
    pub cap: Option<String>,
    pub class: Consequence,
    pub args: Value,
    /// The decision-chain link that asked (`Rule`, `Preset`, …).
    pub reason: String,
    /// The rendered card text.
    pub why: String,
    pub world: String,
    pub enforced_by: String,
    /// Smallest lattice change that would let this call pass.
    pub suggested_cut: Option<String>,
    /// The preset [`Answer::Preset`] writes; `None` = no preset admits the call.
    pub preset_cut: Option<String>,
    pub epoch_next: Epoch,
}

#[derive(Deserialize)]
struct ErrorPayload {
    message: String,
}

#[derive(Deserialize)]
struct JobFinishedWire {
    op: OpId,
}

/// `stream` payload as surface-rpc projects [`StreamEvent`].
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StreamWire {
    Text { text: String },
    Thinking { text: String },
    ToolUseStart { id: CallId, name: String },
    ToolUseEnd { id: CallId, name: String, args: Value },
    Usage { usage: Usage },
}

impl From<StreamWire> for StreamEvent {
    fn from(s: StreamWire) -> StreamEvent {
        match s {
            StreamWire::Text { text } => StreamEvent::Text(text),
            StreamWire::Thinking { text } => StreamEvent::Thinking(text),
            StreamWire::ToolUseStart { id, name } => StreamEvent::ToolUseStart { id, name },
            StreamWire::ToolUseEnd { id, name, args } => StreamEvent::ToolUseEnd { id, name, args },
            StreamWire::Usage { usage } => StreamEvent::Usage(usage),
        }
    }
}

// ───────────────────────────── replies ─────────────────────────────

#[derive(Deserialize)]
pub(crate) struct PromptReply {
    pub transition: Transition,
}

#[derive(Deserialize)]
pub(crate) struct RewindReply {
    pub entry: EntryId,
}

/// One open session job (`job.list`): running, or finished and waiting for delivery.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub op: OpId,
    /// The call the job detached from (already answered with the job id).
    pub call: CallId,
    pub tool: String,
    pub cause: DetachCause,
    /// `None` while running; else finished, delivered at the next request boundary.
    pub status: Option<OpStatus>,
    /// `None` when the serving process does not run it (left by a restart).
    pub started: Option<Timestamp>,
}

#[derive(Deserialize)]
pub(crate) struct JobList {
    pub jobs: Vec<Job>,
}

/// `session.subscribe`: the number of entries replayed before the reply, and the log head.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscribed {
    pub replayed: u64,
    pub head: Option<EntryId>,
}

/// `approval.answer`: empty, or the preset a [`Answer::Preset`] wrote.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answered {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// One operator-facing line per written key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrote: Vec<String>,
}

/// An answer to an approval card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Allow(GrantScope),
    Reject,
    Cancel,
    /// Write the card's `preset_cut` as `policy.preset` (plan → apply) and allow the call for
    /// the session.
    Preset,
}

impl Answer {
    pub(crate) fn params(self, id: u64) -> surface_rpc::wire::AnswerParams {
        let (outcome, scope) = match self {
            Answer::Allow(s) => (
                "allow",
                Some(match s {
                    GrantScope::Once => "once",
                    GrantScope::Session => "session",
                    GrantScope::UntilRevoked => "until_revoked",
                }),
            ),
            Answer::Reject => ("reject", None),
            Answer::Cancel => ("cancel", None),
            Answer::Preset => ("preset", None),
        };
        surface_rpc::wire::AnswerParams { id, outcome: outcome.to_string(), scope: scope.map(str::to_string) }
    }
}

/// `session.fold`: the folded state a thin client reduces over.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fold {
    pub head: Option<EntryId>,
    pub turns: u64,
    pub turn: Option<TurnId>,
    pub plan_hash: Option<Hash>,
    pub policy: PolicyState,
    pub caps: BTreeMap<String, CapState>,
    /// Resolved value per config key.
    pub config: BTreeMap<String, Value>,
    pub open_calls: Vec<CallId>,
    pub pending_approvals: Vec<EntryId>,
    pub checkpoints: Vec<Checkpoint>,
    pub usage: Usage,
    pub last_transition: Option<Transition>,
    pub invariants: BTreeMap<String, InvariantState>,
    /// Capability-owned projections, keyed by namespace.
    pub custom: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub tree: Hash,
    pub entry: EntryId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvariantState {
    pub verdict: Verdict,
    pub code_hash: Hash,
}
