//! Stale verdicts end to end in a live session: a verdict older than the code is `Stale`, and
//! a verify gate cannot pass while a covering invariant is `Stale`. The session checkpoints a
//! write, `spec_check` runs real TLC and binds its verdict to that workspace tree, and
//! `aho_inspect::current_invariants` shows Pass. A further edit is checkpointed: the same verdict
//! reads Stale and the swarm's verify gate refuses. Re-checking the current code makes it Pass
//! again, and the gate passes.
//!
//! Needs `java` and `tla2tools.jar` (`AHO_TLA_JAR`, `target/tmp/tla2tools.jar` or
//! `~/.aho/tools/tla2tools.jar`); prints why it skipped otherwise.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{EntryKind, LogStore, MemoryLog, ToolOutcome, Transition, Verdict};
use aho_session::{Boot, Session};
use aho_types::{Epoch, SessionId};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::{Value, json};

const INV: &str = "inv.counter.bounded";
const SPEC: &str =
    "---- MODULE Counter ----\nEXTENDS Naturals\nVARIABLE x\nInit == x = 0\nNext == x < 3 /\\ x' = x + 1\nInv == x <= 3\n====\n";
const CFG: &str = "INIT Init\nNEXT Next\nINVARIANT Inv\nCHECK_DEADLOCK FALSE\n";

fn jar() -> Option<PathBuf> {
    [
        std::env::var_os("AHO_TLA_JAR").map(PathBuf::from),
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/tmp/tla2tools.jar")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aho/tools/tla2tools.jar")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

fn call(tool: &str, args: Value) -> Step {
    Step::tool_call(tool, args).with_stop(ScriptedStop::ToolUse)
}

/// `(tool, ok, text)` of every result, in log order.
fn results(log: &dyn LogStore) -> Vec<(String, bool, String)> {
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let tool_of = |c: &aho_types::CallId| {
        entries.iter().find_map(|e| match &e.kind {
            EntryKind::ToolCall { call, tool, .. } if call == c => Some(tool.clone()),
            _ => None,
        })
    };
    entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolResult { call, outcome, content, .. } => Some((
                tool_of(call).unwrap_or_default(),
                matches!(outcome, ToolOutcome::Ok),
                content.iter().filter_map(|b| b.as_text()).collect::<String>(),
            )),
            _ => None,
        })
        .collect()
}

fn row(s: &Session) -> aho_inspect::InvariantRow {
    aho_inspect::current_invariants(&s.fold().unwrap()).into_iter().find(|r| r.id == INV).expect("the check was recorded")
}

#[tokio::test]
async fn a_checked_invariant_goes_stale_when_the_code_moves_and_the_verify_gate_follows() {
    let Some(jar) = jar() else {
        eprintln!("skipped: no tla2tools.jar (set AHO_TLA_JAR, or run `cargo test -p formal-tlc --test real_tlc -- --ignored` once)");
        return;
    };
    if !std::process::Command::new("java").arg("-version").output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skipped: `java` does not run on this host, and TLC needs a Java runtime");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("Counter.tla"), SPEC).unwrap();
    std::fs::write(root.join("Counter.cfg"), CFG).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();

    let comp = Composition::new("stale")
        .provide::<world_host::HostWorld>()
        .provide::<formal_tlc::FormalTlc>()
        .consume::<tool_fs::ToolFs>()
        .consume::<tool_formal::ToolFormal>()
        .consume::<tool_tasks::ToolTasks>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 120000\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n\
         [policy]\npreset = \"workspace\"\n[formal.tla]\njar = \"{}\"\n",
        root.display(),
        home.join("shadow").display(),
        jar.display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    env.binaries.insert("java".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&aho_cap::BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();

    let check = call("spec_check", json!({"spec": "Counter.tla", "invariant": INV}));
    let gate = call(
        "task_complete",
        json!({"id": "p/verify", "artifact": {
            "findings": "the counter stays in range", "confidence": "high", "what_i_did_not_check": ["overflow"], "covers": ["k"],
        }}),
    );
    let steps = vec![
        // 1: code, then a check of it
        call("write", json!({"path": "src/counter.rs", "content": "pub fn next(x: u8) -> u8 { x + 1 }\n"})),
        check.clone(),
        Step::text("checked").with_stop(ScriptedStop::EndTurn),
        // 2: more code, then the swarm's verify gate
        call("write", json!({"path": "src/bound.rs", "content": "pub const MAX: u8 = 3;\n"})),
        call("task_seed", json!({"tasks": [{"id": "p", "content": "bounded counter", "kind": "code"}]})),
        call("task_claim", json!({})),
        call("task_expand", json!({"id": "p", "children": [{"id": "k", "content": "write the counter", "kind": "code"}]})),
        call("task_claim", json!({})),
        call(
            "task_complete",
            json!({"id": "k", "artifact": {
                "findings": "counter and bound", "confidence": "high", "what_i_did_not_check": ["overflow"],
                "contract": {"outputs": [{"path": "src/counter.rs", "format": "rust"}], "acceptance": ["cargo test"]},
            }}),
        ),
        call("task_claim", json!({})),
        gate.clone(),
        Step::text("the gate waits for a fresh check").with_stop(ScriptedStop::EndTurn),
        // 3: check the current code, then the gate again
        check,
        gate,
        Step::text("verified").with_stop(ScriptedStop::EndTurn),
    ];
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(Arc::new(ScriptedLlm::from_steps(steps)))));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = Session::boot(Boot {
        id: SessionId(13),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None }],
        log: log.clone(),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();

    assert_eq!(s.prompt("write and check", None).await.unwrap(), Transition::Completed);
    let rs = results(&*log);
    assert!(rs[1].0 == "spec_check" && rs[1].1 && rs[1].2.starts_with("Pass"), "{rs:#?}");
    let first = row(&s);
    let tree = s.fold().unwrap().checkpoints.last().map(|c| c.1).expect("the write was checkpointed");
    assert_eq!((first.verdict.clone(), first.stale, first.code_hash), (Verdict::Pass, false, tree), "bound to the checked tree: {first:?}");

    assert_eq!(s.prompt("more code, then verify", None).await.unwrap(), Transition::Completed);
    let now = row(&s);
    assert!(now.stale && now.verdict == Verdict::Stale && now.recorded == Verdict::Pass, "the code moved past the check: {now:?}");
    let rs = results(&*log);
    let (tool, ok, why) = rs.last().unwrap();
    assert!(tool == "task_complete" && !ok && why.contains(&format!("`{INV}` is Stale")), "{rs:#?}");

    assert_eq!(s.prompt("check again, then verify", None).await.unwrap(), Transition::Completed);
    let again = row(&s);
    assert_eq!((again.verdict.clone(), again.stale), (Verdict::Pass, false), "{again:?}");
    let rs = results(&*log);
    let (tool, ok, text) = rs.last().unwrap();
    assert!(tool == "task_complete" && *ok && text.contains(&format!("Invariants covering it: {INV} (pass).")), "{rs:#?}");
}
