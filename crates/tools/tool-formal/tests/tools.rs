//! The six tools through their registered exec functions: arguments, SPEC.md resolution, the
//! approval flow, and the `value.events` the session appends. A recording backend stands in
//! for the checker where the tool's own logic is under test; one test runs the real TLC
//! provider against formal-tlc's replayed TLC output.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aho_approval::{Approval, ApprovalHandle, ChannelApproval, Decided, NeverApproval};
use aho_cap::*;
use aho_formal::{
    CheckConfig, CheckMode, Counterexample, Formal, FormalError, FormalHandle, FormalKind, Formals, Goal, MutantOutcome, MutationReport,
    Outcome, SpecRef, Translation, Verdict,
};
use aho_log::{EntryKind, GrantScope, LogStore};
use aho_types::*;
use serde_json::json;
use tool_formal::{FormalDeps, FormalToolConfig, TOOLS, ToolFormal};

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct Seen {
    specs: Vec<(SpecRef, Option<CheckConfig>, bool)>,
    traces: Vec<Vec<Value>>,
    proved: Vec<String>,
}

/// Records what the tool asked for; answers by spec name.
struct Recorder {
    kind: FormalKind,
    seen: Arc<Mutex<Seen>>,
}

fn outcome(verdict: Verdict, mode: CheckMode) -> Outcome {
    Outcome {
        verdict,
        mode,
        counterexample: None,
        goals: vec![],
        diagnostics: vec![],
        spec_hash: Hash::of(b"spec"),
        cost_ms: 7,
        output: "tail".into(),
    }
}

impl Formal for Recorder {
    fn kind(&self) -> FormalKind {
        self.kind
    }
    fn check<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            let exists = spec.path.is_file();
            lk(&self.seen).specs.push((spec.clone(), Some(cfg.clone()), exists));
            if spec.module == "Bad" {
                let cx = Counterexample {
                    states: vec![json!({"step": 1, "action": "Initial predicate", "tla": {"x": "0"}})],
                    violated: "invariant Inv of Bad".into(),
                };
                return Ok(Outcome { counterexample: Some(cx.clone()), ..outcome(Verdict::Fail { counterexample: cx.hash() }, cfg.mode) });
            }
            if cfg.mode == CheckMode::Inductive {
                return Err(FormalError::Unsupported("TLC checks finite instances".into()));
            }
            Ok(outcome(Verdict::Pass, CheckMode::Bounded(5)))
        })
    }
    fn trace_validate<'a>(&'a self, spec: &'a SpecRef, trace: &'a [Value]) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            lk(&self.seen).specs.push((spec.clone(), None, spec.path.is_file()));
            lk(&self.seen).traces.push(trace.to_vec());
            Ok(outcome(Verdict::Pass, CheckMode::Trace))
        })
    }
    fn prove<'a>(&'a self, module: &'a str) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            lk(&self.seen).proved.push(module.to_string());
            Ok(Outcome {
                goals: vec![Goal {
                    name: "fold_append (Aho/Fold.lean:3:2)".into(),
                    hypotheses: vec!["xs : List Nat".into()],
                    target: "fold xs = 0".into(),
                }],
                ..outcome(Verdict::Unknown { reason: "unproved: 1 open goal(s) in fold_append".into() }, CheckMode::Proof)
            })
        })
    }
    fn translate<'a>(&'a self, krate: &'a str, fns: &'a [String]) -> BoxFuture<'a, Result<Translation, FormalError>> {
        Box::pin(async move {
            Ok(Translation { module: "Aeneas.Demo.Demo".into(), path: PathBuf::from(krate).join("x.lean"), hash: Hash::of_json(&fns) })
        })
    }
    fn mutate<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<MutationReport, FormalError>> {
        Box::pin(async move {
            lk(&self.seen).specs.push((spec.clone(), Some(cfg.clone()), spec.path.is_file()));
            let caught = spec.module != "Weak";
            let m = |name: &str, caught: bool| MutantOutcome {
                name: name.into(),
                description: format!("{name} changed"),
                caught,
                verdict: if caught { Verdict::Fail { counterexample: Hash::of(name.as_bytes()) } } else { Verdict::Pass },
                violated: caught.then(|| "invariant Inv".to_string()),
            };
            Ok(MutationReport { mutants: vec![m("drop-guard:A#1", caught), m("swap-cmp:A#1", false)], cost_ms: 3 })
        })
    }
}

