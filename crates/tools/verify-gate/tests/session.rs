//! The verify gate in a live session: a scripted model works through the real loop (tool-fs and
//! tool-bash over the host world, snapshots on). A turn that wrote and whose check fails is kept
//! going exactly once with the check's output tail; a passing check lets it stop; a turn that
//! changed nothing, or a disabled gate, never runs the check; a check past its timeout fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{Entry, EntryKind, LogStore, MemoryLog, ToolOutcome, Transition};
use aho_session::{Boot, Session};
use aho_types::{Epoch, Principal, SessionId};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::{Value, json};

struct Rig {
    comp: Composition,
    plan: aho_cap::Plan,
    loadout: aho_config::Loadout,
    root: PathBuf,
    home: PathBuf,
    /// Outside the workspace: one line per check run.
    runs: PathBuf,
    _dir: tempfile::TempDir,
}

/// `verify` is the `[verify]` table body; `{runs}` in it is replaced by the run-counter path.
fn rig(verify: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    let runs = dir.path().join("runs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp = Composition::new("verify-gate-test")
        .provide::<world_host::HostWorld>()
        .consume::<tool_fs::ToolFs>()
        .consume::<tool_bash::ToolBash>()
        .provide::<verify_gate::VerifyGate>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 2000\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n\
         [policy]\npreset = \"workspace\"\n[verify]\n{}\n",
        root.display(),
        home.join("shadow").display(),
        verify.replace("{runs}", &runs.display().to_string()),
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&aho_cap::BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, runs, _dir: dir }
}

