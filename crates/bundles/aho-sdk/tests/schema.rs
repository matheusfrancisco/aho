//! Holds the hand-maintained protocol table (`aho_sdk::protocol`) to what surface-rpc
//! sends and accepts. surface-rpc builds its payloads with `json!` and the log types carry no
//! schema derives, so this is the check that makes the generated SDKs trustworthy:
//!
//! * surface-rpc's committed wire snapshot validates strictly (no unknown or missing field);
//! * one sample of every entry kind, transition, stream chunk, and nested variant, serialized by
//!   the server's own functions, validates, and together they exercise every union variant and
//!   enum value the table lists (a table row nothing produces is as wrong as a missing one);
//! * the frames of a live session (every method called once) validate;
//! * the params structs surface-rpc decodes agree with the table on names and optionality;
//! * the Rust SDK's typed events decode those frames without loss.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use aho_cap::BoundWorld;
use aho_config::{Layer, Origin, Sourced, Where};
use aho_llm::StreamEvent;
use aho_log::{
    ApprovalOutcome, CancelReason, CapDeltaOp, CapState, CheckMode, CheckpointCause, Entry, EntryKind, FormalKind, GrantScope, OpKind,
    OpStatus, PolicyState, ProcessIdentity, RecoveryRole, RewindMode, State, Takeover, ToolOutcome, Transition, Verdict,
};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_sdk::protocol::{PROTOCOL, Shape, Ty};
use aho_sdk::validate::{check, check_client_frame, check_server_frame};
use aho_sdk::{Event, Fold, Open, Sdk};
use aho_types::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use surface_rpc::ServerFrame;
use surface_rpc::wire::{self, AnswerParams, Hello, HelloOk, PromptParams, RewindParams, SteerParams, SubscribeParams};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ───────────────────────────── samples ─────────────────────────────

fn transitions() -> Vec<Transition> {
    use Transition::*;
    let all = vec![
        Completed,
        ToolUse,
        WaitingOnOps,
        ReactiveCompactRetry,
        EmptyStopRetry,
        MaxOutputRecovery,
        StopHookBlocking,
        QueuedCommand,
        BudgetContinue,
        PromptTooLong,
        MaxTurns,
        Reload,
        Aborted("stopped by the operator".into()),
    ];
    // A new variant fails to compile here: add it above and to the table.
    for t in &all {
        match t {
            Completed | ToolUse | WaitingOnOps | ReactiveCompactRetry | EmptyStopRetry | MaxOutputRecovery | StopHookBlocking
            | QueuedCommand | BudgetContinue | PromptTooLong | MaxTurns | Reload | Aborted(_) => {}
        }
    }
    all
}

fn streams() -> Vec<StreamEvent> {
    let all = vec![
        StreamEvent::Text("hel".into()),
        StreamEvent::Thinking("hmm".into()),
        StreamEvent::ToolUseStart { id: CallId("c1".into()), name: "read".into() },
        StreamEvent::ToolUseEnd { id: CallId("c1".into()), name: "read".into(), args: json!({"path": "a.txt"}) },
        StreamEvent::Usage(Usage { input_tokens: 10, output_tokens: 3, cache_read_tokens: 1, cache_write_tokens: 2 }),
    ];
    for s in &all {
        match s {
            StreamEvent::Text(_)
            | StreamEvent::Thinking(_)
            | StreamEvent::ToolUseStart { .. }
            | StreamEvent::ToolUseEnd { .. }
            | StreamEvent::Usage(_) => {}
        }
    }
    all
}

