//! The four tools over a log populated by direct appends: ranking, caps, filters, branch
//! selection, errors, the trace across a fork, and redaction.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::sync::Arc;

use aho_cap::{BuildError, Capability, Config, ExecOutcome};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_log::{ApprovalOutcome, CheckpointCause, EntryKind, GrantScope, LogStore, MemoryLog, OpKind, OpStatus, ToolOutcome};
use aho_types::*;
use serde_json::{Value, json};
use tool_session::{Limits, Redactor, SessionQueryConfig, SessionQueryDeps, ToolSession};

const SECRET: &str = "hunter2-very-secret";

fn put(log: &MemoryLog, kind: EntryKind) -> u64 {
    log.append(None, Principal::system("test"), kind).unwrap().id.0
}

fn user(text: &str) -> EntryKind {
    EntryKind::UserMessage { form: Form::Prompt, text: text.into(), client_key: None }
}

/// 1..=16 on the main branch, then a fork at the checkpoint (#12) with one note (#17).
fn fixture() -> (MemoryLog, u64) {
    let log = MemoryLog::new();
    let c1 = CallId("c1".into());
    put(&log, EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: ConsequenceSet::ALL, preset: "workspace".into(), approval_ref: None });
    put(&log, EntryKind::TurnStart { turn: TurnId(1) });
    put(&log, user("please deploy the widget"));
    put(
        &log,
        EntryKind::AssistantMessage {
            content: vec![
                ContentBlock::text("deploying"),
                ContentBlock::ToolUse { id: c1.clone(), name: "bash".into(), args: json!({"command": "deploy widget"}) },
            ],
            usage: Usage::default(),
            stop: StopReason::ToolUse,
        },
    );
    put(
        &log,
        EntryKind::ToolCall {
            call: c1.clone(),
            tool: "bash".into(),
            args: json!({"command": "deploy widget"}),
            class: Consequence::SpawnsProcess,
        },
    );
    let req = put(
        &log,
        EntryKind::ApprovalRequested {
            wants: ConsequenceSet::EMPTY.with(Consequence::SpawnsProcess),
            for_cap: "tool-bash".into(),
            call: Some(c1.clone()),
            reason: "deploy".into(),
        },
    );
    put(&log, EntryKind::ApprovalDecided { req: EntryId(req), outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Once });
    put(&log, EntryKind::OpStarted { op: OpId(1), call: c1.clone(), kind: OpKind::Process, rerunnable: false, external_effect: true });
    put(&log, EntryKind::OpDone { op: OpId(1), status: OpStatus::Completed { code: 0 } });
    put(
        &log,
        EntryKind::ToolResult {
            call: c1.clone(),
            outcome: ToolOutcome::Ok,
            value: json!(null),
            content: vec![
                ContentBlock::text(format!("widget deployed with token {SECRET}")),
                ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2, 3] },
            ],
            spill_ref: None,
            meta: None,
            late: false,
        },
    );
    put(
        &log,
        EntryKind::Custom { ns: "Job".into(), name: "Adopted".into(), body: json!({"call_id": "c1", "note": format!("key={SECRET}")}) },
    );
    let cp = put(
        &log,
        EntryKind::WorkspaceCheckpoint { id: CheckpointId(1), tree: Hash::of(b"t1"), parent: None, cause: CheckpointCause::ToolResult(c1) },
    );
    put(&log, EntryKind::Note { text: "remember the widget".into() });
    put(&log, EntryKind::TurnEnd { turn: TurnId(1) });
    put(&log, EntryKind::TurnStart { turn: TurnId(2) });
    let old = put(&log, user("again widget"));
    log.fork(EntryId(cp)).unwrap();
    put(&log, EntryKind::Note { text: "alt branch widget".into() });
    (log, old)
}

fn tool(log: MemoryLog, lim: Limits) -> ToolSession {
    ToolSession::new(Arc::new(log), Redactor::new([SECRET.to_string()]), lim)
}

fn run(t: &ToolSession, name: &str, args: Value) -> ExecOutcome {
    t.run(name, &args, &CallId("probe".into()))
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect(),
        other => panic!("expected a result, got {other:?}"),
    }
}

fn value(o: &ExecOutcome) -> Value {
    match o {
        ExecOutcome::Done { value, .. } => value.clone(),
        other => panic!("expected a result, got {other:?}"),
    }
}