async fn boot(r: &Rig, model: Arc<ScriptedLlm>, log: Arc<dyn LogStore>) -> Arc<Session> {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(model)));
    Session::boot(Boot {
        id: SessionId(7),
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

fn done(text: &str) -> Step {
    Step::text(text).with_stop(ScriptedStop::EndTurn)
}

fn write(path: &str) -> Step {
    call("write", json!({"path": path, "content": "pub fn f() {}\n"}))
}

fn entries(log: &dyn LogStore) -> Vec<Entry> {
    log.read_branch(log.head().unwrap()).unwrap()
}

/// How many times the check command ran.
fn runs(path: &Path) -> usize {
    std::fs::read_to_string(path).map(|s| s.lines().count()).unwrap_or(0)
}

/// The decision of every `verify-gate` hook run, in order (`HookResult` as the session logs it).
fn decisions(log: &dyn LogStore) -> Vec<String> {
    entries(log)
        .into_iter()
        .filter(|e| e.by == Principal::system("verify-gate"))
        .filter_map(|e| match e.kind {
            EntryKind::HookResult { decision, .. } => Some(decision),
            _ => None,
        })
        .collect()
}

fn stop_denials(log: &dyn LogStore) -> usize {
    entries(log).iter().filter(|e| matches!(e.kind, EntryKind::StepEnd { transition: Transition::StopHookBlocking, .. })).count()
}

fn notices(log: &dyn LogStore) -> Vec<String> {
    entries(log)
        .into_iter()
        .filter(|e| e.by == Principal::system("hooks"))
        .filter_map(|e| match e.kind {
            EntryKind::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

fn results_ok(log: &dyn LogStore) -> Vec<bool> {
    entries(log)
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolResult { outcome, .. } => Some(matches!(outcome, ToolOutcome::Ok)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_failing_check_after_a_write_keeps_the_turn_going_once_with_the_output_tail() {
    // Fails until `fixed.rs` exists, after printing far more than the tail keeps.
    let r = rig("command = '''echo run >> {runs}; test -f fixed.rs || { seq 1 4000; echo 'boom: fixed.rs is missing' >&2; exit 3; }'''");
    let model = Arc::new(ScriptedLlm::from_steps(vec![write("src.rs"), done("done"), write("fixed.rs"), done("fixed")]));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, model.clone(), log.clone()).await;

    assert_eq!(s.prompt("write the code", None).await.unwrap(), Transition::Completed);
    assert_eq!(model.remaining(), 0, "the denied stop gave the model another step");
    assert_eq!(results_ok(&*log), [true, true]);
    assert!(r.root.join("fixed.rs").is_file());
    assert_eq!(stop_denials(&*log), 1);

    let reason = notices(&*log).into_iter().find(|n| n.starts_with("verify: ")).expect("the failure reached the model");
    assert!(reason.starts_with("verify: echo run >> ") && reason.contains(" exited 3\n"), "{reason}");
    assert!(reason.trim_end().ends_with("boom: fixed.rs is missing"), "the tail ends where the output ends: {reason}");
    assert!(reason.contains("earlier bytes cut]") && !reason.contains("\n1\n2\n3\n"), "the head of a long output is cut: {reason}");
    assert!(reason.len() < verify_gate::TAIL_BYTES + 400, "bounded: {} bytes", reason.len());

    // The stop after the denial is not verified again (the session would not honor a second denial).
    assert_eq!(runs(&r.runs), 1);
    let d = decisions(&*log);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d[0].starts_with("Deny(\"verify: "), "{d:?}");
    assert_eq!(d[1], "none");
}

#[tokio::test]
async fn a_passing_check_lets_the_turn_complete() {
    let r = rig("command = '''echo run >> {runs}; exit 0'''");
    let model = Arc::new(ScriptedLlm::from_steps(vec![write("src.rs"), done("done")]));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, model.clone(), log.clone()).await;

    assert_eq!(s.prompt("write the code", None).await.unwrap(), Transition::Completed);
    assert_eq!(runs(&r.runs), 1);
    assert_eq!(stop_denials(&*log), 0);
    assert_eq!(decisions(&*log), ["Allow"]);
}

#[tokio::test]
async fn a_turn_that_changed_nothing_never_runs_the_check() {
    let r = rig("command = '''echo run >> {runs}; exit 1'''");
    std::fs::write(r.root.join("notes.txt"), "hello\n").unwrap();
    let model = Arc::new(ScriptedLlm::from_steps(vec![
        // turn 1: read-only calls only
        call("read", json!({"path": "notes.txt"})),
        done("read it"),
        // turn 2: a process call that leaves the tree as the last turn ended
        call("bash", json!({"command": "ls"})),
        done("listed"),
    ]));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, model.clone(), log.clone()).await;

    assert_eq!(s.prompt("look", None).await.unwrap(), Transition::Completed);
    assert_eq!(s.prompt("list", None).await.unwrap(), Transition::Completed);
    assert_eq!(results_ok(&*log), [true, true]);
    assert_eq!(runs(&r.runs), 0);
    assert_eq!(stop_denials(&*log), 0);
    assert_eq!(decisions(&*log), ["none", "none"]);
}

#[tokio::test]
async fn a_disabled_gate_never_runs() {
    let r = rig("enabled = false\ncommand = '''echo run >> {runs}; exit 1'''");
    let model = Arc::new(ScriptedLlm::from_steps(vec![write("src.rs"), done("done")]));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, model.clone(), log.clone()).await;

    assert_eq!(s.prompt("write the code", None).await.unwrap(), Transition::Completed);
    assert_eq!(runs(&r.runs), 0);
    assert_eq!(stop_denials(&*log), 0);
    assert!(decisions(&*log).is_empty());
}

#[tokio::test]
async fn a_check_past_its_timeout_fails_with_a_timeout_reason() {
    let r = rig("timeout_s = 1\ncommand = '''echo run >> {runs}; echo started; sleep 30'''");
    let model = Arc::new(ScriptedLlm::from_steps(vec![write("src.rs"), done("done"), done("still done")]));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = boot(&r, model.clone(), log.clone()).await;

    let t0 = std::time::Instant::now();
    assert_eq!(s.prompt("write the code", None).await.unwrap(), Transition::Completed);
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the check was cut off: {:?}", t0.elapsed());
    assert_eq!(model.remaining(), 0);
    assert_eq!(stop_denials(&*log), 1);
    let reason = notices(&*log).into_iter().find(|n| n.starts_with("verify: ")).expect("the timeout reached the model");
    assert!(reason.contains(" timed out after 1s\n") && reason.ends_with("started"), "{reason}");
}