/// Wire tag of every entry kind. A new variant fails to compile here: add a sample and a table row.
fn entry_tag(k: &EntryKind) -> &'static str {
    match k {
        EntryKind::ConfigResolved { .. } => "config_resolved",
        EntryKind::CapActivated { .. } => "cap_activated",
        EntryKind::PlanSealed { .. } => "plan_sealed",
        EntryKind::CapDelta { .. } => "cap_delta",
        EntryKind::LoadoutDiagnostic { .. } => "loadout_diagnostic",
        EntryKind::PolicyEpoch { .. } => "policy_epoch",
        EntryKind::ApprovalRequested { .. } => "approval_requested",
        EntryKind::ApprovalDecided { .. } => "approval_decided",
        EntryKind::LeaseGranted { .. } => "lease_granted",
        EntryKind::LeaseRevoked { .. } => "lease_revoked",
        EntryKind::TurnStart { .. } => "turn_start",
        EntryKind::TurnEnd { .. } => "turn_end",
        EntryKind::StepStart { .. } => "step_start",
        EntryKind::StepEnd { .. } => "step_end",
        EntryKind::SystemMessage { .. } => "system_message",
        EntryKind::UserMessage { .. } => "user_message",
        EntryKind::AssistantMessage { .. } => "assistant_message",
        EntryKind::AssistantAttempt { .. } => "assistant_attempt",
        EntryKind::RequestHeader { .. } => "request_header",
        EntryKind::ToolCall { .. } => "tool_call",
        EntryKind::ToolResult { .. } => "tool_result",
        EntryKind::OpStarted { .. } => "op_started",
        EntryKind::OpDone { .. } => "op_done",
        EntryKind::SurfaceReplace { .. } => "surface_replace",
        EntryKind::CompactionStart { .. } => "compaction_start",
        EntryKind::CompactionEnd { .. } => "compaction_end",
        EntryKind::CompactionFailed { .. } => "compaction_failed",
        EntryKind::HookInvoked { .. } => "hook_invoked",
        EntryKind::HookResult { .. } => "hook_result",
        EntryKind::CommandRun { .. } => "command_run",
        EntryKind::CommandDone { .. } => "command_done",
        EntryKind::SurfaceAttached { .. } => "surface_attached",
        EntryKind::SessionRecoveryIntent { .. } => "session_recovery_intent",
        EntryKind::SessionSeen => "session_seen",
        EntryKind::SessionMeta { .. } => "session_meta",
        EntryKind::SubagentStarted { .. } => "subagent_started",
        EntryKind::SubagentEnded { .. } => "subagent_ended",
        EntryKind::SubagentDescriptor { .. } => "subagent_descriptor",
        EntryKind::WorkspaceCheckpoint { .. } => "workspace_checkpoint",
        EntryKind::WorkspaceRestored { .. } => "workspace_restored",
        EntryKind::InvariantRegistered { .. } => "invariant_registered",
        EntryKind::InvariantChecked { .. } => "invariant_checked",
        EntryKind::InvariantMutated { .. } => "invariant_mutated",
        EntryKind::Custom { .. } => "custom",
        EntryKind::Note { .. } => "note",
    }
}

fn sourced() -> Sourced<Value> {
    let file = |path: &str, line| Where::File { path: path.into(), line };
    Sourced {
        value: json!("workspace"),
        winner: Origin::new(Layer::FileProject, file("aho.toml", 2)),
        shadowed: vec![
            (Origin::new(Layer::Arg, Where::Flag { flag: "--set policy.preset".into() }), json!([1])),
            (Origin::new(Layer::Env, Where::Env { var: "AHO_POLICY_PRESET".into() }), json!(null)),
            (Origin::new(Layer::FileHome, file("~/.aho/config.toml", 1)), json!(true)),
            (Origin::new(Layer::Profile("ci".into()), file("ci.toml", 3)), json!(1)),
            (Origin::new(Layer::Runtime("unlock_request".into()), Where::Entry { id: EntryId(7) }), json!({"a": 1})),
            (Origin::new(Layer::Bundle("aho-base".into()), Where::Const), json!("workspace")),
            (Origin::new(Layer::Catalog("anthropic/claude-sonnet-4-5".into()), Where::Const), json!(64000)),
            (Origin::new(Layer::Default, Where::Const), json!("read-only")),
        ],
    }
}

