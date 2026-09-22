//! The rpc protocol as data (08, 12): every method with its params and result, every event with
//! its payload, and every type those reference. The JSON Schema and the TS/Python SDKs are
//! generated from this table ([`crate::codegen`]).
//!
//! surface-rpc builds its payloads with `serde_json::json!` and the log types carry no schema
//! derives, so the table is maintained here and held to the server by `tests/schema.rs`: every
//! frame surface-rpc produces — its committed snapshot, one sample of every entry kind and
//! transition, and the live frames of a real session — must validate strictly against it
//! ([`crate::validate`]). A field added, removed, or renamed on either side fails that test.
//!
//! Wire rules the table encodes (12): every frame carries `v`; unknown fields are ignored by
//! readers (so generated types are open), a minor version only adds fields, a major version is a
//! handshake decision.

/// A JSON type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Str,
    /// Non-negative integer.
    UInt,
    /// Signed integer.
    Int,
    Bool,
    /// Any JSON value (a shape owned by another crate or by a tool).
    Json,
    /// One literal string.
    Const(&'static str),
    /// The value or `null`.
    Opt(&'static Ty),
    List(&'static Ty),
    /// An object with arbitrary keys and values of one type.
    Map(&'static Ty),
    /// A fixed-length array.
    Tuple(&'static [Ty]),
    /// A named [`Def`].
    Ref(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    /// The key may be absent (`#[serde(default)]` on the reading side).
    pub optional: bool,
    pub doc: &'static str,
}

impl Field {
    pub const fn doc(self, doc: &'static str) -> Field {
        Field { doc, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variant {
    /// Value of the union's tag key.
    pub tag: &'static str,
    pub fields: &'static [Field],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Alias(Ty),
    /// A string from a closed set.
    Enum(&'static [&'static str]),
    Object(&'static [Field]),
    /// An object whose `tag` key picks the variant (serde internally/adjacently tagged enums).
    Union {
        tag: &'static str,
        variants: &'static [Variant],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Def {
    pub name: &'static str,
    pub doc: &'static str,
    pub shape: Shape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Method {
    /// Wire name, as in `hello_ok.capabilities`.
    pub name: &'static str,
    /// SDK method name (snake_case; the TS client camelCases it).
    pub sdk: &'static str,
    /// Params def; `None` = the method takes none (send `{}` or omit).
    pub params: Option<&'static str>,
    pub result: Ty,
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub name: &'static str,
    pub payload: Ty,
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Protocol {
    pub version_min: u16,
    pub version_max: u16,
    pub defs: &'static [Def],
    pub methods: &'static [Method],
    pub events: &'static [Event],
}

impl Protocol {
    pub fn def(&self, name: &str) -> Option<&'static Def> {
        self.defs.iter().find(|d| d.name == name)
    }
    pub fn method(&self, name: &str) -> Option<&'static Method> {
        self.methods.iter().find(|m| m.name == name)
    }
    pub fn event(&self, name: &str) -> Option<&'static Event> {
        self.events.iter().find(|e| e.name == name)
    }
}

use Ty::{Bool, Int, Json, List, Map, Opt, Ref, Str, Tuple, UInt};

const HASH: Ty = Ref("Hash");
const ENTRY_ID: Ty = Ref("EntryId");
const CALL_ID: Ty = Ref("CallId");
const TIMESTAMP: Ty = Ref("Timestamp");
const STRS: Ty = List(&Str);

const fn req(name: &'static str, ty: Ty) -> Field {
    Field { name, ty, optional: false, doc: "" }
}
const fn opt(name: &'static str, ty: Ty) -> Field {
    Field { name, ty, optional: true, doc: "" }
}
const fn var(tag: &'static str, fields: &'static [Field]) -> Variant {
    Variant { tag, fields }
}
const fn alias(name: &'static str, ty: Ty, doc: &'static str) -> Def {
    Def { name, doc, shape: Shape::Alias(ty) }
}
const fn strs(name: &'static str, values: &'static [&'static str], doc: &'static str) -> Def {
    Def { name, doc, shape: Shape::Enum(values) }
}
const fn obj(name: &'static str, fields: &'static [Field], doc: &'static str) -> Def {
    Def { name, doc, shape: Shape::Object(fields) }
}
const fn union(name: &'static str, tag: &'static str, variants: &'static [Variant], doc: &'static str) -> Def {
    Def { name, doc, shape: Shape::Union { tag, variants } }
}
const fn method(name: &'static str, sdk: &'static str, params: Option<&'static str>, result: Ty, doc: &'static str) -> Method {
    Method { name, sdk, params, result, doc }
}

/// `aho-log::EntryKind`, tag `type` (06).
const ENTRY_KINDS: &[Variant] = &[
    var("config_resolved", &[req("key", Str), req("sourced", Ref("Sourced"))]),
    var("cap_activated", &[req("id", Str), req("manifest_hash", HASH), req("deps", STRS), req("config_hash", HASH)]),
    var("plan_sealed", &[req("plan_hash", HASH), req("fingerprint", HASH), req("schema_hash", HASH)]),
    var("cap_delta", &[req("id", Str), req("op", Ref("CapDeltaOp")), req("reason", Str)]),
    var("loadout_diagnostic", &[req("lint", Str), req("severity", Str), req("key", Str), req("detail", Str)]),
    var(
        "policy_epoch",
        &[req("epoch", Ref("Epoch")), req("allowed", Ref("ConsequenceSet")), req("preset", Str), req("approval_ref", Opt(&ENTRY_ID))],
    ),
    var("approval_requested", &[req("wants", Ref("ConsequenceSet")), req("for_cap", Str), req("call", Opt(&CALL_ID)), req("reason", Str)]),
    var("approval_decided", &[req("req", ENTRY_ID), req("outcome", Ref("ApprovalOutcome")), req("scope", Ref("GrantScope"))]),
    var("lease_granted", &[req("lease", Ref("LeaseId")), req("subject", Str), req("expires", TIMESTAMP)]),
    var("lease_revoked", &[req("lease", Ref("LeaseId")), req("proof_next_attempt", Opt(&ENTRY_ID))]),
    var("turn_start", &[req("turn", Ref("TurnId"))]),
    var("turn_end", &[req("turn", Ref("TurnId"))]),
    var("step_start", &[req("step", Ref("StepId"))]),
    var("step_end", &[req("step", Ref("StepId")), req("transition", Ref("Transition"))]),
    var("system_message", &[req("sections", List(&Tuple(&[Str, HASH]))), req("cache_break_at", UInt)]),
    var("user_message", &[req("form", Ref("Form")), req("text", Str), req("client_key", Opt(&Str))]),
    var("assistant_message", &[req("content", List(&Ref("ContentBlock"))), req("usage", Ref("Usage")), req("stop", Ref("StopReason"))]),
    var("assistant_attempt", &[req("error", Str)]),
    var(
        "request_header",
        &[
            req("model", Str),
            req("tools_visible", STRS),
            req("policy_epoch", Ref("Epoch")),
            req("plan_hash", HASH),
            req("section_hashes", List(&HASH)),
            req("open_calls", List(&CALL_ID)),
            req("request_hash", HASH),
        ],
    ),
    var("tool_call", &[req("call", CALL_ID), req("tool", Str), req("args", Json), req("class", Ref("Consequence"))]),
    var(
        "tool_result",
        &[
            req("call", CALL_ID),
            req("outcome", Ref("ToolOutcome")),
            req("value", Json).doc("Canonical, schema-validated value (10); SDK callers read this."),
            req("content", List(&Ref("ContentBlock"))).doc("What the model saw."),
            req("spill_ref", Opt(&HASH)),
            req("meta", Opt(&Ref("Card"))),
            req("late", Bool).doc("Arrived after the request boundary it belonged to (16)."),
        ],
    ),
    var(
        "op_started",
        &[req("op", Ref("OpId")), req("call", CALL_ID), req("kind", Ref("OpKind")), req("rerunnable", Bool), req("external_effect", Bool)],
    ),
    var("op_done", &[req("op", Ref("OpId")), req("status", Ref("OpStatus"))]),
    var(
        "surface_replace",
        &[req("start", ENTRY_ID), req("end", ENTRY_ID), req("shadow_price", UInt), req("summary", Opt(&Str)), req("by_cap", Str)],
    ),
    var("compaction_start", &[req("id", UInt)]),
    var("compaction_end", &[req("id", UInt), req("error", Opt(&Str))]),
    var("compaction_failed", &[req("consecutive", UInt)]),
    var("hook_invoked", &[req("point", Str), req("cap", Str), req("matcher", Str)]),
    var("hook_result", &[req("decision", Str), req("exit", Opt(&Int)), req("stderr_summary", Str), req("ms", UInt)]),
    var("command_run", &[req("name", Str), req("args", STRS), req("key", Opt(&Str))]),
    var("command_done", &[req("run", ENTRY_ID), req("kind", Str)]),
    var("surface_attached", &[req("client_instance", Str), req("has_local_history", Bool), req("takeover", Ref("Takeover"))]),
    var("session_recovery_intent", &[req("role", Ref("RecoveryRole")), req("directive", Str)]),
    var("session_seen", &[]),
    var(
        "session_meta",
        &[req("title", Opt(&Str)), req("tags", STRS), req("last_prompt", Opt(&Str)), req("process", Opt(&Ref("ProcessIdentity")))],
    ),
    var("subagent_started", &[req("child", Ref("SessionId")), req("provider", Str), req("depth", UInt)]),
    var("subagent_ended", &[req("child", Ref("SessionId")), req("stop", Ref("StopReason")), req("last_message", Opt(&Str))]),
    var(
        "subagent_descriptor",
        &[req("provider", Str), req("persona", Opt(&Str)), req("tool_filter", STRS), req("parent", Ref("SessionId")), req("depth", UInt)],
    ),
    var(
        "workspace_checkpoint",
        &[
            req("id", Ref("CheckpointId")),
            req("tree", HASH),
            req("parent", Opt(&Ref("CheckpointId"))),
            req("cause", Ref("CheckpointCause")),
        ],
    ),
    var(
        "workspace_restored",
        &[
            req("from", Ref("CheckpointId")),
            req("to", Ref("CheckpointId")),
            req("mode", Ref("RewindMode")),
            req("changed", STRS),
            req("deleted", STRS),
        ],
    ),
    var("invariant_registered", &[req("id", Str), req("kind", Ref("FormalKind")), req("statement", Str), req("covers", STRS)]),
    var("invariant_checked", &[req("id", Str), req("mode", Ref("CheckMode")), req("verdict", Ref("Verdict")), req("code_hash", HASH)]),
    var("invariant_mutated", &[req("id", Str), req("mutant", Str), req("caught", Bool)]),
    var("custom", &[req("ns", Str), req("name", Str), req("body", Json)]),
    var("note", &[req("text", Str)]),
];

const TRANSITIONS: &[Variant] = &[
    var("completed", &[]),
    var("tool_use", &[]),
    var("waiting_on_ops", &[]),
    var("reactive_compact_retry", &[]),
    var("max_output_recovery", &[]),
    var("stop_hook_blocking", &[]),
    var("queued_command", &[]),
    var("budget_continue", &[]),
    var("prompt_too_long", &[]),
    var("max_turns", &[]),
    var("reload", &[]),
    var("aborted", &[req("reason", Str)]),
];

pub static DEFS: &[Def] = &[
    // ── scalars and ids (aho-types) ──
    alias("Hash", Str, "blake3 digest as 64 lowercase hex characters."),
    alias("EntryId", UInt, "Log entry id, monotonic per log (06); resubscribe `from` the last one seen."),
    alias("TurnId", UInt, "Turn number within a session."),
    alias("StepId", UInt, "Model request number within a session."),
    alias("Epoch", UInt, "Policy epoch; every widening of the policy starts a new one (04)."),
    alias("OpId", UInt, "Operation behind a tool call (16)."),
    alias("LeaseId", UInt, "Lease on a leased resource (04)."),
    alias("CheckpointId", UInt, "Workspace checkpoint (11)."),
    alias("Timestamp", UInt, "Milliseconds since the Unix epoch."),
    alias("SessionId", Str, "u128 session id as a decimal string (`hello_ok.session` is the same id in lowercase hex)."),
    alias("AgentId", Str, "u128 agent id as a decimal string."),
    alias("CallId", Str, "Tool call id as issued by the model."),
    strs(
        "Consequence",
        &[
            "read_only",
            "workspace_write",
            "host_write",
            "network_egress",
            "spawns_process",
            "controls_vm",
            "controls_desktop",
            "talks_to_human",
        ],
        "What a tool can do (02); every tool declares one class.",
    ),
    alias("ConsequenceSet", List(&Ref("Consequence")), "A set of consequence classes, in class order."),
    union(
        "Principal",
        "kind",
        &[
            var("operator", &[]),
            var("engineer", &[]),
            var("agent", &[req("id", Ref("AgentId")), req("turn", Ref("TurnId"))]),
            var("system", &[req("name", Str)]),
        ],
        "Who caused an entry (04).",
    ),
    union(
        "ContentBlock",
        "type",
        &[
            var("text", &[req("text", Str)]),
            var("image", &[req("media_type", Str), req("data", List(&UInt)).doc("The image bytes, one number (0-255) per byte.")]),
            var("tool_use", &[req("id", CALL_ID), req("name", Str), req("args", Json)]),
            var("tool_result", &[req("id", CALL_ID), req("content", List(&Ref("ContentBlock"))), req("is_error", Bool)]),
            var("thinking", &[req("text", Str)]),
            var("provider_metadata", &[req("protocol", Str), req("data", Json)]),
        ],
        "One block of a model-facing message.",
    ),
    obj(
        "Usage",
        &[req("input_tokens", UInt), req("output_tokens", UInt), req("cache_read_tokens", UInt), req("cache_write_tokens", UInt)],
        "Token counts.",
    ),
    strs("StopReason", &["end_turn", "tool_use", "max_tokens", "refusal", "aborted", "error"], "Why the model stopped."),
    strs(
        "Form",
        &["prompt", "snapshot", "catalog", "instructions", "notice"],
        "Source label of injected context (10); surfaces read this, never prose.",
    ),
    union(
        "Card",
        "card",
        &[
            var("generic", &[req("title", Str), req("body", Str)]),
            var("terminal", &[req("command", Str), req("output", Str), req("exit", Opt(&Int))]),
            var("diff", &[req("path", Str), req("hunks", STRS)]),
            var("search", &[req("query", Str), req("hits", STRS)]),
            var("read", &[req("path", Str), req("range", Opt(&Tuple(&[UInt, UInt])))]),
            var("web", &[req("url", Str), req("title", Str)]),
        ],
        "Presentation intent for a tool result (10).",
    ),
    // ── config provenance (aho-config, 03) ──
    union(
        "Layer",
        "layer",
        &[
            var("default", &[]),
            var("bundle", &[req("name", Str)]),
            var("profile", &[req("name", Str)]),
            var("file_home", &[]),
            var("file_project", &[]),
            var("env", &[]),
            var("arg", &[]),
            var("runtime", &[req("name", Str)]),
        ],
        "Config layer, lowest rank first (03).",
    ),
    strs("Scope", &["machine", "user", "project", "invocation", "session"], "Config breadth (03)."),
    union(
        "Where",
        "at",
        &[
            var("const", &[]),
            var("file", &[req("path", Str), req("line", UInt)]),
            var("env", &[req("var", Str)]),
            var("flag", &[req("flag", Str)]),
            var("entry", &[req("id", ENTRY_ID)]),
        ],
        "Where a config value was written.",
    ),
    obj("Origin", &[req("layer", Ref("Layer")), req("scope", Ref("Scope")), req("at", Ref("Where"))], "Where a config value came from."),
    obj(
        "Sourced",
        &[
            req("value", Json),
            req("winner", Ref("Origin")),
            req("shadowed", List(&Tuple(&[Ref("Origin"), Json]))).doc("Every loser, most specific first."),
        ],
        "A resolved config value with its winning origin (03).",
    ),
    // ── log (aho-log, 06) ──
    union(
        "CapDeltaOp",
        "op",
        &[
            var("activated", &[]),
            var("deactivated", &[]),
            var("broken", &[req("err", Str)]),
            var("replaced", &[req("by", Str)]),
            var("rejected", &[req("reference", ENTRY_ID)]),
        ],
        "A capability state change after bootstrap.",
    ),
    strs("ApprovalOutcome", &["allowed_once", "rejected", "cancelled", "unavailable"], "How an approval request ended."),
    strs("GrantScope", &["once", "session", "until_revoked"], "How long an allow holds."),
    union("Transition", "transition", TRANSITIONS, "How a step or turn ended (10)."),
    union(
        "ToolOutcome",
        "outcome",
        &[var("ok", &[]), var("err", &[req("code", Str), req("reason", Str)]), var("cancelled", &[req("reason", Ref("CancelReason"))])],
        "How a tool call ended.",
    ),
    strs(
        "CancelReason",
        &["aborted", "before_dispatch", "sibling_failed", "user_interrupted", "rewind", "reload"],
        "Why a call or operation was cancelled.",
    ),
    strs("OpKind", &["process", "io", "timer", "remote", "compute"], "What an operation waits on (16)."),
    union(
        "OpStatus",
        "status",
        &[
            var("completed", &[req("code", Int)]),
            var("failed", &[req("err", Str)]),
            var("cancelled", &[req("reason", Ref("CancelReason"))]),
            var("uncertain", &[req("last_seen", TIMESTAMP)]),
        ],
        "How an operation ended; `uncertain` = the process died mid-dispatch and it may or may not have happened (16).",
    ),
    obj(
        "ProcessIdentity",
        &[req("pid", UInt), req("start_time", TIMESTAMP)],
        "Crash-honest process identity: pids recycle, `(pid, start_time)` does not.",
    ),
    union(
        "Takeover",
        "takeover",
        &[var("allowed", &[]), var("refused", &[req("reason", Str)])],
        "Whether an attaching surface took the session (12).",
    ),
    strs("RecoveryRole", &["initiator", "interrupted_peer", "headless"], "Role in a crash recovery (12)."),
    union(
        "CheckpointCause",
        "cause",
        &[var("tool_result", &[req("call", CALL_ID)]), var("turn_end", &[]), var("manual", &[]), var("pre_rewind", &[])],
        "Why a workspace checkpoint was taken (11).",
    ),
    strs("RewindMode", &["both", "chat_only", "code_only"], "What a rewind restored (11)."),
    strs("FormalKind", &["tla", "lean"], "Invariant language (13)."),
    union(
        "CheckMode",
        "mode",
        &[var("bounded", &[req("n", UInt)]), var("inductive", &[]), var("proof", &[]), var("trace", &[])],
        "How an invariant was checked (13).",
    ),
    union(
        "Verdict",
        "verdict",
        &[
            var("pass", &[]),
            var("fail", &[req("counterexample", HASH)]),
            var("unknown", &[req("reason", Str)]),
            var("stale", &[]),
            var("vacuous", &[]),
        ],
        "An invariant check result (13).",
    ),
    union(
        "CapState",
        "state",
        &[
            var("active", &[]),
            var("disabled", &[req("why", Str)]),
            var("locked", &[req("missing", STRS)]),
            var("broken", &[req("err", Str)]),
            var("unlocked", &[]),
        ],
        "A capability's state in the fold (02, 05).",
    ),
    obj(
        "PolicyState",
        &[req("epoch", Ref("Epoch")), req("allowed", Ref("ConsequenceSet")), req("preset", Str)],
        "The current policy epoch (04).",
    ),
    obj(
        "Entry",
        &[
            req("id", ENTRY_ID),
            req("parent", Opt(&ENTRY_ID)).doc("A branch is a leaf pointing at an older parent; `null` only for the first entry."),
            req("prev_hash", HASH),
            req("hash", HASH).doc("`chain(prev_hash, body)` over the branch."),
            req("at", TIMESTAMP),
            req("by", Ref("Principal")),
            req("kind", Ref("EntryKind")),
        ],
        "One session log entry (06).",
    ),
    union("EntryKind", "type", ENTRY_KINDS, "What an entry records: closed core kinds plus namespaced `custom` (06)."),
    // ── envelope and handshake (08, 12) ──
    obj(
        "ClientFrame",
        &[req("v", UInt), opt("id", Opt(&UInt)).doc("Absent = notification: no reply."), req("method", Str), opt("params", Json)],
        "Client → server. Unknown fields are ignored.",
    ),
    obj(
        "ServerFrame",
        &[
            req("v", UInt),
            req("reply_to", Opt(&UInt)).doc("The request a `hello_ok`/`result`/`error` answers; `null` on events."),
            req("event", Str),
            req("payload", Json),
        ],
        "Server → client. Unknown fields are ignored.",
    ),
    obj(
        "Hello",
        &[
            req("min", UInt),
            req("max", UInt),
            opt("client_instance", Str).doc("Stable per client install; the owner of a session is a client instance (12)."),
            opt("has_local_history", Bool),
            opt("allow_takeover", Bool).doc("Take the session from its owner; needs `has_local_history`."),
        ],
        "The first client frame's params: the protocol versions the client speaks.",
    ),
    obj(
        "HelloOk",
        &[
            req("version", UInt).doc("The highest version both sides speak; every later frame carries it as `v`."),
            req("capabilities", STRS).doc("Every method this server answers."),
            req("session", Str).doc("Session id, lowercase hex."),
            req("head", Opt(&ENTRY_ID)).doc("Last entry on the log; resubscribe `from` here to catch up."),
        ],
        "The server's answer to `hello`.",
    ),
    obj("ErrorPayload", &[req("message", Str)], "What happened, what the server did, how to change it."),
    obj("Empty", &[], "No fields."),
    // ── session.* params and results ──
    obj(
        "PromptParams",
        &[
            req("text", Str),
            opt("client_key", Opt(&Str)).doc("Caller-chosen dedup key: a prompt whose key the log has seen is not run again (16)."),
        ],
        "`session.prompt` params.",
    ),
    obj("PromptResult", &[req("transition", Ref("Transition"))], "The turn's final transition."),
    obj("SteerParams", &[req("text", Str), opt("urgent", Bool)], "`session.steer` params."),
    obj(
        "SubscribeParams",
        &[opt("from", Opt(&ENTRY_ID)).doc("Replay entries after this id; absent or `null` = from the first entry.")],
        "`session.subscribe` params.",
    ),
    obj(
        "SubscribeResult",
        &[req("replayed", UInt).doc("Entries replayed before this reply."), req("head", Opt(&ENTRY_ID))],
        "`session.subscribe` result.",
    ),
    obj("Checkpoint", &[req("id", Ref("CheckpointId")), req("tree", HASH), req("entry", ENTRY_ID)], "A workspace checkpoint in the fold."),
    obj("InvariantState", &[req("verdict", Ref("Verdict")), req("code_hash", HASH)], "An invariant's latest verdict (13)."),
    obj(
        "Fold",
        &[
            req("head", Opt(&ENTRY_ID)),
            req("turns", UInt),
            req("turn", Opt(&Ref("TurnId"))),
            req("plan_hash", Opt(&HASH)),
            req("policy", Ref("PolicyState")),
            req("caps", Map(&Ref("CapState"))),
            req("config", Map(&Json)).doc("Resolved value per config key."),
            req("open_calls", List(&CALL_ID)),
            req("pending_approvals", List(&ENTRY_ID)),
            req("checkpoints", List(&Ref("Checkpoint"))),
            req("usage", Ref("Usage")),
            req("last_transition", Opt(&Ref("Transition"))),
            req("invariants", Map(&Ref("InvariantState"))),
            req("custom", Map(&Json)).doc("Capability-owned projections, keyed by namespace."),
        ],
        "`session.fold`: the folded state a thin client reduces over (surface-local state excluded).",
    ),
    strs("RewindModeName", &["both", "chat", "code", "chat_only", "code_only"], "What `session.rewind` restores; default `both`."),
    obj("RewindParams", &[req("checkpoint", Ref("CheckpointId")), opt("mode", Opt(&Ref("RewindModeName")))], "`session.rewind` params."),
    obj("RewindResult", &[req("entry", ENTRY_ID).doc("The entry the rewound branch continues from.")], "`session.rewind` result."),
    // ── approvals (04, 05) ──
    strs(
        "AnswerOutcome",
        &["allow", "allow_once", "allow_session", "reject", "cancel", "preset"],
        "`preset` writes the card's `preset_cut` as `policy.preset` (plan → apply) and allows the call for the session.",
    ),
    obj(
        "AnswerParams",
        &[
            req("id", UInt).doc("The card's `id`."),
            req("outcome", Ref("AnswerOutcome")),
            opt("scope", Opt(&Ref("GrantScope"))).doc("Only an allow carries a scope wider than `once`."),
        ],
        "`approval.answer` params.",
    ),
    obj(
        "AnswerResult",
        &[opt("preset", Str), opt("wrote", STRS).doc("One operator-facing line per written key.")],
        "`approval.answer` result: empty, or the preset written for a `preset` answer.",
    ),
    obj(
        "ApprovalCard",
        &[
            req("id", UInt).doc("Answer with this id."),
            req("tool", Str),
            req("cap", Opt(&Str)).doc("The capability that registered the tool."),
            req("class", Ref("Consequence")),
            req("args", Json),
            req("reason", Str).doc("Which link of the decision chain asked (`Rule`, `Preset`, …)."),
            req("why", Str).doc("The rendered card text."),
            req("world", Str),
            req("enforced_by", Str),
            req("suggested_cut", Opt(&Str)).doc("Smallest lattice change that would let this call pass."),
            req("preset_cut", Opt(&Str)).doc("The preset the `preset` answer writes; `null` = no preset admits the call."),
            req("epoch_next", Ref("Epoch")),
        ],
        "A tool call waiting for a human (05): the same fields every surface renders.",
    ),
    // ── events ──
    union(
        "StreamPayload",
        "kind",
        &[
            var("text", &[req("text", Str)]),
            var("thinking", &[req("text", Str)]),
            var("tool_use_start", &[req("id", CALL_ID), req("name", Str)]),
            var("tool_use_end", &[req("id", CALL_ID), req("name", Str), req("args", Json)]),
            var("usage", &[req("usage", Ref("Usage"))]),
        ],
        "A live model stream chunk; never durable (the log has the final message).",
    ),
    obj("Idle", &[req("turn", Ref("TurnId")), req("transition", Ref("Transition"))], "The session went idle."),
    // ── caps.* (05) ──
    obj("CapsListParams", &[opt("kind", Str).doc("Only rows of this kind (`tool`, `provider`, …).")], "`caps.list` params."),
    obj("CapsTreeParams", &[opt("id", Str).doc("One node instead of the whole tree.")], "`caps.tree` params."),
    obj("CapsWhyParams", &[req("id", Str)], "`caps.why` params."),
    obj(
        "CapsExplainParams",
        &[req("key", Str).doc("A config key or a capability id."), opt("at", ENTRY_ID).doc("Explain as of this entry of the session.")],
        "`caps.explain` params.",
    ),
    obj(
        "CapsPlanParams",
        &[opt("patch", Str).doc("TOML layer to plan as if it were a `--set` layer."), opt("patch_path", Str)],
        "`caps.plan` params.",
    ),
    obj(
        "CapsSetParams",
        &[req("key", Str), req("value", Json), opt("apply", Bool).doc("Write the change; without it the reply is only the plan.")],
        "`caps.set` params.",
    ),
    obj("CapsSwitchParams", &[req("id", Str), opt("apply", Bool)], "`caps.enable` / `caps.disable` / `caps.allow` params."),
    obj("CapsPresetParams", &[req("name", Str), opt("apply", Bool)], "`caps.preset` params."),
    obj(
        "Proposal",
        &[
            req("file", Str).doc("The layer file the change lands in."),
            req("changes", List(&Tuple(&[Str, Json]))),
            req("diff", Json).doc("The plan diff (aho-compose `PlanDiff`)."),
            req("rendered", Str).doc("The block `aho` prints before `apply? [y/N]`."),
            req("plan_hash", HASH).doc("The plan after the change."),
            req("applied", Bool),
            req("wrote", Opt(&STRS)),
        ],
        "A planned (and with `apply`, written) configuration change (05: plan → diff → apply).",
    ),
];

pub static METHODS: &[Method] = &[
    method(
        "session.prompt",
        "prompt",
        Some("PromptParams"),
        Ref("PromptResult"),
        "Runs one turn to idle and replies with its final transition; entries, stream chunks, and approval cards arrive as events meanwhile.",
    ),
    method(
        "session.steer",
        "steer",
        Some("SteerParams"),
        Ref("Empty"),
        "Queues input for the running turn; injected at the next request boundary.",
    ),
    method("session.abort", "abort", None, Ref("Empty"), "Hard interrupt: cancels the running turn's operations."),
    method(
        "session.subscribe",
        "subscribe",
        Some("SubscribeParams"),
        Ref("SubscribeResult"),
        "Replays entries after `from` as `entry` events, then streams live events on this connection.",
    ),
    method("session.fold", "fold", None, Ref("Fold"), "The folded session state."),
    method("session.rewind", "rewind", Some("RewindParams"), Ref("RewindResult"), "Rewinds chat and/or code to a checkpoint (11)."),
    method(
        "approval.answer",
        "answer_approval",
        Some("AnswerParams"),
        Ref("AnswerResult"),
        "Answers one approval card; a card is answered once.",
    ),
    method("caps.list", "caps_list", Some("CapsListParams"), Json, "Inventory rows (05); shape owned by aho-inspect."),
    method("caps.tree", "caps_tree", Some("CapsTreeParams"), Json, "The capability tree (05); shape owned by aho-inspect."),
    method(
        "caps.explain",
        "caps_explain",
        Some("CapsExplainParams"),
        Json,
        "Where a setting's value comes from, or why a capability is in its state (05); shape owned by aho-inspect.",
    ),
    method("caps.why", "caps_why", Some("CapsWhyParams"), Json, "Why a capability is in its state and how to change it (05)."),
    method("caps.plan", "caps_plan", Some("CapsPlanParams"), Json, "The resolved plan, or the diff a patch layer would make (05)."),
    method("caps.set", "caps_set", Some("CapsSetParams"), Ref("Proposal"), "Plans one setting; `apply` writes it."),
    method("caps.enable", "caps_enable", Some("CapsSwitchParams"), Ref("Proposal"), "Plans `caps.<id>.enabled = true`; `apply` writes it."),
    method(
        "caps.disable",
        "caps_disable",
        Some("CapsSwitchParams"),
        Ref("Proposal"),
        "Plans `caps.<id>.enabled = false`; `apply` writes it.",
    ),
    method(
        "caps.allow",
        "caps_allow",
        Some("CapsSwitchParams"),
        Ref("Proposal"),
        "Plans `caps.<id>.allow = true` (past the preset); `apply` writes it.",
    ),
    method("caps.preset", "caps_preset", Some("CapsPresetParams"), Ref("Proposal"), "Plans `policy.preset = <name>`; `apply` writes it."),
];

pub static EVENTS: &[Event] = &[
    Event { name: "entry", payload: Ref("Entry"), doc: "A log entry: replayed after `session.subscribe`, then live." },
    Event { name: "stream", payload: Ref("StreamPayload"), doc: "A live model stream chunk." },
    Event { name: "idle", payload: Ref("Idle"), doc: "The session went idle after a turn." },
    Event { name: "approval", payload: Ref("ApprovalCard"), doc: "A tool call waits for a human; answer it with `approval.answer`." },
    Event {
        name: "error",
        payload: Ref("ErrorPayload"),
        doc: "A failure outside any request (a lagging client, a log write that did not land).",
    },
];

/// The protocol this build of surface-rpc speaks.
pub static PROTOCOL: Protocol =
    Protocol { version_min: surface_rpc::VERSION_MIN, version_max: surface_rpc::VERSION_MAX, defs: DEFS, methods: METHODS, events: EVENTS };

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(t: &Ty, out: &mut Vec<&'static str>) {
        match *t {
            Ty::Ref(n) => out.push(n),
            Ty::Opt(t) | Ty::List(t) | Ty::Map(t) => refs(t, out),
            Ty::Tuple(ts) => ts.iter().for_each(|t| refs(t, out)),
            _ => {}
        }
    }

    fn def_refs(d: &Def) -> Vec<&'static str> {
        let mut out = Vec::new();
        match d.shape {
            Shape::Alias(t) => refs(&t, &mut out),
            Shape::Enum(_) => {}
            Shape::Object(fs) => fs.iter().for_each(|f| refs(&f.ty, &mut out)),
            Shape::Union { variants, .. } => variants.iter().flat_map(|v| v.fields).for_each(|f| refs(&f.ty, &mut out)),
        }
        out
    }

    #[test]
    fn every_reference_resolves_and_names_are_unique() {
        let mut names: Vec<&str> = PROTOCOL.defs.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate def names");
        let mut used = Vec::new();
        PROTOCOL.defs.iter().for_each(|d| used.extend(def_refs(d)));
        for m in PROTOCOL.methods {
            used.extend(m.params);
            refs(&m.result, &mut used);
        }
        PROTOCOL.events.iter().for_each(|e| refs(&e.payload, &mut used));
        for n in &used {
            assert!(PROTOCOL.def(n).is_some(), "`{n}` is referenced but not defined");
        }
    }

    #[test]
    fn union_tags_are_unique_and_fields_never_shadow_the_tag() {
        for d in PROTOCOL.defs {
            if let Shape::Union { tag, variants } = d.shape {
                let mut tags: Vec<&str> = variants.iter().map(|v| v.tag).collect();
                tags.sort_unstable();
                tags.dedup();
                assert_eq!(tags.len(), variants.len(), "{}: duplicate variant tags", d.name);
                for v in variants {
                    assert!(v.fields.iter().all(|f| f.name != tag), "{}.{}: a field shadows the tag `{tag}`", d.name, v.tag);
                }
            }
        }
    }
}
