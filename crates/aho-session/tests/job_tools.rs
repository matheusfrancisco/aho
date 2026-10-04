//! Real tools as session jobs: `bash` promoted by `jobs.detach.bash = after:<dur>`, and the job
//! tools (`job_list`, `job_wait`, `job_kill`) seeing and controlling jobs, through the model loop
//! with a scripted model and the host world. Every scenario also proves replay equality.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{DetachCause, Entry, EntryKind, MemoryLog, OpStatus, Transition};
use aho_session::{AbortScope, Boot, Session, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;

struct Rig {
    comp: Composition,
    plan: aho_cap::Plan,
    loadout: aho_config::Loadout,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp = Composition::new("job-tools-test")
        .provide::<world_host::HostWorld>()
        .consume::<tool_bash::ToolBash>()
        .consume::<tool_jobs::ToolJobs>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n{extra}",
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

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

fn replay_ok(s: &Session) -> usize {
    verify_replay(&entries(s), &Default::default(), LateResults::SameCallId).unwrap()
}

fn bash(command: &str, background: bool) -> serde_json::Value {
    if background { json!({"command": command, "background": true}) } else { json!({"command": command}) }
}

async fn eventually(what: &str, ok: impl Fn() -> bool) {
    for _ in 0..500 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `(op, status, text)` of every `JobDelivered`, in log order.
fn delivered(s: &Session) -> Vec<(OpId, OpStatus, String)> {
    entries(s)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::JobDelivered { op, status, content, .. } => Some((op, status, content.iter().filter_map(|b| b.as_text()).collect())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn bash_still_running_after_its_duration_becomes_a_job_and_is_delivered() {
    let r = rig("[jobs.detach]\nbash = \"after:300ms\"\n");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("bash", bash("echo fast", false)),
            Step::text("quick one").expect_tool_result("fast"),
            Step::tool_call("bash", bash("sleep 1.5; echo built", false)),
            Step::text("it went to the background").expect_tool_result("background as job 2"),
            Step::text("the build is done").expect_user("built"),
        ],
    )
    .await;
    assert_eq!(s.prompt("quick", None).await.unwrap(), Transition::Completed);
    assert!(s.jobs().unwrap().is_empty(), "a command done before the duration is an ordinary call");

    assert_eq!(s.prompt("build", None).await.unwrap(), Transition::Completed);
    let jobs = s.jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].op, jobs[0].tool.as_str(), jobs[0].cause), (OpId(2), "bash", DetachCause::After(300)));
    assert_eq!(jobs[0].status, None, "the turn completed while the command still ran");

    eventually("the command to finish", || s.jobs().unwrap()[0].status.is_some()).await;
    assert_eq!(s.prompt("done yet?", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].0, &got[0].1), (OpId(2), &OpStatus::Completed { code: 0 }));
    assert!(got[0].2.contains("built"), "{}", got[0].2);
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn job_list_job_kill_and_job_wait_work_on_a_job() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("bash", bash("sleep 30", true)),
            Step::tool_call("job_list", json!({})).expect_tool_result("background as job 1"),
            Step::tool_call("job_kill", json!({"op": 1})).expect_tool_result("op 1  bash  job  running"),
            Step::tool_call("job_wait", json!({"op": 1})).expect_tool_result("Cancelled job 1 (bash)"),
            Step::text("it is stopped").expect_tool_result("job 1 cancelled (aborted)"),
        ],
    )
    .await;
    assert_eq!(s.prompt("start, then stop it", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let got = delivered(&s);
    assert_eq!(got.len(), 1, "the cancellation was delivered within the turn");
    assert!(matches!(got[0].1, OpStatus::Cancelled { .. }), "{:?}", got[0].1);
    assert!(s.jobs().unwrap().is_empty());
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn job_wait_returns_when_another_job_finishes_and_its_delivery_follows() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("bash", bash("sleep 30", true)).with_call(None, "bash", bash("sleep 0.4; echo quick job", true)),
            Step::tool_call("job_wait", json!({"op": 1})).expect_tool_result("background as job"),
            Step::text("job 2 is in").expect_tool_result("While waiting on op 1, job 2 (bash) completed (exit 0)").expect_user("quick job"),
        ],
    )
    .await;
    assert_eq!(s.prompt("two jobs", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let got = delivered(&s);
    assert_eq!(got.iter().map(|d| d.0).collect::<Vec<_>>(), vec![OpId(2)], "job 2 delivered; job 1 still runs");
    assert_eq!(s.jobs().unwrap()[0].status, None);

    assert!(s.abort_scope(AbortScope::All));
    eventually("job 1 to stop", || s.jobs().unwrap()[0].status.is_some()).await;
    assert!(replay_ok(&s) >= 1);
}