fn samples() -> Vec<EntryKind> {
    use EntryKind as K;
    let h = Hash::of(b"x");
    let call = || CallId("c1".into());
    let s = |t: &str| t.to_string();
    let mut k = vec![
        K::ConfigResolved { key: s("policy.preset"), sourced: sourced() },
        K::CapActivated { id: s("tool-fs"), manifest_hash: h, deps: vec![s("FsRead")], config_hash: h },
        K::PlanSealed { plan_hash: h, fingerprint: h, schema_hash: h },
        K::LoadoutDiagnostic { lint: s("dead-key"), severity: s("warning"), key: s("x.y"), detail: s("d") },
        K::PolicyEpoch { epoch: Epoch(2), allowed: ConsequenceSet::ALL, preset: s("full"), approval_ref: Some(EntryId(5)) },
        K::PolicyEpoch { epoch: Epoch(1), allowed: ConsequenceSet::EMPTY, preset: s("read-only"), approval_ref: None },
        K::ApprovalRequested {
            wants: ConsequenceSet::of(&[Consequence::NetworkEgress]),
            for_cap: s("tool-web"),
            call: Some(call()),
            reason: s("r"),
        },
        K::ApprovalRequested { wants: ConsequenceSet::EMPTY, for_cap: s("x"), call: None, reason: s("r") },
        K::LeaseGranted { lease: LeaseId(1), subject: s("vm:a"), expires: Timestamp(5) },
        K::LeaseRevoked { lease: LeaseId(1), proof_next_attempt: Some(EntryId(2)) },
        K::LeaseRevoked { lease: LeaseId(1), proof_next_attempt: None },
        K::TurnStart { turn: TurnId(1) },
        K::TurnEnd { turn: TurnId(1) },
        K::StepStart { step: StepId(1) },
        K::SystemMessage { sections: vec![(s("identity"), h)], cache_break_at: 1 },
        K::AssistantMessage {
            content: vec![
                ContentBlock::text("hi"),
                ContentBlock::Image { media_type: s("image/png"), data: vec![0, 255] },
                ContentBlock::ToolUse { id: call(), name: s("read"), args: json!({"path": "a.txt"}) },
                ContentBlock::ToolResult { id: call(), content: vec![ContentBlock::text("nested")], is_error: true },
                ContentBlock::Thinking { text: s("hmm") },
                ContentBlock::ProviderMetadata { protocol: s("pi-messages"), data: json!({"responseId": "response-1"}) },
            ],
            usage: Usage { input_tokens: 1, output_tokens: 2, cache_read_tokens: 3, cache_write_tokens: 4 },
            stop: StopReason::EndTurn,
        },
        K::AssistantAttempt { error: s("rate limited") },
        K::RequestHeader {
            model: s("m"),
            tools_visible: vec![s("read")],
            policy_epoch: Epoch(1),
            plan_hash: h,
            section_hashes: vec![h],
            open_calls: vec![call()],
            request_hash: h,
        },
        K::ToolCall { call: call(), tool: s("read"), args: json!({"path": "a.txt"}), class: Consequence::ReadOnly },
        K::SurfaceReplace { start: EntryId(1), end: EntryId(9), shadow_price: 40, summary: Some(s("sum")), by_cap: s("compaction") },
        K::SurfaceReplace { start: EntryId(1), end: EntryId(9), shadow_price: 0, summary: None, by_cap: s("compaction") },
        K::CompactionStart { id: 1 },
        K::CompactionEnd { id: 1, error: Some(s("e")) },
        K::CompactionEnd { id: 1, error: None },
        K::CompactionFailed { consecutive: 2 },
        K::HookInvoked { point: s("pre_tool"), cap: s("hooks"), matcher: s("*") },
        K::HookResult { decision: s("allow"), exit: Some(-2), stderr_summary: s(""), ms: 4 },
        K::HookResult { decision: s("allow"), exit: None, stderr_summary: s(""), ms: 4 },
        K::CommandRun { name: s("compact"), args: vec![s("--now")], key: Some(s("k")) },
        K::CommandDone { run: EntryId(3), kind: s("done") },
        K::SurfaceAttached { client_instance: s("tui"), has_local_history: true, takeover: Takeover::Allowed },
        K::SurfaceAttached { client_instance: s("web"), has_local_history: false, takeover: Takeover::Refused(s("owned")) },
        K::SessionSeen,
        K::SessionMeta {
            title: Some(s("t")),
            tags: vec![s("a")],
            last_prompt: Some(s("p")),
            process: Some(ProcessIdentity { pid: 42, start_time: Timestamp(9) }),
        },
        K::SessionMeta { title: None, tags: vec![], last_prompt: None, process: None },
        K::SubagentStarted { child: SessionId(u128::MAX), provider: s("inproc"), depth: 1 },
        K::SubagentEnded { child: SessionId(3), stop: StopReason::EndTurn, last_message: Some(s("done")) },
        K::SubagentDescriptor {
            provider: s("inproc"),
            persona: Some(s("rev")),
            tool_filter: vec![s("read")],
            parent: SessionId(1),
            depth: 1,
        },
        K::SubagentDescriptor { provider: s("inproc"), persona: None, tool_filter: vec![], parent: SessionId(1), depth: 1 },
        K::InvariantMutated { id: s("inv"), mutant: s("m1"), caught: true },
        K::Custom { ns: s("Task"), name: s("Seeded"), body: json!({"n": 1}) },
        K::Note { text: s("note") },
    ];
    let ops = [
        CapDeltaOp::Activated,
        CapDeltaOp::Deactivated,
        CapDeltaOp::Broken { err: s("e") },
        CapDeltaOp::Replaced { by: s("x") },
        CapDeltaOp::Rejected { reference: EntryId(3) },
    ];
    k.extend(ops.into_iter().map(|op| K::CapDelta { id: s("tool-bash"), op, reason: s("r") }));
    let decided = [
        (ApprovalOutcome::AllowedOnce, GrantScope::Once),
        (ApprovalOutcome::Rejected, GrantScope::Session),
        (ApprovalOutcome::Cancelled, GrantScope::UntilRevoked),
        (ApprovalOutcome::Unavailable, GrantScope::Once),
    ];
    k.extend(decided.into_iter().map(|(outcome, scope)| K::ApprovalDecided { req: EntryId(9), outcome, scope }));
    k.extend(transitions().into_iter().map(|transition| K::StepEnd { step: StepId(1), transition }));
    let forms = [Form::Prompt, Form::Snapshot, Form::Catalog, Form::Instructions, Form::Notice];
    k.extend(forms.into_iter().enumerate().map(|(i, form)| K::UserMessage {
        form,
        text: s("hi"),
        client_key: (i % 2 == 0).then(|| s("k")),
    }));
    let stops = [StopReason::ToolUse, StopReason::MaxTokens, StopReason::Refusal, StopReason::Aborted, StopReason::Error];
    k.extend(stops.into_iter().map(|stop| K::AssistantMessage { content: vec![], usage: Usage::default(), stop }));
    let cards = [
        Some(Card::Generic { title: s("t"), body: s("b") }),
        Some(Card::Terminal { command: s("ls"), output: s("a"), exit: Some(-1) }),
        Some(Card::Terminal { command: s("ls"), output: s("a"), exit: None }),
        Some(Card::Diff { path: s("a"), hunks: vec![s("@@")] }),
        Some(Card::Search { query: s("q"), hits: vec![s("h")] }),
        Some(Card::Read { path: s("a"), range: Some((1, 9)) }),
        Some(Card::Read { path: s("a"), range: None }),
        Some(Card::Web { url: s("https://example.org"), title: s("t") }),
        None,
    ];
    let outcomes =
        [ToolOutcome::Ok, ToolOutcome::Err { code: s("E"), reason: s("r") }, ToolOutcome::Cancelled { reason: CancelReason::Aborted }];
    k.extend(cards.into_iter().enumerate().map(|(i, meta)| K::ToolResult {
        call: call(),
        outcome: outcomes[i % outcomes.len()].clone(),
        value: json!({"i": i}),
        content: vec![ContentBlock::text("r")],
        spill_ref: (i % 2 == 0).then_some(h),
        meta,
        late: i % 2 == 1,
    }));
    let kinds = [OpKind::Process, OpKind::Io, OpKind::Timer, OpKind::Remote, OpKind::Compute];
    k.extend(kinds.into_iter().enumerate().map(|(i, kind)| K::OpStarted {
        op: OpId(i as u64),
        call: call(),
        kind,
        rerunnable: i % 2 == 0,
        external_effect: i % 2 == 1,
    }));
    let reasons = [
        CancelReason::Aborted,
        CancelReason::BeforeDispatch,
        CancelReason::SiblingFailed,
        CancelReason::UserInterrupted,
        CancelReason::Rewind,
        CancelReason::Reload,
    ];
    let statuses = [OpStatus::Completed { code: -2 }, OpStatus::Failed { err: s("e") }, OpStatus::Uncertain { last_seen: Timestamp(3) }];
    k.extend(
        statuses.into_iter().chain(reasons.map(|reason| OpStatus::Cancelled { reason })).map(|status| K::OpDone { op: OpId(1), status }),
    );
    let roles = [RecoveryRole::Initiator, RecoveryRole::InterruptedPeer, RecoveryRole::Headless];
    k.extend(roles.into_iter().map(|role| K::SessionRecoveryIntent { role, directive: s("resume") }));
    let causes = [CheckpointCause::ToolResult(call()), CheckpointCause::TurnEnd, CheckpointCause::Manual, CheckpointCause::PreRewind];
    k.extend(causes.into_iter().enumerate().map(|(i, cause)| K::WorkspaceCheckpoint {
        id: CheckpointId(i as u64 + 1),
        tree: h,
        parent: (i > 0).then_some(CheckpointId(i as u64)),
        cause,
    }));
    let modes = [RewindMode::Both, RewindMode::ChatOnly, RewindMode::CodeOnly];
    k.extend(modes.into_iter().map(|mode| K::WorkspaceRestored {
        from: CheckpointId(3),
        to: CheckpointId(1),
        mode,
        changed: vec![s("a")],
        deleted: vec![s("b")],
    }));
    k.extend([FormalKind::Tla, FormalKind::Lean].into_iter().map(|kind| K::InvariantRegistered {
        id: s("inv"),
        kind,
        statement: s("[]P"),
        covers: vec![s("src/a.rs")],
    }));
    let checks = [
        (CheckMode::Bounded(3), Verdict::Pass),
        (CheckMode::Inductive, Verdict::Fail { counterexample: h }),
        (CheckMode::Proof, Verdict::Unknown { reason: s("timeout") }),
        (CheckMode::Trace, Verdict::Stale),
        (CheckMode::Proof, Verdict::Vacuous),
    ];
    k.extend(checks.into_iter().map(|(mode, verdict)| K::InvariantChecked { id: s("inv"), mode, verdict, code_hash: h }));
    k
}