fn code(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Err { code, .. } => code.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn hits(o: &ExecOutcome) -> Vec<u64> {
    value(o)["hits"].as_array().unwrap().iter().map(|h| h["id"].as_u64().unwrap()).collect()
}

#[test]
fn search_ranks_what_people_wrote_first_and_respects_the_hit_cap() {
    let (log, old) = fixture();
    let t = tool(log, Limits { max_hits: 2, ..Limits::default() });
    let o = run(&t, "session_search", json!({"query": "widget", "branch": old}));
    let v = value(&o);
    assert_eq!(hits(&o), vec![16, 13], "notes and user messages outrank tool output, newest first: {}", text(&o));
    assert!(v["total"].as_u64().unwrap() > 2 && v["capped"] == true, "{v}");
    assert!(text(&o).contains("more match; narrow"), "{}", text(&o));
    let one = run(&t, "session_search", json!({"query": "widget", "branch": old, "limit": 1}));
    assert_eq!(hits(&one), vec![16]);
}

#[test]
fn search_filters_by_turn_tool_and_kind_and_rejects_bad_input() {
    let (log, old) = fixture();
    let t = tool(log, Limits::default());
    assert_eq!(hits(&run(&t, "session_search", json!({"query": "widget", "at": old, "turn_from": 2}))), vec![16]);
    assert_eq!(hits(&run(&t, "session_search", json!({"query": "widget", "at": old, "turn_to": 0}))), Vec::<u64>::new());
    let mut by_tool = hits(&run(&t, "session_search", json!({"query": "widget", "tool": "bash"})));
    by_tool.sort();
    assert_eq!(by_tool, vec![5, 10], "tool calls and results of bash only");
    assert_eq!(hits(&run(&t, "session_search", json!({"query": "adopted|note", "regex": true, "kinds": ["Job/*"]}))), vec![11]);
    assert_eq!(code(&run(&t, "session_search", json!({"query": "x", "kinds": ["tool_calls_please"]}))), "BAD_KIND");
    assert_eq!(code(&run(&t, "session_search", json!({"query": "(", "regex": true}))), "BAD_REGEX");
    assert_eq!(code(&run(&t, "session_search", json!({"query": "  "}))), "BAD_ARGS");
    assert_eq!(code(&run(&t, "session_search", json!({"query": "x", "turn_from": 3, "turn_to": 1}))), "BAD_ARGS");
    let literal = run(&t, "session_search", json!({"query": "("}));
    assert!(hits(&literal).is_empty(), "a literal search treats `(` as text");
}

#[test]
fn branches_default_to_the_head_and_can_be_selected() {
    let (log, old) = fixture();
    let t = tool(log, Limits::default());
    assert_eq!(hits(&run(&t, "session_search", json!({"query": "alt branch"}))), vec![17]);
    assert!(hits(&run(&t, "session_search", json!({"query": "alt branch", "branch": old}))).is_empty());
    assert!(hits(&run(&t, "session_search", json!({"query": "again"}))).is_empty(), "#16 is off the current branch");
    let e = run(&t, "session_search", json!({"query": "x", "branch": 3}));
    assert_eq!(code(&e), "NOT_A_BRANCH");
    assert_eq!(code(&run(&t, "session_search", json!({"query": "x", "branch": old, "at": 17}))), "NOT_ON_BRANCH");
    assert_eq!(code(&run(&t, "event_list", json!({"at": 999}))), "NO_SUCH_ENTRY");
}

#[test]
fn trace_follows_ops_and_approvals_to_the_checkpoint_and_marks_the_fork() {
    let (log, _) = fixture();
    let t = tool(log, Limits::default());
    let o = run(&t, "session_trace", json!({"id": 9}));
    let ids: Vec<u64> = value(&o)["entries"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, vec![4, 5, 6, 7, 8, 9, 10, 11, 12], "{}", text(&o));
    let s = text(&o);
    assert!(s.contains("fork point: branches continue at #13 (off-branch, head #16), #17 (current branch)"), "{s}");
    assert!(s.contains("← traced entry") && s.contains("answers #6"), "{s}");
    assert!(!s.contains(SECRET), "{s}");
    assert_eq!(value(&run(&t, "session_trace", json!({"call": "c1"})))["total"], 9);
    let note = run(&t, "session_trace", json!({"id": 17}));
    assert!(text(&note).contains("parent") && text(&note).contains("#12"), "{}", text(&note));
    assert_eq!(code(&run(&t, "session_trace", json!({}))), "BAD_ARGS");
    assert_eq!(code(&run(&t, "session_trace", json!({"id": 1, "call": "c1"}))), "BAD_ARGS");
    assert_eq!(code(&run(&t, "session_trace", json!({"call": "nope"}))), "NO_SUCH_CALL");
}

#[test]
fn trace_caps_the_rendered_chain() {
    let (log, _) = fixture();
    let t = tool(log, Limits { max_trace: 3, ..Limits::default() });
    let o = run(&t, "session_trace", json!({"call": "c1"}));
    assert_eq!(value(&o)["entries"].as_array().unwrap().len(), 3);
    assert!(text(&o).contains("6 more linked entries not shown"), "{}", text(&o));
}

#[test]
fn event_get_redacts_secrets_and_image_bytes_and_places_the_entry() {
    let (log, _) = fixture();
    let t = tool(log, Limits::default());
    for id in [10, 11] {
        let s = text(&run(&t, "event_get", json!({"id": id})));
        assert!(s.contains("[redacted]") && !s.contains(SECRET), "{s}");
    }
    let s = text(&run(&t, "event_get", json!({"id": 10})));
    assert!(s.contains("[3 bytes]") && s.contains("current branch"), "{s}");
    let s = text(&run(&t, "event_get", json!({"id": "#12"})));
    assert!(s.contains("fork point"), "{s}");
    let v = value(&run(&t, "event_get", json!({"id": 16})));
    assert_eq!((v["on_current_branch"].clone(), v["turn"].clone()), (json!(false), json!(2)));
    assert_eq!(code(&run(&t, "event_get", json!({"id": 0}))), "NO_SUCH_ENTRY");
    assert_eq!(code(&run(&t, "event_get", json!({"id": "twelve"}))), "BAD_ARGS");
    let search = run(&t, "session_search", json!({"query": SECRET, "kinds": ["tool", "Job/*"]}));
    assert!(hits(&search).is_empty(), "a secret is never matched");
}

#[test]
fn event_list_windows_by_id_and_turn_with_paging_hints() {
    let (log, old) = fixture();
    let t = tool(log, Limits { max_window: 2, ..Limits::default() });
    let ids = |o: &ExecOutcome| value(o)["entries"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect::<Vec<_>>();
    let o = run(&t, "event_list", json!({"from": 3, "limit": 50}));
    assert_eq!(ids(&o), vec![3, 4], "limit is capped by max_window");
    assert!(text(&o).contains("continue with from=5"), "{}", text(&o));
    let o = run(&t, "event_list", json!({}));
    assert_eq!(ids(&o), vec![12, 17], "the newest entries of the current branch");
    assert!(text(&o).contains("earlier; page back with to=11"), "{}", text(&o));
    assert_eq!(ids(&run(&t, "event_list", json!({"kinds": ["note"], "branch": old}))), vec![13]);
    assert_eq!(ids(&run(&t, "event_list", json!({"turn_from": 2, "branch": old}))), vec![15, 16]);
    assert_eq!(code(&run(&t, "event_list", json!({"from": 5, "to": 4}))), "BAD_ARGS");
}

#[test]
fn empty_logs_unknown_tools_and_missing_seeds_are_reported() {
    let t = ToolSession::new(Arc::new(MemoryLog::new()), Redactor::default(), Limits::default());
    assert_eq!(code(&run(&t, "event_list", json!({}))), "EMPTY_LOG");
    assert_eq!(code(&run(&t, "session_trace", json!({"id": 1}))), "EMPTY_LOG");
    assert_eq!(code(&run(&t, "nope", json!({}))), "UNKNOWN_TOOL");
    let cfg = SessionQueryConfig { limits: Limits::default(), secrets: vec![] };
    assert!(matches!(ToolSession::build(SessionQueryDeps { log: None, index: None }, cfg), Err(BuildError::Unsupported(_))));
}

#[test]
fn config_reads_limits_and_secrets_and_rejects_out_of_range_values() {
    let load = |text: &str| {
        let mut schema = Schema::default();
        for k in SessionQueryConfig::keys() {
            schema.add(k);
        }
        schema.add(aho_config::ConfigKey::new("credentials.demo", aho_config::Ty::String, None, "x").secret());
        let mut inputs = ResolveInputs::default();
        inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: text.into() });
        resolve(&inputs, &schema, false).unwrap()
    };
    let cfg = SessionQueryConfig::from_loadout(&load("[session_query]\nmax_hits = 7\n[credentials]\ndemo = \"s3cr3t-value\"\n")).unwrap();
    assert_eq!(cfg.limits.max_hits, 7);
    assert_eq!(cfg.secrets, vec!["s3cr3t-value".to_string()]);
    assert!(SessionQueryConfig::from_loadout(&load("[session_query]\nmax_hits = 0\n")).is_err());
    assert!(SessionQueryConfig::from_loadout(&load("[session_query]\nmax_window = 100000\n")).is_err());
}