struct Fixture {
    ws: tempfile::TempDir,
    seen: Arc<Mutex<Seen>>,
}

impl Fixture {
    fn new() -> Fixture {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("spec")).unwrap();
        Fixture { ws, seen: Arc::default() }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.ws.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }
    fn fs(&self) -> Arc<dyn aho_fs::Fs> {
        Arc::new(world_host::HostFs::new(self.ws.path()).unwrap())
    }
    fn deps(&self, formals: Formals, approval: Option<Arc<dyn Approval>>) -> FormalDeps {
        let fs = self.fs();
        FormalDeps {
            formals,
            read: aho_fs::FsRead(fs.clone()),
            write: aho_fs::FsWrite(fs),
            approval: approval.map(ApprovalHandle),
            snapshot: None,
        }
    }
    fn tools(&self, approval: Option<Arc<dyn Approval>>) -> ToolFormal {
        let h = |kind| Some(FormalHandle(Arc::new(Recorder { kind, seen: self.seen.clone() })));
        ToolFormal::new(
            self.deps(Formals { tla: h(FormalKind::Tla), lean: h(FormalKind::Lean) }, approval),
            FormalToolConfig { spec_doc: "spec/SPEC.md".into(), budget_ms: 1000 },
        )
    }
    fn view(&self, tool: &str, args: Value) -> ExecView {
        let decl = TOOLS.iter().find(|d| d.name == tool).unwrap();
        ExecView {
            call: CallId("call_1".into()),
            tool: tool.into(),
            class: decl.class,
            args,
            policy: PolicyView {
                epoch: Epoch(1),
                allowed: ConsequenceSet::ALL,
                world: BoundWorld::host(),
                workspace: self.ws.path().display().to_string(),
                home: String::new(),
            },
            by: Principal::Agent { id: AgentId(1), turn: TurnId(1) },
        }
    }
}

async fn call(t: &ToolFormal, f: &Fixture, tool: &'static str, args: Value) -> ExecOutcome {
    let exec = t.exec(tool);
    exec(ExecRequest { view: f.view(tool, args), cancel: CancelToken::new(), progress: None }).await
}

fn done(o: ExecOutcome) -> (Value, String) {
    match o {
        ExecOutcome::Done { value, content, .. } => (value, content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n")),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn err(o: ExecOutcome) -> (String, String) {
    match o {
        ExecOutcome::Err { code, reason } => (code, reason),
        other => panic!("expected Err, got {other:?}"),
    }
}

/// Event bodies as the session would append them.
fn events(v: &Value) -> Vec<EntryKind> {
    v["events"].as_array().unwrap().iter().map(|e| serde_json::from_value(e["kind"].clone()).unwrap()).collect()
}

const SPEC_MD: &str = "# S\n\n## 6 Invariants\n\n### inv.calls.one-result\n\n- statement: a call has at most one result\n- mode: bounded(4)\n\n```tla id=inv.calls.one-result\n---- MODULE Calls ----\nVARIABLE x\nvars == <<x>>\nInit == x = 0\nNext == x' = x\n====\n```\n\n```cfg id=inv.calls.one-result\nINIT Init\nNEXT Next\nINVARIANT TypeOK\n```\n\n### inv.order.total\n\n- statement: totals add up\n\n```lean id=inv.order.total module=Spec.Order\ntheorem t : 1 + 1 = 2 := rfl\n```\n";

#[tokio::test]
async fn spec_check_by_file_uses_the_sibling_cfg_with_overrides() {
    let f = Fixture::new();
    f.write("specs/Log.tla", "---- MODULE Log ----\n====\n");
    f.write("specs/Log.cfg", "CONSTANT N = 2\nINIT Init\nNEXT Next\nINVARIANT TypeOK\n");
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "spec_check", json!({"spec": "specs/Log.tla", "constants": {"N": 3}, "bound": 4})).await);
    let seen = lk(&f.seen);
    let (spec, cfg, _) = &seen.specs[0];
    assert_eq!(spec.module, "Log");
    let cfg = cfg.clone().unwrap();
    assert_eq!(cfg.constants, [("N".to_string(), "3".to_string())], "the call's constants replace the cfg's");
    assert_eq!(cfg.invariants, ["TypeOK"]);
    assert_eq!(cfg.mode, CheckMode::Bounded(4));
    assert!(cfg.check_deadlock, "TLC's default when the cfg says nothing");
    assert_eq!(cfg.budget_ms, 1000);
    assert_eq!(v["verdict"], json!({"verdict": "pass"}));
    assert!(text.contains("Pass: no violation in every behavior of up to 5 steps") && text.contains("evidence, not proof"), "{text}");
    assert!(events(&v).is_empty(), "nothing is recorded without an invariant id");
}