fn principals() -> [Principal; 4] {
    [
        Principal::Operator,
        Principal::Engineer,
        Principal::Agent { id: AgentId(u128::MAX - 7), turn: TurnId(2) },
        Principal::system("session"),
    ]
}

fn entries() -> Vec<Entry> {
    samples()
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Entry {
            id: EntryId(i as u64),
            parent: i.checked_sub(1).map(|p| EntryId(p as u64)),
            prev_hash: Hash::of(b"prev"),
            hash: Hash::of(b"this"),
            at: Timestamp(1_700_000_000_000 + i as u64),
            by: principals()[i % 4].clone(),
            kind,
        })
        .collect()
}

fn state() -> State {
    let mut s = State { turns: 3, turn: Some(TurnId(3)), head: Some(EntryId(42)), plan_hash: Some(Hash::of(b"plan")), ..State::default() };
    s.config.insert("policy.preset".into(), sourced());
    let caps = [
        CapState::Active,
        CapState::Disabled { why: "off".into() },
        CapState::Locked { missing: vec!["git".into()] },
        CapState::Broken { err: "e".into() },
        CapState::Unlocked,
    ];
    for (i, c) in caps.into_iter().enumerate() {
        s.caps.insert(format!("cap-{i}"), c);
    }
    s.policy = PolicyState { epoch: Epoch(2), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]), preset: "read-only".into() };
    s.open_calls = vec![CallId("c1".into())];
    s.pending_approvals.insert(EntryId(7), Some(CallId("c1".into())));
    s.checkpoints = vec![(CheckpointId(1), Hash::of(b"tree"), EntryId(9))];
    s.usage = Usage { input_tokens: 5, ..Usage::default() };
    s.last_transition = Some(Transition::Aborted("stop".into()));
    s.invariants.insert("inv".into(), (Verdict::Fail { counterexample: Hash::of(b"cx") }, Hash::of(b"code")));
    s.custom.insert("Task".into(), json!({"open": 1}));
    s
}

