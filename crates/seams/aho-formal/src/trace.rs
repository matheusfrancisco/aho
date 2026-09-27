//! Log entries → TLA+ action records (every session log is a behavior).
//!
//! One record per entry, in branch order. Common fields: `action`, `id`, `parent` (0 = none),
//! `by` (`operator|engineer|agent|system`), `at` (ms since the first entry of the slice, so it
//! fits TLC's 32-bit integers). The per-action fields are the ones `aho/formal/*.tla`
//! `TraceStep(e)` reads; every other kind becomes `Other{kind}` so a spec can stutter on it.
//!
//! `Task/*` entries are the records of `aho_tasks::tla_record`; a body that does not decode
//! is `TaskUnreadable{name}`, which `Tasks.tla` takes no step for, so it is a finding rather than
//! a stutter. `Invariant/Checked` (`inv`, `verdict`, `code`) and `Workspace/Checkpoint` (`tree`)
//! carry what a verify gate is judged against.

use aho_log::{ApprovalOutcome, Entry, EntryKind, OpStatus, ToolOutcome, Verdict};
use aho_tasks::{Actor, Event, Settings};
use aho_types::{CallId, Consequence, ContentBlock, Form, Principal, Value};
use serde_json::{Map, json};

/// The module whose trace [`trace_for`] completes with the fold's starting settings.
pub const TASKS_MODULE: &str = "Tasks";

pub fn trace_of(entries: &[Entry]) -> Vec<Value> {
    let t0 = entries.first().map(|e| e.at.0).unwrap_or(0);
    entries.iter().map(|e| record(e, t0)).collect()
}

/// The trace `aho session verify --formal` validates against `module`: [`trace_of`], plus what
/// that module's `Init` cannot know. The `Task` fold starts from `Settings::default()` while
/// `Tasks.tla`'s `Init` takes its caps from the model constants, so for `Tasks` a first
/// `Configured` record (entry 0) states the defaults, unless the first `Task/*` entry is itself
/// a `Configured`.
pub fn trace_for(module: &str, entries: &[Entry]) -> Vec<Value> {
    let mut trace = trace_of(entries);
    if module != TASKS_MODULE {
        return trace;
    }
    let first = entries.iter().find_map(|e| match &e.kind {
        EntryKind::Custom { ns, name, .. } if ns == aho_tasks::NS => Some(name.as_str()),
        _ => None,
    });
    if first == Some("Configured") {
        return trace;
    }
    let mut start = aho_tasks::tla_record(&Event::Configured { settings: Settings::default(), by: Actor::System("fold".into()) });
    if let Value::Object(m) = &mut start {
        m.insert("id".into(), json!(0));
        m.insert("parent".into(), json!(0));
        m.insert("by".into(), json!("system"));
        m.insert("at".into(), json!(0));
    }
    trace.insert(0, start);
    trace
}

fn record(e: &Entry, t0: u64) -> Value {
    let (action, fields) = action(&e.kind, e);
    let mut m = Map::new();
    m.insert("action".into(), json!(action));
    m.insert("id".into(), json!(e.id.0));
    m.insert("parent".into(), json!(e.parent.map(|p| p.0).unwrap_or(0)));
    m.insert("by".into(), json!(principal(&e.by)));
    m.insert("at".into(), json!(e.at.0.saturating_sub(t0).min(i32::MAX as u64)));
    if let Value::Object(extra) = fields {
        m.extend(extra);
    }
    Value::Object(m)
}

fn principal(p: &Principal) -> &'static str {
    match p {
        Principal::Operator => "operator",
        Principal::Engineer => "engineer",
        Principal::Agent { .. } => "agent",
        Principal::System { .. } => "system",
    }
}

fn calls(cs: &[CallId]) -> Vec<&str> {
    cs.iter().map(|c| c.0.as_str()).collect()
}

fn form(f: Form) -> &'static str {
    match f {
        Form::Prompt => "prompt",
        Form::Snapshot => "snapshot",
        Form::Catalog => "catalog",
        Form::Instructions => "instructions",
        Form::Notice => "notice",
    }
}

fn outcome(o: ApprovalOutcome) -> &'static str {
    match o {
        ApprovalOutcome::AllowedOnce => "allowed_once",
        ApprovalOutcome::Rejected => "rejected",
        ApprovalOutcome::Cancelled => "cancelled",
        ApprovalOutcome::Unavailable => "unavailable",
    }
}