#[tokio::test]
async fn spec_check_by_invariant_checks_what_spec_md_says_and_records_the_verdict() {
    let f = Fixture::new();
    f.write("spec/SPEC.md", SPEC_MD);
    let t = f.tools(None);
    let (v, _) = done(call(&t, &f, "spec_check", json!({"invariant": "inv.calls.one-result"})).await);
    let (spec, cfg, existed) = lk(&f.seen).specs[0].clone();
    assert!(existed, "the block was materialized for the checker");
    assert_eq!(spec.module, "Calls");
    assert!(spec.path.starts_with(f.ws.path().join("spec").canonicalize().unwrap()) || spec.path.starts_with(f.ws.path().join("spec")));
    assert_eq!(cfg.unwrap().mode, CheckMode::Bounded(4), "the claim's mode applies when the call gives no bound");
    assert!(!spec.path.exists(), "the scratch copy is gone after the call");
    let ev = events(&v);
    assert_eq!(
        ev,
        [EntryKind::InvariantChecked {
            id: "inv.calls.one-result".into(),
            mode: CheckMode::Bounded(5),
            verdict: Verdict::Pass,
            code_hash: Hash::ZERO, // no snapshots in this world: the code is unknown
        }]
    );

    let (code, reason) = err(call(&t, &f, "spec_check", json!({"invariant": "inv.nope"})).await);
    assert_eq!(code, "NO_FORMAL_BLOCK");
    assert!(reason.contains("not an invariant"), "{reason}");
    let (code, _) = err(call(&t, &f, "spec_check", json!({"invariant": "inv.order.total"})).await);
    assert_eq!(code, "NO_FORMAL_BLOCK", "a Lean-only claim has no TLA+ block to model-check");
}

#[tokio::test]
async fn failures_carry_the_trace_and_unsupported_modes_are_unknown_verdicts() {
    let f = Fixture::new();
    f.write("spec/SPEC.md", SPEC_MD);
    f.write("Bad.tla", "---- MODULE Bad ----\n====\n");
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "spec_check", json!({"spec": "Bad.tla"})).await);
    assert_eq!(v["verdict"]["verdict"], json!("fail"));
    assert!(
        text.starts_with("Fail: invariant Inv of Bad (counterexample ") && text.contains("State 1: <Initial predicate>\n  x = 0"),
        "{text}"
    );

    let (v, text) = done(call(&t, &f, "spec_check", json!({"invariant": "inv.calls.one-result", "mode": "inductive"})).await);
    assert!(
        matches!(&events(&v)[0], EntryKind::InvariantChecked { verdict: Verdict::Unknown { reason }, mode: CheckMode::Inductive, .. } if reason.starts_with("unsupported")),
        "{v}"
    );
    assert!(text.starts_with("Unknown for Calls"), "{text}");
}