fn cards() -> Vec<Value> {
    let card = |cut: Option<&str>| ApprovalCard {
        tool: "write".into(),
        class: Consequence::WorkspaceWrite,
        args: json!({"path": "y.txt"}),
        reason: DecisionReason::Preset("read-only".into()),
        world: BoundWorld::host(),
        suggested_cut: cut.map(str::to_string),
        rendered: "`write` is outside preset `read-only`; the user is asked before it runs.".into(),
    };
    vec![wire::approval_payload(1, &card(Some("preset workspace")), Some("tool-fs"), 3), wire::approval_payload(2, &card(None), None, 1)]
}

/// Every server payload the samples produce, as (event, payload).
fn server_payloads() -> Vec<(&'static str, Value)> {
    let mut out: Vec<(&str, Value)> = entries().iter().map(|e| ("entry", ServerFrame::entry(1, e).payload)).collect();
    out.extend(streams().iter().map(|s| ("stream", wire::stream_payload(s))));
    out.extend(transitions().iter().map(|t| ("idle", wire::idle_payload(TurnId(1), t))));
    out.extend(cards().into_iter().map(|c| ("approval", c)));
    out
}

// ───────────────────────────── coverage walk ─────────────────────────────

type Seen = BTreeSet<(&'static str, String)>;

/// Records every (union, tag) and (enum, value) a value exercises, walking it by the table.
fn observe(ty: &Ty, v: &Value, seen: &mut Seen) {
    match *ty {
        Ty::Opt(t) if !v.is_null() => observe(t, v, seen),
        Ty::List(t) => v.as_array().into_iter().flatten().for_each(|x| observe(t, x, seen)),
        Ty::Map(t) => v.as_object().into_iter().flat_map(|o| o.values()).for_each(|x| observe(t, x, seen)),
        Ty::Tuple(ts) => ts.iter().zip(v.as_array().into_iter().flatten()).for_each(|(t, x)| observe(t, x, seen)),
        Ty::Ref(n) => match PROTOCOL.def(n).unwrap().shape {
            Shape::Alias(t) => observe(&t, v, seen),
            Shape::Enum(_) => {
                seen.insert((n, v.as_str().unwrap_or_default().to_string()));
            }
            Shape::Object(fs) => fs.iter().for_each(|f| v.get(f.name).into_iter().for_each(|x| observe(&f.ty, x, seen))),
            Shape::Union { tag, variants } => {
                let t = v[tag].as_str().unwrap_or_default();
                seen.insert((n, t.to_string()));
                for f in variants.iter().filter(|x| x.tag == t).flat_map(|x| x.fields) {
                    v.get(f.name).into_iter().for_each(|x| observe(&f.ty, x, seen));
                }
            }
        },
        _ => {}
    }
}

/// Union and enum defs reachable from `root`.
fn reachable(root: &'static str, out: &mut BTreeSet<&'static str>) {
    fn refs(t: &Ty, out: &mut Vec<&'static str>) {
        match *t {
            Ty::Ref(n) => out.push(n),
            Ty::Opt(t) | Ty::List(t) | Ty::Map(t) => refs(t, out),
            Ty::Tuple(ts) => ts.iter().for_each(|t| refs(t, out)),
            _ => {}
        }
    }
    if !out.insert(root) {
        return;
    }
    let mut next = Vec::new();
    match PROTOCOL.def(root).unwrap().shape {
        Shape::Alias(t) => refs(&t, &mut next),
        Shape::Enum(_) => {}
        Shape::Object(fs) => fs.iter().for_each(|f| refs(&f.ty, &mut next)),
        Shape::Union { variants, .. } => variants.iter().flat_map(|v| v.fields).for_each(|f| refs(&f.ty, &mut next)),
    }
    next.into_iter().for_each(|n| reachable(n, out));
}

fn event_ty(event: &str) -> Ty {
    PROTOCOL.event(event).unwrap().payload
}

// ───────────────────────────── tests ─────────────────────────────

#[test]
fn the_method_table_is_the_servers_method_set_in_order() {
    let ours: Vec<&str> = PROTOCOL.methods.iter().map(|m| m.name).collect();
    assert_eq!(ours, surface_rpc::METHODS);
    assert_eq!((PROTOCOL.version_min, PROTOCOL.version_max), (surface_rpc::VERSION_MIN, surface_rpc::VERSION_MAX));
}

#[test]
fn every_frame_of_the_committed_wire_snapshot_validates() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../surfaces/surface-rpc/tests/fixtures/frames.json");
    let snapshot: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let client = snapshot["client"].as_object().unwrap();
    for m in surface_rpc::METHODS.iter().chain(&["hello"]) {
        assert!(client.contains_key(*m), "the snapshot has no `{m}` frame");
    }
    for (name, frame) in client {
        check_client_frame(&PROTOCOL, frame).unwrap_or_else(|e| panic!("client frame `{name}`: {e}"));
    }
    for (name, frame) in snapshot["server"].as_object().unwrap() {
        let method = match name.as_str() {
            "result" => Some("session.prompt"),
            "fold" => Some("session.fold"),
            _ => None,
        };
        check_server_frame(&PROTOCOL, frame, method).unwrap_or_else(|e| panic!("server frame `{name}`: {e}"));
    }
}

