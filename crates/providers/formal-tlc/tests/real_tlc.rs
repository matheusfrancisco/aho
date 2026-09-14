//! Real TLC (network + java; `cargo test -p formal-tlc --test real_tlc -- --ignored`).
//! Downloads tla2tools.jar v1.8.0 once into this test's target tmp dir, re-records the fake's
//! fixtures through `record-tlc.sh` (so the offline tests replay exactly what TLC printed),
//! and model-checks, trace-validates and mutates aho's own specs under `aho/formal/`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use aho_formal::*;
use common::*;

const JAR_URL: &str = "https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar";

fn jar() -> PathBuf {
    let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tla2tools.jar");
    if !p.is_file() {
        let ok = Command::new("curl").args(["-fsSL", "-o"]).arg(&p).arg(JAR_URL).status().map(|s| s.success()).unwrap_or(false);
        assert!(ok && p.is_file(), "could not download {JAR_URL}");
    }
    p
}

fn formal_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../formal")
}

/// `<Module>.cfg` → `CheckConfig` (INIT/NEXT/INVARIANT/CONSTANTS/CHECK_DEADLOCK).
fn cfg_of(module: &str) -> CheckConfig {
    let text = std::fs::read_to_string(formal_dir().join(format!("{module}.cfg"))).unwrap();
    aho_formal::tla::parse_cfg(&text).unwrap()
}

#[tokio::test]
#[ignore = "network + java: downloads tla2tools.jar and runs the real TLC"]
async fn real_tlc_records_the_fake_fixtures() {
    let t = tlc(&fixtures().join("record-tlc.sh"), &jar());
    run_all(&t).await;
}

#[tokio::test]
#[ignore = "network + java: downloads tla2tools.jar and runs the real TLC"]
async fn aho_specs_pass_validate_real_logs_and_catch_mutants() {
    let t = tlc(Path::new("java"), &jar());
    for module in ["Log", "Policy", "Turn"] {
        let s = SpecRef::tla(formal_dir().join(format!("{module}.tla")));
        let cfg = cfg_of(module);
        let o = t.check(&s, &cfg).await.unwrap();
        assert_eq!(o.verdict, Verdict::Pass, "{module}: {}", o.output);
        println!("{module}: Pass, {:?}, {} ms", o.mode, o.cost_ms);

        let trace = trace_of(&sample_session());
        let o = t.trace_validate(&s, &trace).await.unwrap();
        assert_eq!(o.verdict, Verdict::Pass, "{module} rejects a legal session log: {:?}\n{}", o.counterexample, o.output);
        println!("{module}: trace of {} records accepted, {} ms", trace.len(), o.cost_ms);

        // mutants may drop the guard that keeps the model finite: bound them in depth and time
        let mcfg = CheckConfig { mode: CheckMode::Bounded(4), budget_ms: 120_000, ..cfg.clone() };
        let r = t.mutate(&s, &mcfg).await.unwrap();
        println!("{module}: {} of {} mutants caught", r.caught(), r.mutants.len());
        for m in &r.mutants {
            println!("  {:<40} caught={} {:?}", m.name, m.caught, m.violated);
        }
        assert!(r.caught() >= 1, "{module} is vacuous");
    }
}

/// A legal session appended through the real store (fold-checked), incl. an approval and a
/// widening epoch that cites it.
fn sample_session() -> Vec<aho_log::Entry> {
    use aho_log::*;
    use aho_types::*;
    use serde_json::json;
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
    ap(sys.clone(), EntryKind::StepStart { step: StepId(1) });
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
                ContentBlock::ToolUse { id: CallId("c1".into()), name: "bash".into(), args: json!({}) },
                ContentBlock::ToolUse { id: CallId("c2".into()), name: "read".into(), args: json!({}) },
            ],
            usage: Usage::default(),
            stop: StopReason::ToolUse,
        },
    );
    let agent = Principal::Agent { id: AgentId(1), turn: TurnId(1) };
    ap(
        agent.clone(),
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
        EntryKind::OpStarted { op: OpId(1), call: CallId("c1".into()), kind: OpKind::Process, rerunnable: false, external_effect: false },
    );
    ap(agent, EntryKind::ToolCall { call: CallId("c2".into()), tool: "read".into(), args: json!({}), class: Consequence::ReadOnly });
    ap(
        Principal::system("ops"),
        EntryKind::OpStarted { op: OpId(2), call: CallId("c2".into()), kind: OpKind::Io, rerunnable: true, external_effect: false },
    );
    for (c, op) in [("c2", 2), ("c1", 1)] {
        ap(
            Principal::system("tool"),
            EntryKind::ToolResult {
                call: CallId(c.into()),
                outcome: ToolOutcome::Ok,
                value: json!(null),
                content: vec![],
                spill_ref: None,
                meta: None,
                late: false,
            },
        );
        ap(Principal::system("ops"), EntryKind::OpDone { op: OpId(op), status: OpStatus::Completed { code: 0 } });
    }
    ap(sys.clone(), EntryKind::StepEnd { step: StepId(1), transition: Transition::ToolUse });
    ap(sys.clone(), EntryKind::StepStart { step: StepId(2) });
    ap(
        sys.clone(),
        EntryKind::RequestHeader {
            model: "m".into(),
            tools_visible: vec![],
            policy_epoch: Epoch(2),
            plan_hash: Hash::ZERO,
            section_hashes: vec![],
            open_calls: vec![],
            request_hash: Hash::ZERO,
        },
    );
    ap(
        Principal::system("model"),
        EntryKind::AssistantMessage { content: vec![ContentBlock::text("done")], usage: Usage::default(), stop: StopReason::EndTurn },
    );
    ap(sys.clone(), EntryKind::StepEnd { step: StepId(2), transition: Transition::Completed });
    ap(
        sys.clone(),
        EntryKind::SurfaceReplace {
            start: EntryId(3),
            end: EntryId(5),
            shadow_price: 1,
            summary: Some("s".into()),
            by_cap: "compaction".into(),
        },
    );
    ap(sys, EntryKind::TurnEnd { turn: TurnId(1) });
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    fold(&entries, &Default::default()).unwrap();
    entries
}