fn op_status(s: &OpStatus) -> &'static str {
    match s {
        OpStatus::Completed { .. } => "completed",
        OpStatus::Failed { .. } => "failed",
        OpStatus::Cancelled { .. } => "cancelled",
        OpStatus::Uncertain { .. } => "uncertain",
    }
}

/// The serde tag of a transition (`completed`, `tool_use`, `aborted`, …).
fn transition(t: &aho_log::Transition) -> String {
    serde_json::to_value(t).ok().and_then(|v| v.get("transition").and_then(Value::as_str).map(str::to_string)).unwrap_or_default()
}

fn verdict(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass => "pass",
        Verdict::Fail { .. } => "fail",
        Verdict::Unknown { .. } => "unknown",
        Verdict::Stale => "stale",
        Verdict::Vacuous => "vacuous",
    }
}

fn action(k: &EntryKind, e: &Entry) -> (&'static str, Value) {
    match k {
        EntryKind::TurnStart { turn } => ("TurnStart", json!({"turn": turn.0})),
        EntryKind::TurnEnd { turn } => ("TurnEnd", json!({"turn": turn.0})),
        EntryKind::StepStart { step } => ("StepStart", json!({"step": step.0})),
        EntryKind::StepEnd { transition: t, .. } => ("StepEnd", json!({"transition": transition(t)})),
        EntryKind::UserMessage { form: f, .. } => ("Input", json!({"form": form(*f)})),
        EntryKind::RequestHeader { open_calls, policy_epoch, .. } => {
            ("Request", json!({"open": calls(open_calls), "epoch": policy_epoch.0}))
        }
        EntryKind::AssistantMessage { content, .. } => {
            let uses: Vec<&str> = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.0.as_str()),
                    _ => None,
                })
                .collect();
            ("Response", json!({"uses": uses}))
        }
        EntryKind::ToolCall { call, class, .. } => ("Call", json!({"call": call.0, "class": class.name()})),
        EntryKind::ToolResult { call, outcome: o, late, .. } => {
            ("Result", json!({"call": call.0, "late": late, "ok": matches!(o, ToolOutcome::Ok)}))
        }
        EntryKind::OpStarted { op, call, .. } => ("OpStart", json!({"op": op.0, "call": call.0})),
        EntryKind::OpDone { op, status } => ("OpDone", json!({"op": op.0, "status": op_status(status)})),
        EntryKind::ApprovalRequested { call, .. } => {
            ("ApprovalRequested", json!({"req": e.id.0, "call": call.as_ref().map(|c| c.0.as_str()).unwrap_or("")}))
        }
        EntryKind::ApprovalDecided { req, outcome: o, .. } => ("ApprovalDecided", json!({"req": req.0, "outcome": outcome(*o)})),
        EntryKind::PolicyEpoch { epoch, allowed, approval_ref, .. } => {
            let names: Vec<&str> = Consequence::ALL.iter().filter(|c| allowed.contains(**c)).map(|c| c.name()).collect();
            ("Epoch", json!({"epoch": epoch.0, "allowed": names, "approval": approval_ref.map(|r| r.0).unwrap_or(0)}))
        }
        EntryKind::SurfaceReplace { start, end, .. } => ("Replace", json!({"start": start.0, "end": end.0})),
        EntryKind::InvariantChecked { id, verdict: v, code_hash, .. } => {
            ("InvariantChecked", json!({"inv": id, "verdict": verdict(v), "code": code_hash.to_hex()}))
        }
        EntryKind::WorkspaceCheckpoint { tree, .. } => ("WorkspaceCheckpoint", json!({"tree": tree.to_hex()})),
        EntryKind::Custom { ns, name, body } if ns == aho_tasks::NS => match Event::parse(name, body) {
            Ok(ev) => (ev.name(), aho_tasks::tla_record(&ev)),
            Err(_) => ("TaskUnreadable", json!({"name": name})),
        },
        other => ("Other", json!({"kind": other.tag()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_log::{GrantScope, LogStore, MemoryLog, Transition};
    use aho_types::*;

    fn agent() -> Principal {
        Principal::Agent { id: AgentId(1), turn: TurnId(1) }
    }

    /// A small real log: appended through `MemoryLog`, so ids/parents/hashes are the store's.
    fn session() -> Vec<Entry> {
        let log = MemoryLog::new();
        let sys = Principal::system("loop");
        let ap = |by: Principal, k: EntryKind| log.append(None, by, k).map(|e| e.id).unwrap();
        ap(
            sys.clone(),
            EntryKind::PolicyEpoch {
                epoch: Epoch(1),
                allowed: ConsequenceSet::of(&[Consequence::ReadOnly]),
                preset: "read-only".into(),
                approval_ref: None,
            },
        );
        ap(sys.clone(), EntryKind::TurnStart { turn: TurnId(1) });
        ap(Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "go".into(), client_key: None });
        ap(
            sys.clone(),
            EntryKind::RequestHeader {
                model: "m".into(),
                tools_visible: vec![],
                policy_epoch: Epoch(1),
                plan_hash: Hash::ZERO,
                section_hashes: vec![],
                open_calls: vec![],
                request_hash: Hash::ZERO,
            },
        );
        ap(
            Principal::system("model"),
            EntryKind::AssistantMessage {
                content: vec![
                    ContentBlock::text("x"),
                    ContentBlock::ToolUse { id: CallId("c1".into()), name: "bash".into(), args: json!({}) },
                ],
                usage: Usage::default(),
                stop: StopReason::ToolUse,
            },
        );
        ap(
            agent(),
            EntryKind::ToolCall { call: CallId("c1".into()), tool: "bash".into(), args: json!({}), class: Consequence::SpawnsProcess },
        );
        let req = ap(
            sys.clone(),
            EntryKind::ApprovalRequested {
                wants: ConsequenceSet::of(&[Consequence::SpawnsProcess]),
                for_cap: "bash".into(),
                call: Some(CallId("c1".into())),
                reason: "r".into(),
            },
        );
        ap(Principal::Operator, EntryKind::ApprovalDecided { req, outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Session });
        ap(
            Principal::Operator,
            EntryKind::PolicyEpoch {
                epoch: Epoch(2),
                allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::SpawnsProcess]),
                preset: "custom".into(),
                approval_ref: Some(req),
            },
        );
        ap(
            Principal::system("ops"),
            EntryKind::OpStarted {
                op: OpId(7),
                call: CallId("c1".into()),
                kind: aho_log::OpKind::Process,
                rerunnable: false,
                external_effect: false,
            },
        );
        ap(
            Principal::system("tool"),
            EntryKind::ToolResult {
                call: CallId("c1".into()),
                outcome: ToolOutcome::Ok,
                value: json!(null),
                content: vec![],
                spill_ref: None,
                meta: None,
                late: false,
            },
        );
        ap(Principal::system("ops"), EntryKind::OpDone { op: OpId(7), status: OpStatus::Completed { code: 0 } });
        ap(sys.clone(), EntryKind::StepEnd { step: StepId(1), transition: Transition::Completed });
        ap(
            sys.clone(),
            EntryKind::SurfaceReplace { start: EntryId(4), end: EntryId(11), shadow_price: 1, summary: None, by_cap: "c".into() },
        );
        ap(sys.clone(), EntryKind::TurnEnd { turn: TurnId(1) });
        ap(sys, EntryKind::Note { text: "n".into() });
        let head = log.head().unwrap();
        log.read_branch(head).unwrap()
    }

    #[test]
    fn a_real_log_maps_to_action_records() {
        let entries = session();
        // the log itself folds cleanly: the trace is of a legal session
        aho_log::fold(&entries, &Default::default()).unwrap();
        let t = trace_of(&entries);
        assert_eq!(t.len(), entries.len());
        let actions: Vec<&str> = t.iter().map(|r| r["action"].as_str().unwrap()).collect();
        assert_eq!(
            actions,
            [
                "Epoch",
                "TurnStart",
                "Input",
                "Request",
                "Response",
                "Call",
                "ApprovalRequested",
                "ApprovalDecided",
                "Epoch",
                "OpStart",
                "Result",
                "OpDone",
                "StepEnd",
                "Replace",
                "TurnEnd",
                "Other"
            ]
        );
        for (r, e) in t.iter().zip(&entries) {
            assert_eq!(r["id"], json!(e.id.0));
            assert_eq!(r["parent"], json!(e.parent.map(|p| p.0).unwrap_or(0)));
            assert!(r["at"].as_u64().unwrap() <= i32::MAX as u64);
        }
        assert_eq!(t[0]["allowed"], json!(["ReadOnly"]));
        assert_eq!(t[2]["form"], json!("prompt"));
        assert_eq!(t[2]["by"], json!("operator"));
        assert_eq!(t[4]["uses"], json!(["c1"]));
        assert_eq!(t[5]["class"], json!("SpawnsProcess"));
        assert_eq!(t[5]["by"], json!("agent"));
        assert_eq!(t[6]["req"], t[6]["id"], "a request is referenced by its own entry id");
        assert_eq!(t[6]["call"], json!("c1"));
        assert_eq!(t[7]["req"], t[6]["id"]);
        assert_eq!(t[7]["outcome"], json!("allowed_once"));
        assert_eq!(t[8]["approval"], t[6]["id"], "the widening epoch cites the approval");
        assert_eq!(t[8]["allowed"], json!(["ReadOnly", "SpawnsProcess"]), "classes in the fixed lattice order");
        assert_eq!(t[10]["ok"], json!(true));
        assert_eq!(t[10]["late"], json!(false));
        assert_eq!(t[11]["status"], json!("completed"));
        assert_eq!(t[12]["transition"], json!("completed"));
        assert_eq!((t[13]["start"].clone(), t[13]["end"].clone()), (json!(4), json!(11)));
        assert_eq!(t[15]["kind"], json!("Note"));
        assert_eq!(t[1]["parent"], t[0]["id"]);
        // every record renders as a TLA+ record
        for r in &t {
            crate::tla::to_tla(r).unwrap();
        }
    }

    #[test]
    fn timestamps_are_relative_to_the_slice() {
        let mut entries = session();
        entries[0].at = Timestamp(1_700_000_000_000);
        entries[1].at = Timestamp(1_700_000_000_250);
        let t = trace_of(&entries[..2]);
        assert_eq!((t[0]["at"].clone(), t[1]["at"].clone()), (json!(0), json!(250)));
        assert!(trace_of(&[]).is_empty());
    }

    #[test]
    fn the_tasks_trace_starts_from_the_folds_defaults_and_keeps_bad_task_bodies_visible() {
        let log = MemoryLog::new();
        let ap = |k: EntryKind| log.append(None, Principal::system("tool-tasks"), k).unwrap();
        ap(EntryKind::InvariantChecked {
            id: "inv.a".into(),
            mode: aho_log::CheckMode::Bounded(3),
            verdict: Verdict::Fail { counterexample: Hash::of(b"cx") },
            code_hash: Hash::of(b"code"),
        });
        let seed = Event::Seeded {
            id: aho_tasks::TaskId::new("a"),
            content: "do a".into(),
            kind: aho_tasks::Kind::Code,
            depends_on: vec![],
            origin: aho_tasks::Origin::Seed,
            parent: None,
            by: Actor::Operator,
        };
        ap(seed.to_entry().unwrap());
        ap(EntryKind::Custom { ns: "Task".into(), name: "Completed".into(), body: json!({"id": "a"}) });
        let entries = log.read_branch(log.head().unwrap()).unwrap();

        let t = trace_for(TASKS_MODULE, &entries);
        let actions: Vec<&str> = t.iter().map(|r| r["action"].as_str().unwrap()).collect();
        assert_eq!(actions, ["Configured", "InvariantChecked", "Seeded", "TaskUnreadable"]);
        let d = Settings::default();
        assert_eq!((t[0]["id"].clone(), t[0]["maxLive"].clone()), (json!(0), json!(d.max_live_workers)));
        assert_eq!((t[1]["inv"].clone(), t[1]["verdict"].clone()), (json!("inv.a"), json!("fail")));
        assert_eq!(t[1]["code"], json!(Hash::of(b"code").to_hex()));
        assert_eq!((t[2]["task"].clone(), t[2]["id"].clone()), (json!("a"), json!(entries[1].id.0)));
        assert_eq!(t[3]["name"], json!("Completed"));
        assert_eq!(trace_for("Log", &entries), trace_of(&entries), "only Tasks gets the starting record");

        // a log that configures first needs no stated defaults
        let configured = MemoryLog::new();
        let settings = Settings { max_live_workers: 1, ..Settings::default() };
        let first = Event::Configured { settings, by: Actor::System("swarm".into()) };
        configured.append(None, Principal::system("tool-tasks"), first.to_entry().unwrap()).unwrap();
        let entries = configured.read_branch(configured.head().unwrap()).unwrap();
        let t = trace_for(TASKS_MODULE, &entries);
        assert_eq!((t.len(), t[0]["maxLive"].clone()), (1, json!(1)));
    }
}