#[test]
fn every_entry_kind_and_nested_variant_validates_and_the_samples_cover_the_table() {
    let mut seen = Seen::new();
    for e in entries() {
        let frame = serde_json::to_value(ServerFrame::entry(1, &e)).unwrap();
        check_server_frame(&PROTOCOL, &frame, None).unwrap_or_else(|err| panic!("{}: {err}", entry_tag(&e.kind)));
    }
    for (event, payload) in server_payloads() {
        let frame = serde_json::to_value(ServerFrame::new(1, None, event, payload.clone())).unwrap();
        check_server_frame(&PROTOCOL, &frame, None).unwrap_or_else(|err| panic!("{event}: {err}"));
        observe(&event_ty(event), &payload, &mut seen);
    }
    let fold = wire::state_payload(&state());
    check(&PROTOCOL, &Ty::Ref("Fold"), &fold).unwrap();
    observe(&Ty::Ref("Fold"), &fold, &mut seen);

    let sampled: BTreeSet<&str> = samples().iter().map(entry_tag).collect();
    let Shape::Union { variants, .. } = PROTOCOL.def("EntryKind").unwrap().shape else { panic!("EntryKind is a union") };
    let table: BTreeSet<&str> = variants.iter().map(|v| v.tag).collect();
    assert_eq!(sampled, table, "every entry kind has a sample and a table row");

    let mut defs = BTreeSet::new();
    for root in ["Entry", "StreamPayload", "Idle", "ApprovalCard", "Fold"] {
        reachable(root, &mut defs);
    }
    let mut missing = Vec::new();
    for d in defs {
        let listed: Vec<&str> = match PROTOCOL.def(d).unwrap().shape {
            Shape::Enum(values) => values.to_vec(),
            Shape::Union { variants, .. } => variants.iter().map(|v| v.tag).collect(),
            _ => continue,
        };
        missing.extend(listed.into_iter().filter(|x| !seen.contains(&(d, x.to_string()))).map(|x| format!("{d}.{x}")));
    }
    assert!(missing.is_empty(), "table rows no real payload produced: {missing:?}");
}

/// `decoded_by_server`: the server reads this type, so the table's required fields must be the
/// ones it refuses to go without; otherwise (a type it only sends) presence is enough.
fn params_agree<T: Serialize + DeserializeOwned>(def: &'static str, full: T, decoded_by_server: bool) {
    let v = serde_json::to_value(&full).unwrap();
    check(&PROTOCOL, &Ty::Ref(def), &v).unwrap_or_else(|e| panic!("{def}: {e}"));
    let Shape::Object(fields) = PROTOCOL.def(def).unwrap().shape else { panic!("{def} is an object") };
    for f in fields {
        assert!(v.get(f.name).is_some(), "{def}.{} is in the table but not in the Rust type", f.name);
    }
    let minimal: Map<String, Value> = fields.iter().filter(|f| !f.optional).map(|f| (f.name.to_string(), v[f.name].clone())).collect();
    serde_json::from_value::<T>(Value::Object(minimal.clone())).unwrap_or_else(|e| panic!("{def}: an optional field is required ({e})"));
    for f in fields.iter().filter(|f| !f.optional && decoded_by_server) {
        let mut m = minimal.clone();
        m.remove(f.name);
        assert!(serde_json::from_value::<T>(Value::Object(m)).is_err(), "{def}.{} is optional on the server", f.name);
    }
}

