//! Wire schema snapshot: every frame type, serialized by the functions the server uses,
//! compared to the committed `tests/fixtures/frames.json`. A diff is a protocol change: additive
//! fields are a minor bump, anything else needs a new major and a handshake decision.
//! Regenerate deliberately with `UPDATE_SNAPSHOTS=1 cargo test -p surface-rpc --test schema`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use aho_cap::BoundWorld;
use aho_config::{Layer, Origin, Sourced, Where};
use aho_llm::StreamEvent;
use aho_log::{Entry, EntryKind, State, Transition};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_types::*;
use serde_json::{Value, json};
use surface_rpc::wire::{
    self, AbortParams, AnswerParams, ClientFrame, Hello, HelloOk, JobParams, PromptParams, RewindParams, ServerFrame, SteerParams,
    SubscribeParams,
};

fn client(id: u64, method: &str, params: Value) -> Value {
    serde_json::to_value(ClientFrame { v: 1, id: Some(id), method: method.into(), params }).unwrap()
}

fn j<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

fn server(f: ServerFrame) -> Value {
    serde_json::to_value(f).unwrap()
}

fn frames() -> Value {
    let entry = Entry {
        id: EntryId(42),
        parent: Some(EntryId(41)),
        prev_hash: Hash::of(b"prev"),
        hash: Hash::of(b"this"),
        at: Timestamp(1_700_000_000_000),
        by: Principal::Operator,
        kind: EntryKind::ToolCall {
            call: CallId("c1".into()),
            tool: "read".into(),
            args: json!({"path": "a.txt"}),
            class: Consequence::ReadOnly,
        },
    };
    let card = ApprovalCard {
        tool: "write".into(),
        class: Consequence::WorkspaceWrite,
        args: json!({"path": "y.txt"}),
        reason: DecisionReason::Preset("read-only".into()),
        world: BoundWorld::host(),
        suggested_cut: Some("preset workspace".into()),
        rendered: "`write` is outside preset `read-only`; the user is asked before it runs.".into(),
    };
    let mut state = State { turns: 2, head: Some(EntryId(42)), ..State::default() };
    state.config.insert(
        "policy.preset".into(),
        Sourced {
            value: json!("workspace"),
            winner: Origin::new(Layer::FileProject, Where::File { path: "aho.toml".into(), line: 2 }),
            shadowed: vec![],
        },
    );
    let hello = Hello { min: 1, max: 1, client_instance: "tui-7f".into(), has_local_history: true, allow_takeover: false };
    let hello_ok = HelloOk {
        version: 1,
        capabilities: wire::METHODS.iter().map(|m| m.to_string()).collect(),
        session: "18f3a".into(),
        head: Some(EntryId(42)),
    };
    let job = aho_session::JobInfo {
        op: OpId(7),
        call: CallId("c4".into()),
        tool: "bash".into(),
        cause: aho_log::DetachCause::After(60_000),
        status: None,
        started: Some(Timestamp(1_700_000_000_000)),
    };
    json!({
        "client": {
            "hello": client(1, "hello", j(&hello)),
            "session.prompt": client(2, "session.prompt", j(&PromptParams { text: "run the tests".into(), client_key: Some("k1".into()) })),
            "session.steer": client(3, "session.steer", j(&SteerParams { text: "also lint".into(), urgent: true })),
            "session.abort": client(4, "session.abort", j(&AbortParams { scope: Some("op".into()), op: Some(OpId(7)) })),
            "session.subscribe": client(5, "session.subscribe", j(&SubscribeParams { from: Some(EntryId(41)) })),
            "session.fold": client(6, "session.fold", Value::Null),
            "session.rewind": client(7, "session.rewind", j(&RewindParams { checkpoint: 2, mode: Some("code".into()) })),
            "job.list": client(19, "job.list", Value::Null),
            "job.kill": client(20, "job.kill", j(&JobParams { op: OpId(7) })),
            "job.detach": client(21, "job.detach", j(&JobParams { op: OpId(7) })),
            "approval.answer": client(8, "approval.answer", j(&AnswerParams { id: 1, outcome: "allow".into(), scope: Some("session".into()) })),
            "caps.list": client(9, "caps.list", json!({"kind": "tool"})),
            "caps.tree": client(10, "caps.tree", json!({"id": "tool-fs"})),
            "caps.explain": client(11, "caps.explain", json!({"key": "policy.preset", "at": 12})),
            "caps.why": client(12, "caps.why", json!({"id": "tool-bash"})),
            "caps.plan": client(13, "caps.plan", json!({"patch": "[policy]\npreset = \"dev\"\n", "patch_path": "x.toml"})),
            "caps.set": client(14, "caps.set", json!({"key": "tools.grace_ms", "value": 500, "apply": false})),
            "caps.enable": client(15, "caps.enable", json!({"id": "skill.deploy", "apply": true})),
            "caps.disable": client(16, "caps.disable", json!({"id": "tool-bash"})),
            "caps.allow": client(17, "caps.allow", json!({"id": "tool-bash"})),
            "caps.preset": client(18, "caps.preset", json!({"name": "dev"})),
        },
        "server": {
            "hello_ok": server(ServerFrame::new(1, Some(1), "hello_ok", j(&hello_ok))),
            "result": server(ServerFrame::result(1, Some(2), json!({"transition": Transition::Completed}))),
            "error": server(ServerFrame::error(1, Some(3), "a turn is already running in this session")),
            "entry": server(ServerFrame::entry(1, &entry)),
            "stream.text": server(ServerFrame::new(1, None, "stream", wire::stream_payload(&StreamEvent::Text("hel".into())))),
            "stream.thinking": server(ServerFrame::new(1, None, "stream", wire::stream_payload(&StreamEvent::Thinking("hmm".into())))),
            "stream.tool_use_start": server(ServerFrame::new(1, None, "stream", wire::stream_payload(&StreamEvent::ToolUseStart { id: CallId("c1".into()), name: "read".into() }))),
            "stream.tool_use_end": server(ServerFrame::new(1, None, "stream", wire::stream_payload(&StreamEvent::ToolUseEnd { id: CallId("c1".into()), name: "read".into(), args: json!({"path": "a.txt"}) }))),
            "stream.usage": server(ServerFrame::new(1, None, "stream", wire::stream_payload(&StreamEvent::Usage(Usage { input_tokens: 10, output_tokens: 3, ..Usage::default() })))),
            "idle": server(ServerFrame::new(1, None, "idle", wire::idle_payload(TurnId(2), &Transition::Aborted("stopped by the operator".into())))),
            "approval": server(ServerFrame::new(1, None, "approval", wire::approval_payload(1, &card, Some("tool-fs"), 3))),
            "fold": server(ServerFrame::result(1, Some(6), wire::state_payload(&state))),
            "jobs": server(ServerFrame::result(1, Some(19), json!({ "jobs": [job.to_json()] }))),
            "job_finished": server(ServerFrame::new(1, None, "job_finished", wire::job_finished_payload(OpId(7)))),
        },
    })
}

#[test]
fn every_frame_matches_the_committed_schema() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/frames.json");
    let now = serde_json::to_string_pretty(&frames()).unwrap() + "\n";
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &now).unwrap();
    }
    let committed =
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} is missing; run with UPDATE_SNAPSHOTS=1 once", path.display()));
    assert_eq!(committed, now, "the wire schema changed; decide the version bump, then UPDATE_SNAPSHOTS=1");
}

#[test]
fn every_advertised_method_has_a_snapshot() {
    let f = frames();
    for m in wire::METHODS {
        assert!(f["client"].get(*m).is_some(), "{m} has no frame in the snapshot");
    }
}
