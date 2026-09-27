//! In a live session, a Verify gate cannot pass while a covering invariant is `Fail` or `Stale`.
//! A scripted model works a code task through the real loop (tool-tasks, tool-fs
//! over the host world, per-call checkpoints); the invariant covering the written file is
//! registered and checked the way tool-formal records it. The gate is refused while the check is
//! of older code (Stale) and while it fails, and passes once it passes against the current code;
//! the pass records what it was judged against.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{CheckMode, EntryKind, FormalKind, LogStore, MemoryLog, ToolOutcome, Transition, Verdict};
use aho_session::{Boot, Session};
use aho_types::{Epoch, Hash, Principal, SessionId};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::{Value, json};

struct Rig {
    comp: Composition,
    plan: aho_cap::Plan,
    loadout: aho_config::Loadout,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp =
        Composition::new("verify-gate").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().consume::<tool_tasks::ToolTasks>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 2000\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n\
         [policy]\npreset = \"workspace\"\n",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&aho_cap::BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, _dir: dir }
}

async fn boot(r: &Rig, steps: Vec<Step>, log: Arc<dyn LogStore>) -> Arc<Session> {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(Arc::new(ScriptedLlm::from_steps(steps)))));
    Session::boot(Boot {
        id: SessionId(9),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![EntryKind::PolicyEpoch {
            epoch: Epoch(1),
            allowed: r.plan.allowed,
            preset: r.plan.preset.clone(),
            approval_ref: None,
        }],
        log,
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap()
}

fn call(tool: &str, args: Value) -> Step {
    Step::tool_call(tool, args).with_stop(ScriptedStop::ToolUse)
}

/// `(tool, ok, text)` of every result, in log order.
fn results(log: &dyn LogStore) -> Vec<(String, bool, String)> {
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let tool_of = |call: &aho_types::CallId| {
        entries.iter().find_map(|e| match &e.kind {
            EntryKind::ToolCall { call: c, tool, .. } if c == call => Some(tool.clone()),
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

const INV: &str = "inv.parser.total";

/// The entry tool-formal appends for a check of `INV`, bound to `code`.
fn checked(log: &dyn LogStore, verdict: Verdict, code: Hash) {
    let kind = EntryKind::InvariantChecked { id: INV.into(), mode: CheckMode::Bounded(4), verdict, code_hash: code };
    log.append(None, Principal::system("spec_check"), kind).unwrap();
}

#[tokio::test]
async fn a_live_verify_gate_is_refused_while_its_invariant_is_stale_or_failing() {
    let r = rig();
    let gate_pass = json!({"id": "p/verify", "artifact": {
        "findings": "the lexer matches the grammar", "confidence": "high", "what_i_did_not_check": ["error recovery"], "covers": ["k"],
    }});
    let steps = vec![
        call("task_seed", json!({"tasks": [{"id": "p", "content": "port the parser", "kind": "code"}]})),
        call("task_claim", json!({})),
        call("task_expand", json!({"id": "p", "children": [{"id": "k", "content": "write the lexer", "kind": "code"}]})),
        call("task_claim", json!({})),
        call("write", json!({"path": "src/lexer.rs", "content": "pub fn lex() {}\n"})),
        call(
            "task_complete",
            json!({"id": "k", "artifact": {
                "findings": "wrote the lexer", "confidence": "high", "what_i_did_not_check": ["unicode"],
                "contract": {"outputs": [{"path": "src/lexer.rs", "format": "rust"}], "acceptance": ["cargo test -p parser"]},
            }}),
        ),
        call("task_claim", json!({})),
        call("task_complete", gate_pass.clone()),
        Step::text("the gate is waiting on the invariant").with_stop(ScriptedStop::EndTurn),
        // second prompt: the check was re-run against the current code and fails
        call("task_complete", gate_pass.clone()),
        Step::text("still failing").with_stop(ScriptedStop::EndTurn),
        // third prompt: the fix went in and the check passes
        call("task_complete", gate_pass),
        Step::text("verified").with_stop(ScriptedStop::EndTurn),
    ];
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, steps, log.clone()).await;
    // registered (the engineer approved it) and checked before any code was written
    let register = EntryKind::InvariantRegistered {
        id: INV.into(),
        kind: FormalKind::Tla,
        statement: "every token stream parses".into(),
        covers: vec!["src/**".into(), "Task/*".into()],
    };
    log.append(None, Principal::system("invariant_add"), register).unwrap();
    checked(&*log, Verdict::Pass, Hash::of(b"before any code"));

    assert_eq!(s.prompt("run the swarm", None).await.unwrap(), Transition::Completed);
    let rs = results(&*log);
    let names: Vec<(&str, bool)> = rs.iter().map(|(t, ok, _)| (t.as_str(), *ok)).collect();
    assert_eq!(
        names,
        [
            ("task_seed", true),
            ("task_claim", true),
            ("task_expand", true),
            ("task_claim", true),
            ("write", true),
            ("task_complete", true),
            ("task_claim", true),
            ("task_complete", false)
        ],
        "{rs:#?}"
    );
    let refused = &rs[7].2;
    assert!(refused.contains(&format!("`{INV}` is Stale")) && refused.contains("gate stays running"), "{refused}");
    let tree = s.fold().unwrap().checkpoints.last().map(|c| c.1).expect("the write was checkpointed");

    checked(&*log, Verdict::Fail { counterexample: Hash::of(b"cx") }, tree);
    assert_eq!(s.prompt("try the gate again", None).await.unwrap(), Transition::Completed);
    let rs = results(&*log);
    let (tool, ok, why) = &rs[8];
    assert!(tool == "task_complete" && !ok && why.contains(&format!("`{INV}` fails")), "{why}");

    checked(&*log, Verdict::Pass, tree);
    assert_eq!(s.prompt("and again", None).await.unwrap(), Transition::Completed);
    let rs = results(&*log);
    let (tool, ok, text) = &rs[9];
    assert!(tool == "task_complete" && *ok && text.contains(&format!("Invariants covering it: {INV} (pass).")), "{text}");

    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let passes: Vec<&Value> = entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Custom { ns, name, body } if ns == "Task" && name == "Completed" && body["id"] == "p/verify" => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(passes.len(), 1, "only the last attempt is a Task/Completed");
    assert_eq!(passes[0]["invariants"], json!([{"id": INV, "status": "pass"}]));
    let graph = aho_tasks::project(s.fold().unwrap().custom.get("Task")).unwrap();
    assert_eq!(graph.node(&aho_tasks::TaskId::new("p/verify")).unwrap().status, aho_tasks::Status::Completed);
}