#[test]
fn the_params_the_server_decodes_agree_with_the_table() {
    params_agree("Hello", Hello { min: 1, max: 1, client_instance: "x".into(), has_local_history: true, allow_takeover: true }, true);
    params_agree("PromptParams", PromptParams { text: "t".into(), client_key: Some("k".into()) }, true);
    params_agree("SteerParams", SteerParams { text: "t".into(), urgent: true }, true);
    params_agree("SubscribeParams", SubscribeParams { from: Some(EntryId(3)) }, true);
    params_agree("RewindParams", RewindParams { checkpoint: 2, mode: Some("code".into()) }, true);
    params_agree("AnswerParams", AnswerParams { id: 1, outcome: "allow".into(), scope: Some("session".into()) }, true);
    let ok = HelloOk { version: 1, capabilities: vec!["session.prompt".into()], session: "ab".into(), head: Some(EntryId(1)) };
    params_agree("HelloOk", ok, false);
}

#[test]
fn every_enumerated_param_value_is_accepted_by_the_server_and_nothing_else() {
    let values = |d: &str| match PROTOCOL.def(d).unwrap().shape {
        Shape::Enum(v) => v,
        _ => panic!("{d} is an enum"),
    };
    for m in values("RewindModeName") {
        assert!(wire::rewind_mode(Some(m)).is_ok(), "rewind mode {m}");
    }
    assert!(wire::rewind_mode(Some("both_ways")).is_err());
    for o in values("AnswerOutcome").iter().filter(|o| **o != "preset") {
        assert!(wire::decided(o, None).is_ok(), "outcome {o}");
    }
    for s in values("GrantScope") {
        assert!(wire::decided("allow", Some(s)).is_ok(), "scope {s}");
    }
    assert!(wire::decided("maybe", None).is_err());
}

#[test]
fn the_rust_sdk_types_decode_every_payload_without_loss() {
    for e in entries() {
        let tag = entry_tag(&e.kind);
        assert_eq!(Event::from_frame(ServerFrame::entry(1, &e)), Event::Entry(Box::new(e)), "{tag}");
    }
    for s in streams() {
        assert_eq!(Event::from_frame(ServerFrame::new(1, None, "stream", wire::stream_payload(&s))), Event::Stream(s));
    }
    for t in transitions() {
        let Event::Idle(idle) = Event::from_frame(ServerFrame::new(1, None, "idle", wire::idle_payload(TurnId(4), &t))) else {
            panic!("idle")
        };
        assert_eq!((idle.turn, idle.transition), (TurnId(4), t));
    }
    for c in cards() {
        let Event::Approval(card) = Event::from_frame(ServerFrame::new(1, None, "approval", c.clone())) else { panic!("approval") };
        assert_eq!(serde_json::to_value(&card).unwrap(), c);
    }
    let fold = wire::state_payload(&state());
    assert_eq!(serde_json::to_value(serde_json::from_value::<Fold>(fold.clone()).unwrap()).unwrap(), fold);
    assert_eq!(Event::from_frame(ServerFrame::error(1, None, "lagging")), Event::Error("lagging".into()));
    let odd = ServerFrame::new(1, None, "teleport", json!({"to": "mars"}));
    assert_eq!(Event::from_frame(odd), Event::Unknown { event: "teleport".into(), payload: json!({"to": "mars"}) });
}

// ───────────────────────────── live ─────────────────────────────

struct Raw {
    w: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    methods: BTreeMap<u64, &'static str>,
    frames: Vec<(Value, Option<&'static str>)>,
}

impl Raw {
    async fn send(&mut self, id: u64, method: &'static str, params: Value) {
        let frame = json!({"v": 1, "id": id, "method": method, "params": params});
        check_client_frame(&PROTOCOL, &frame).unwrap_or_else(|e| panic!("the test sends a frame the table rejects: {e}"));
        self.methods.insert(id, method);
        self.w.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let line =
            tokio::time::timeout(Duration::from_secs(30), self.lines.next_line()).await.expect("a frame within 30 s").unwrap().unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        let method = v["reply_to"].as_u64().and_then(|id| self.methods.get(&id).copied());
        self.frames.push((v.clone(), method));
        v
    }