#[tokio::test]
async fn paths_outside_the_workspace_are_refused() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("X.tla"), "---- MODULE X ----\n====\n").unwrap();
    let t = f.tools(None);
    let (code, _) = err(call(&t, &f, "spec_check", json!({"spec": outside.path().join("X.tla")})).await);
    assert_eq!(code, "OUTSIDE_WORKSPACE");
    let (code, _) = err(call(&t, &f, "spec_check", json!({"spec": "../../etc/hosts"})).await);
    assert!(code == "OUTSIDE_WORKSPACE" || code == "NOT_FOUND");
    let (code, _) = err(call(&t, &f, "spec_check", json!({})).await);
    assert_eq!(code, "BAD_ARGS");
}

#[tokio::test]
async fn spec_trace_maps_a_session_log_range() {
    let f = Fixture::new();
    f.write("formal/Log.tla", "---- MODULE Log ----\n====\n");
    let log_path = f.ws.path().join("session.jsonl");
    {
        let log = aho_log::JsonlLog::open(&log_path).unwrap();
        let sys = Principal::system("loop");
        log.append(None, sys.clone(), EntryKind::TurnStart { turn: TurnId(1) }).unwrap();
        log.append(
            None,
            sys.clone(),
            EntryKind::ToolCall { call: CallId("c".into()), tool: "t".into(), args: json!({}), class: Consequence::ReadOnly },
        )
        .unwrap();
        log.append(
            None,
            sys.clone(),
            EntryKind::ToolResult {
                call: CallId("c".into()),
                outcome: aho_log::ToolOutcome::Ok,
                value: json!(null),
                content: vec![],
                spill_ref: None,
                meta: None,
                late: false,
            },
        )
        .unwrap();
        log.append(None, sys, EntryKind::TurnEnd { turn: TurnId(1) }).unwrap();
    }
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "spec_trace", json!({"spec": "formal/Log.tla", "log": "session.jsonl", "from": 2, "to": 3})).await);
    let traces = lk(&f.seen).traces.clone();
    let actions: Vec<&str> = traces[0].iter().map(|r| r["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["Call", "Result"]);
    assert_eq!(v["mode"], json!({"mode": "trace"}));
    assert!(text.starts_with("Pass: the log is a behavior of Log"), "{text}");

    // a records file is taken as-is
    f.write("trace.jsonl", "{\"action\": \"Call\", \"id\": 1, \"call\": \"a\"}\n{\"action\": \"Result\", \"id\": 2, \"call\": \"a\"}\n");
    done(call(&t, &f, "spec_trace", json!({"spec": "formal/Log.tla", "trace": "trace.jsonl"})).await);
    assert_eq!(lk(&f.seen).traces[1].len(), 2);
    let (code, _) = err(call(&t, &f, "spec_trace", json!({"spec": "formal/Log.tla"})).await);
    assert_eq!(code, "BAD_ARGS");
}

#[tokio::test]
async fn lean_build_reports_goals_and_records_proof_mode() {
    let f = Fixture::new();
    f.write("spec/SPEC.md", SPEC_MD);
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "lean_build", json!({"module": "Spec.Order", "invariant": "inv.order.total"})).await);
    assert_eq!(lk(&f.seen).proved, ["Spec.Order"]);
    assert!(text.contains("Open goals:\n- fold_append (Aho/Fold.lean:3:2)\n    xs : List Nat\n    ⊢ fold xs = 0"), "{text}");
    assert!(
        matches!(&events(&v)[0], EntryKind::InvariantChecked { mode: CheckMode::Proof, verdict: Verdict::Unknown { .. }, code_hash, .. } if *code_hash == Hash::ZERO)
    );
}

#[tokio::test]
async fn lean_translate_returns_the_bound_module() {
    let f = Fixture::new();
    f.write("crates/demo/Cargo.toml", "[package]\nname = \"demo\"\n");
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "lean_translate", json!({"crate": "crates/demo", "fns": ["step"]})).await);
    assert_eq!(v["module"], json!("Aeneas.Demo.Demo"));
    assert_eq!(v["hash"], json!(Hash::of_json(&vec!["step".to_string()])));
    assert!(text.contains("translation hash") && text.contains("re-translate after the Rust changes"), "{text}");
}

