//! End-to-end smoke test: a scripted model calls `run_code`; the program calls `read` back
//! through the session.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{EntryKind, MemoryLog, Transition};
use aho_session::{Boot, Session, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

struct Rig {
    comp: Composition,
    plan: aho_cap::Plan,
    loadout: aho_config::Loadout,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(preset: &str, extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let comp =
        Composition::new("ptc").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().consume::<tool_run_code::ToolRunCode>();
    let mut schema = Schema::default();
    for k in comp.config_keys().into_iter().chain(aho_cap::core_keys()) {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"{preset}\"\n{extra}",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.extend(["git".to_string(), "python3".to_string()]);
    let world = aho_cap::BoundWorld::host();
    let allowed = aho_policy::preset(preset).unwrap().cut(&world);
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: preset.into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, _dir: dir }
}

async fn boot(r: &Rig, steps: Vec<Step>) -> (Arc<Session>, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let s = Session::boot(Boot {
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
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    (s, llm)
}

const PROGRAM: &str = "import json\nv = aho.call('read', {'path': 'a.txt'})\nprint('READ', 'hello from disk' in json.dumps(v))\nfor p in ['secret.txt', 'b.txt']:\n    try:\n        aho.call('read', {'path': p})\n    except aho.ToolError as e:\n        print('REFUSED', p, e.code)\n";

#[tokio::test]
async fn nested_calls_run_through_the_decision_chain_and_are_logged_as_ptc() {
    let r = rig("dev", "rules = [\"deny read(secret.txt)\", \"ask read(b.txt)\"]\n");
    for f in ["a.txt", "secret.txt", "b.txt"] {
        std::fs::write(r.root.join(f), "hello from disk\n").unwrap();
    }
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("run_code", json!({"language": "python", "code": PROGRAM})).with_stop(ScriptedStop::ToolUse),
            Step::text("done").expect_tool_result("READ True\nREFUSED secret.txt DENIED\nREFUSED b.txt APPROVAL"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    let calls: Vec<String> =
        entries.iter().filter_map(|e| if let EntryKind::ToolCall { tool, .. } = &e.kind { Some(tool.clone()) } else { None }).collect();
    assert_eq!(calls, vec!["run_code"], "nested calls must not appear as Tool/Call");
    let ptc: Vec<(String, Value)> = entries
        .iter()
        .filter_map(|e| {
            if let EntryKind::Custom { ns, name, body } = &e.kind { (ns == "Ptc").then(|| (name.clone(), body.clone())) } else { None }
        })
        .collect();
    assert_eq!(ptc.len(), 6, "{ptc:?}");
    assert_eq!(ptc[3].1["code"], json!("DENIED"));
    let approvals = entries.iter().filter(|e| matches!(e.kind, EntryKind::ApprovalRequested { .. })).count();
    assert_eq!(approvals, 1, "the `ask` rule on b.txt went to the approver");
    let state = s.fold().unwrap();
    assert_eq!(state.custom["Ptc"], json!({"calls": 3, "errors": 2, "open": []}));
    assert!(verify_replay(&entries, &Default::default(), LateResults::SameCallId).unwrap() >= 2);
}

#[tokio::test]
async fn under_the_workspace_preset_run_code_asks_for_host_write() {
    let r = rig("workspace", "");
    let (s, _llm) = boot(
        &r,
        vec![
            Step::tool_call("run_code", json!({"language": "python", "code": "print(1)"})).with_stop(ScriptedStop::ToolUse),
            Step::text("refused").expect_tool_result("Error (APPROVAL)"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    let asked = entries
        .iter()
        .find_map(|e| if let EntryKind::ApprovalRequested { reason, .. } = &e.kind { Some(reason.clone()) } else { None })
        .unwrap();
    assert!(asked.contains("HostWrite"), "{asked}");
}