    async fn until_replied(&mut self, ids: &[u64]) {
        let seen: BTreeSet<u64> = self.frames.iter().filter_map(|(f, _)| f["reply_to"].as_u64()).collect();
        let mut left: BTreeSet<u64> = ids.iter().copied().filter(|id| !seen.contains(id)).collect();
        while !left.is_empty() {
            if let Some(id) = self.recv().await["reply_to"].as_u64() {
                left.remove(&id);
            }
        }
    }
}

#[tokio::test]
async fn every_frame_of_a_live_session_validates() {
    let dir = tempfile::tempdir().unwrap();
    let (project, home) = (dir.path().join("p"), dir.path().join("h"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(project.join("a.txt"), "hello from disk\n").unwrap();
    let script = dir.path().join("script.json");
    let steps = json!({"steps": [
        {"tool_calls": [{"name": "read", "args": {"path": "a.txt"}}], "stop": "tool_use"},
        {"tool_calls": [{"name": "write", "args": {"path": "b.txt", "content": "b\n"}}], "stop": "tool_use"},
        {"thinking": "done", "text": "Read and wrote."}
    ]});
    std::fs::write(&script, steps.to_string()).unwrap();
    let mut b = Sdk::builder().project(&project).home(&home).script(&script).set("policy.rules", "[\"ask write(*)\"]");
    if let Ok(p) = std::env::var("PATH") {
        b = b.env("PATH", &p);
    }
    let sdk = b.build().unwrap();

    let (near, far) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(far);
    let (cr, cw) = tokio::io::split(near);
    let mut raw = Raw { w: cw, lines: BufReader::new(cr).lines(), methods: BTreeMap::new(), frames: Vec::new() };
    let drive = async move {
        raw.send(1, "hello", json!({"min": 1, "max": 1, "client_instance": "schema-test"})).await;
        raw.until_replied(&[1]).await;
        raw.send(2, "session.subscribe", json!({})).await;
        raw.until_replied(&[2]).await;
        raw.send(3, "session.prompt", json!({"text": "read then write"})).await;
        loop {
            let f = raw.recv().await;
            if f["event"] == "approval" {
                raw.send(4, "approval.answer", json!({"id": f["payload"]["id"], "outcome": "allow", "scope": "session"})).await;
            }
            if f["reply_to"] == 3 {
                break;
            }
        }
        raw.send(5, "session.fold", Value::Null).await;
        raw.send(6, "caps.list", json!({"kind": "tool"})).await;
        raw.send(7, "caps.tree", json!({"id": "tool-fs"})).await;
        raw.send(8, "caps.why", json!({"id": "tool-fs"})).await;
        raw.send(9, "caps.explain", json!({"key": "policy.preset"})).await;
        raw.send(10, "caps.explain", json!({"key": "policy.preset", "at": 3})).await;
        raw.send(11, "caps.plan", json!({"patch": "[policy]\npreset = \"dev\"\n"})).await;
        raw.send(12, "caps.set", json!({"key": "tools.grace_ms", "value": 500})).await;
        raw.send(13, "caps.enable", json!({"id": "tool-bash"})).await;
        raw.send(14, "caps.disable", json!({"id": "tool-bash"})).await;
        raw.send(15, "caps.allow", json!({"id": "tool-bash"})).await;
        raw.send(16, "caps.preset", json!({"name": "dev"})).await;
        raw.send(17, "session.steer", json!({"text": "later", "urgent": false})).await;
        raw.send(18, "session.abort", json!({})).await;
        raw.until_replied(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]).await;
        let fold = raw.frames.iter().find(|(f, _)| f["reply_to"] == 5).map(|(f, _)| f["payload"].clone()).unwrap();
        let checkpoint = fold["checkpoints"][0]["id"].as_u64().expect("the write left a checkpoint");
        raw.send(19, "session.rewind", json!({"checkpoint": checkpoint, "mode": "code"})).await;
        raw.until_replied(&[19]).await;
        raw.w.shutdown().await.unwrap();
        while tokio::time::timeout(Duration::from_secs(30), raw.lines.next_line()).await.expect("the server hangs up").unwrap().is_some() {}
        raw.frames
    };
    let (served, frames) = tokio::join!(sdk.serve_io(Open::InMemory, sr, sw), drive);
    served.unwrap();

    let mut events = BTreeSet::new();
    for (f, method) in &frames {
        check_server_frame(&PROTOCOL, f, *method).unwrap_or_else(|e| panic!("{e}\nframe: {f}"));
        if let Some(m) = method {
            assert_ne!(f["event"], "error", "`{m}` failed: {}", f["payload"]);
        }
        events.insert(f["event"].as_str().unwrap().to_string());
    }
    for e in ["hello_ok", "result", "entry", "stream", "idle", "approval"] {
        assert!(events.contains(e), "the live session produced no `{e}` frame");
    }
    let answered: BTreeSet<&str> = frames.iter().filter_map(|(_, m)| *m).collect();
    for m in surface_rpc::METHODS {
        assert!(answered.contains(m), "`{m}` was not exercised");
    }
}