#[tokio::test]
async fn mutation_records_each_mutant_and_vacuity() {
    let f = Fixture::new();
    f.write("Weak.tla", "---- MODULE Weak ----\n====\n");
    f.write("Strong.tla", "---- MODULE Strong ----\n====\n");
    let t = f.tools(None);
    let (v, text) = done(call(&t, &f, "invariant_mutate", json!({"id": "inv.weak", "spec": "Weak.tla", "bound": 3})).await);
    assert!(text.starts_with("⚠ Vacuous: Weak caught none of 2 mutants"), "{text}");
    let ev = events(&v);
    assert_eq!(ev.len(), 3);
    assert_eq!(ev[0], EntryKind::InvariantMutated { id: "inv.weak".into(), mutant: "drop-guard:A#1".into(), caught: false });
    assert!(
        matches!(&ev[2], EntryKind::InvariantChecked { id, mode: CheckMode::Bounded(3), verdict: Verdict::Vacuous, .. } if id == "inv.weak")
    );

    let (v, text) = done(call(&t, &f, "invariant_mutate", json!({"id": "inv.strong", "spec": "Strong.tla"})).await);
    assert!(text.starts_with("Strong caught 1 of 2 mutants"), "{text}");
    assert!(text.contains("- drop-guard:A#1 [caught]: drop-guard:A#1 changed; invariant Inv"), "{text}");
    let ev = events(&v);
    assert_eq!(ev.len(), 2, "no Vacuous verdict once a mutant is caught");
    assert_eq!(v["verdict"], json!({"verdict": "pass"}));
}

fn answering(outcome: Decided) -> (Arc<dyn Approval>, Arc<Mutex<Vec<aho_policy::ApprovalCard>>>) {
    let (a, mut rx) = ChannelApproval::new(1);
    let cards = Arc::new(Mutex::new(Vec::new()));
    let c = cards.clone();
    tokio::spawn(async move {
        while let Some((card, reply)) = rx.recv().await {
            lk(&c).push(card);
            let _ = reply.send(outcome);
        }
    });
    (Arc::new(a), cards)
}

#[tokio::test]
async fn invariant_add_registers_only_what_the_engineer_approves() {
    let f = Fixture::new();
    let args = json!({"id": "inv.log.one-result", "kind": "tla", "statement": "every call has at most one result", "covers": ["Tool/*", "crates/aho-log/src/lib.rs"], "supersedes": "inv.log.results", "why": "the old one allowed late duplicates"});

    let (approval, cards) = answering(Decided::allowed(GrantScope::Once));
    let t = f.tools(Some(approval));
    let (v, text) = done(call(&t, &f, "invariant_add", args.clone()).await);
    let card = lk(&cards)[0].clone();
    assert_eq!(card.class, Consequence::TalksToHuman);
    assert!(
        card.rendered.contains("Register invariant `inv.log.one-result` (tla)?")
            && card.rendered.contains("“every call has at most one result”")
    );
    assert!(
        card.rendered.contains("covers: Tool/*, crates/aho-log/src/lib.rs")
            && card.rendered.contains("supersedes `inv.log.results`: the old one allowed late duplicates")
    );
    assert_eq!(
        v["registered"],
        json!({"id": "inv.log.one-result", "kind": "tla", "statement": "every call has at most one result", "covers": ["Tool/*", "crates/aho-log/src/lib.rs"]})
    );
    assert_eq!(
        events(&v),
        [
            EntryKind::InvariantRegistered {
                id: "inv.log.one-result".into(),
                kind: FormalKind::Tla,
                statement: "every call has at most one result".into(),
                covers: vec!["Tool/*".into(), "crates/aho-log/src/lib.rs".into()],
            },
            EntryKind::Note { text: "inv.log.one-result supersedes inv.log.results: the old one allowed late duplicates".into() },
        ]
    );
    assert!(text.contains("it counts only once a mutant is caught"), "{text}");

    let (approval, _) = answering(Decided::rejected());
    let (code, reason) = err(call(&f.tools(Some(approval)), &f, "invariant_add", args.clone()).await);
    assert_eq!(code, "REJECTED");
    assert!(reason.contains("Do not propose it again unchanged"), "{reason}");

    let (code, _) = err(call(&f.tools(Some(Arc::new(NeverApproval))), &f, "invariant_add", args.clone()).await);
    assert_eq!(code, "APPROVAL_OFF", "a `never` channel is not even asked");
    let (code, _) = err(call(&f.tools(None), &f, "invariant_add", args.clone()).await);
    assert_eq!(code, "NO_APPROVAL");
    let (code, _) = err(call(&f.tools(None), &f, "invariant_add", json!({"id": "has space", "kind": "tla", "statement": "x"})).await);
    assert_eq!(code, "BAD_ARGS");
    let (code, _) = err(call(&f.tools(None), &f, "invariant_add", json!({"id": "inv.x", "kind": "coq", "statement": "x"})).await);
    assert_eq!(code, "BAD_ARGS");
}

