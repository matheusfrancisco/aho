//! Scenarios shared by the offline tests (fake TLC replaying recordings) and the ignored
//! real-TLC test that records them: same inputs, so the same assertions hold for both.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_formal::*;
use aho_proc::SubprocessHandle;
use formal_tlc::{Tlc, TlcConfig};
use serde_json::{Value, json};

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn spec(name: &str) -> SpecRef {
    SpecRef::tla(fixtures().join("specs").join(format!("{name}.tla")))
}

/// Fixtures, `formal/` and `CARGO_TARGET_TMPDIR` share the aho workspace: the test world's root.
pub fn world() -> aho_fs::FsWrite {
    aho_fs::FsWrite(Arc::new(world_host::HostFs::new(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")).unwrap()))
}

pub fn tlc(java: &Path, jar: &Path) -> Tlc {
    let proc = SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&fixtures())));
    let cfg = TlcConfig { jar: jar.to_path_buf(), java: java.display().to_string(), workers: 1, budget_ms: 120_000, max_mutants: 24 };
    Tlc::new(proc, world(), cfg)
}

pub fn counter(max: u32, deadlock: bool, mode: CheckMode) -> CheckConfig {
    CheckConfig {
        mode,
        invariants: vec!["Small".into()],
        constants: vec![("Max".into(), max.to_string())],
        check_deadlock: deadlock,
        budget_ms: 60_000,
        ..CheckConfig::default()
    }
}

pub fn calls_cfg() -> CheckConfig {
    CheckConfig {
        invariants: vec!["TypeOK".into(), "AtMostOneResult".into()],
        constants: vec![("CallIds".into(), "{c1, c2}".into())],
        budget_ms: 60_000,
        ..CheckConfig::default()
    }
}

fn rec(id: u64, action: &str, extra: Value) -> Value {
    let mut r = json!({"action": action, "id": id, "parent": id.saturating_sub(1), "by": "system", "at": id * 10});
    if let (Some(m), Value::Object(x)) = (r.as_object_mut(), extra) {
        m.extend(x);
    }
    r
}

pub fn good_trace() -> Vec<Value> {
    vec![
        rec(1, "TurnStart", json!({"turn": 1})),
        rec(2, "Call", json!({"call": "call_a", "class": "SpawnsProcess"})),
        rec(3, "Call", json!({"call": "call_b", "class": "ReadOnly"})),
        rec(4, "Result", json!({"call": "call_b", "late": false, "ok": true})),
        rec(5, "Result", json!({"call": "call_a", "late": false, "ok": true})),
        rec(6, "TurnEnd", json!({"turn": 1})),
    ]
}

/// The third record is a second result for `call_a`.
pub fn bad_trace() -> Vec<Value> {
    vec![
        rec(1, "Call", json!({"call": "call_a", "class": "SpawnsProcess"})),
        rec(2, "Result", json!({"call": "call_a", "late": false, "ok": true})),
        rec(3, "Result", json!({"call": "call_a", "late": true, "ok": true})),
    ]
}

/// Every scenario, with the assertions that must hold whether TLC is real or replayed.
pub async fn run_all(t: &Tlc) {
    // pass: exhaustive, so the mode reports the depth reached
    let o = t.check(&spec("Pass"), &counter(2, false, CheckMode::Bounded(0))).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{}", o.output);
    assert_eq!(o.mode, CheckMode::Bounded(3), "depth of the complete state graph");
    assert!(o.counterexample.is_none());

    // bounded: behaviors of at most one step
    let o = t.check(&spec("Pass"), &counter(2, false, CheckMode::Bounded(1))).await.unwrap();
    assert_eq!((o.verdict.clone(), o.mode), (Verdict::Pass, CheckMode::Bounded(1)), "{}", o.output);

    // invariant violation with a multi-variable trace
    let small = CheckConfig { invariants: vec!["Small".into()], budget_ms: 60_000, ..CheckConfig::default() };
    let o = t.check(&spec("Violation"), &small).await.unwrap();
    let Verdict::Fail { counterexample } = o.verdict else { panic!("expected Fail: {}", o.output) };
    let cx = o.counterexample.unwrap();
    assert_eq!(counterexample, cx.hash());
    assert_eq!(cx.violated, "invariant Small of Violation");
    assert_eq!(cx.states.len(), 3);
    assert_eq!(cx.states[0]["action"], json!("Initial predicate"));
    assert_eq!(cx.states[1]["vars"]["log"], json!([{"kind": "call", "call": "c1"}]));
    assert_eq!(cx.states[2]["vars"]["open"], json!(["c1", "c2"]));
    assert!(cx.render().contains("State 3: <Call(\"c2\")>"), "{}", cx.render());

    // deadlock
    let o = t.check(&spec("Pass"), &counter(2, true, CheckMode::Bounded(0))).await.unwrap();
    assert!(matches!(o.verdict, Verdict::Fail { .. }), "{}", o.output);
    let cx = o.counterexample.unwrap();
    assert!(cx.violated.starts_with("deadlock"), "{}", cx.violated);
    assert_eq!(cx.states.last().unwrap()["vars"]["x"], json!(2));

    // a module that does not parse is Unknown, not Fail
    let o = t.check(&spec("Broken"), &small).await.unwrap();
    let Verdict::Unknown { reason } = &o.verdict else { panic!("expected Unknown: {:?}", o.verdict) };
    assert!(reason.starts_with("parse error:") && reason.contains("line 6"), "{reason}");

    // trace validation: a real-shaped log is a behavior; a second result is not
    let o = t.trace_validate(&spec("Calls"), &good_trace()).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{}", o.output);
    assert_eq!(o.mode, CheckMode::Trace);
    let o = t.trace_validate(&spec("Calls"), &bad_trace()).await.unwrap();
    assert!(matches!(o.verdict, Verdict::Fail { .. }), "{}", o.output);
    assert_eq!(o.counterexample.unwrap().violated, "trace event 3, Result (entry 3), is not a step of Calls");

    // mutation: the model's guards matter; frames that leave a variable unassigned are not verdicts
    let r = t.mutate(&spec("Calls"), &calls_cfg()).await.unwrap();
    let by = |n: &str| r.mutants.iter().find(|m| m.name == n).unwrap_or_else(|| panic!("no mutant {n}: {:?}", r.mutants));
    assert!(by("drop-guard:Call#2").caught, "re-calling a finished call yields a second result");
    assert!(by("drop-guard:Result#1").caught);
    assert!(by("drop-guard:Call#1").verdict == Verdict::Pass && !by("drop-guard:Call#1").caught, "re-adding an open call is harmless");
    let u = by("unchanged:Call-done");
    assert!(!u.caught && matches!(&u.verdict, Verdict::Unknown { reason } if reason.contains("not completely specified")), "{u:?}");
    assert_eq!(r.verdict(), Verdict::Pass, "at least one mutant caught");
    assert!(r.caught() >= 2 && r.caught() < r.mutants.len());
    let caught = by("drop-guard:Call#2");
    assert!(caught.violated.as_deref().unwrap_or("").contains("AtMostOneResult"), "{caught:?}");
}