#[tokio::test]
async fn cancellation_ends_the_call() {
    struct Slow;
    impl Formal for Slow {
        fn kind(&self) -> FormalKind {
            FormalKind::Tla
        }
        fn check<'a>(&'a self, _: &'a SpecRef, _: &'a CheckConfig) -> BoxFuture<'a, Result<Outcome, FormalError>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Err(FormalError::Tool("unreachable".into()))
            })
        }
        fn trace_validate<'a>(&'a self, _: &'a SpecRef, _: &'a [Value]) -> BoxFuture<'a, Result<Outcome, FormalError>> {
            Box::pin(async { Err(FormalError::Tool("no".into())) })
        }
        fn prove<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Outcome, FormalError>> {
            Box::pin(async { Err(FormalError::Tool("no".into())) })
        }
        fn translate<'a>(&'a self, _: &'a str, _: &'a [String]) -> BoxFuture<'a, Result<Translation, FormalError>> {
            Box::pin(async { Err(FormalError::Tool("no".into())) })
        }
        fn mutate<'a>(&'a self, _: &'a SpecRef, _: &'a CheckConfig) -> BoxFuture<'a, Result<MutationReport, FormalError>> {
            Box::pin(async { Err(FormalError::Tool("no".into())) })
        }
    }
    let f = Fixture::new();
    f.write("A.tla", "---- MODULE A ----\n====\n");
    let t = ToolFormal::new(
        f.deps(Formals { tla: Some(FormalHandle(Arc::new(Slow))), lean: None }, None),
        FormalToolConfig { spec_doc: "spec/SPEC.md".into(), budget_ms: 1000 },
    );
    let cancel = CancelToken::new();
    let exec = t.exec("spec_check");
    let fut = exec(ExecRequest { view: f.view("spec_check", json!({"spec": "A.tla"})), cancel: cancel.clone(), progress: None });
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = std::time::Instant::now();
    assert_eq!(fut.await, ExecOutcome::Cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let (code, reason) = err(call(&t, &f, "lean_build", json!({})).await);
    assert_eq!(code, "FORMAL_TOOL_MISSING");
    assert!(reason.contains("aho doctor formal --install lean"), "{reason}");
}

/// End to end: the real provider, TLC replayed by formal-tlc's fake (same inputs as its recording).
#[tokio::test]
async fn spec_check_through_formal_tlc() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/formal-tlc/tests/fixtures").canonicalize().unwrap();
    let proc = aho_proc::SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&fixtures)));
    let f = Fixture::new();
    std::fs::copy(fixtures.join("specs/Pass.tla"), f.ws.path().join("Pass.tla")).unwrap();
    let tlc = formal_tlc::Tlc::new(
        proc,
        aho_fs::FsWrite(f.fs()),
        formal_tlc::TlcConfig {
            jar: fixtures.join("specs/Pass.tla"),
            java: fixtures.join("fake-tlc.sh").display().to_string(),
            workers: 1,
            budget_ms: 60_000,
            max_mutants: 4,
        },
    );
    let t = ToolFormal::new(
        f.deps(Formals { tla: Some(FormalHandle(Arc::new(tlc))), lean: None }, None),
        FormalToolConfig { spec_doc: "spec/SPEC.md".into(), budget_ms: 60_000 },
    );
    let (v, text) = done(
        call(&t, &f, "spec_check", json!({"spec": "Pass.tla", "constants": {"Max": 2}, "invariants": ["Small"], "deadlock": false})).await,
    );
    assert_eq!(v["verdict"], json!({"verdict": "pass"}), "{text}");
    assert!(text.starts_with("Pass: no violation in every reachable state of this finite instance (depth 3)"), "{text}");
    let (v, text) = done(
        call(&t, &f, "spec_check", json!({"spec": "Pass.tla", "constants": {"Max": 2}, "invariants": ["Small"], "deadlock": true})).await,
    );
    assert_eq!(v["verdict"]["verdict"], json!("fail"));
    assert!(text.contains("deadlock") && text.contains("State 3:"), "{text}");
}

#[test]
fn composition_activates_the_tools_and_registers_exactly_the_manifest() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/formal-tlc/tests/fixtures").canonicalize().unwrap();
    let comp = Composition::new("formal-test").provide::<formal_tlc::FormalTlc>().consume::<ToolFormal>();
    let mut schema = aho_config::Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let inputs = aho_config::ResolveInputs {
        sets: vec![("formal.tla.jar".into(), fixtures.join("specs/Pass.tla").display().to_string())],
        ..Default::default()
    };
    let loadout = aho_config::resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("java".into());
    let policy = PolicyInput {
        preset: "spec".into(),
        allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::SpawnsProcess, Consequence::TalksToHuman]),
    };
    let plan = plan(&comp.manifests(), &loadout, &env, &policy, vec![]).unwrap();
    assert!(matches!(plan.row("tool-formal").unwrap().state, aho_log::CapState::Active), "{:?}", plan.row("tool-formal"));
    assert!(matches!(plan.row("formal-tlc").unwrap().state, aho_log::CapState::Active), "{:?}", plan.row("formal-tlc"));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let mut seed = Registry::default();
    let host: Arc<dyn std::any::Any + Send + Sync> =
        Arc::new(aho_proc::SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&fixtures))));
    seed.handles.insert(HandleKind::Subprocess, (CapabilityId { name: "test-world", major: 0 }, host));
    let fs: Arc<dyn aho_fs::Fs> = Arc::new(world_host::HostFs::new(&fixtures).unwrap());
    let world = CapabilityId { name: "test-world", major: 0 };
    seed.handles.insert(HandleKind::FsRead, (world, Arc::new(aho_fs::FsRead(fs.clone()))));
    seed.handles.insert(HandleKind::FsWrite, (world, Arc::new(aho_fs::FsWrite(fs))));
    seed.handles
        .insert(HandleKind::Approval, (CapabilityId { name: "aho-session", major: 0 }, Arc::new(ApprovalHandle(Arc::new(NeverApproval)))));
    let (reg, broken) = comp.start_seeded(&plan, &loadout, seed);
    assert!(broken.is_empty(), "{broken:?}");
    let mut names: Vec<&str> = reg.tools.iter().map(|(_, d, _)| d.name).collect();
    names.sort();
    assert_eq!(names, ["invariant_add", "invariant_mutate", "lean_build", "lean_translate", "spec_check", "spec_trace"]);
    assert!(reg.service::<FormalHandle>("formal:tla").is_some());

    // without any formal backend the tools are locked, not broken
    let lonely = Composition::new("no-backend").consume::<ToolFormal>();
    let p = aho_cap::plan(&lonely.manifests(), &loadout, &env, &policy, vec![]).unwrap();
    assert!(matches!(&p.row("tool-formal").unwrap().state, aho_log::CapState::Locked { .. }));
}
