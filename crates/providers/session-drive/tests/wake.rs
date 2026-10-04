//! The jobs wake driver on a real Session (scripted model): a job that finishes while no turn
//! runs gets a wake turn under `jobs.wake = idle`, the driver stops waking at `jobs.wake_budget`,
//! and `never` / `operator` start no turn at all. The log replays.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{Entry, EntryKind, JsonlLog, LogStore, OpKind, Transition};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session, SessionEvent, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;
use session_drive::host_jobs;

struct SleepTool;
static SLEEP: &[ToolDecl] = &[ToolDecl::new("sleep", "Sleeps, then answers.", Consequence::ReadOnly)
    .safe()
    .schema(schema)
    .detachable()
    .op(OpKind::Timer, false, false)];

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {"ms": {"type": "integer"}, "text": {"type": "string"}}}))
}

impl Capability for SleepTool {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SleepTool)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &SLEEP[0],
            Arc::new(|req: ExecRequest| {
                Box::pin(async move {
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(req.view.args["ms"].as_u64().unwrap_or(0));
                    while tokio::time::Instant::now() < deadline {
                        if req.cancel.is_cancelled() {
                            return ExecOutcome::Cancelled;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    ExecOutcome::text(req.view.args["text"].as_str().unwrap_or("").to_string())
                })
            }),
        );
    }
}
declare_capability!(SleepTool, "sleep-tool", kind = Kind::Tool, provides = [], requires = [], tools = SLEEP);

struct Rig {
    session: Arc<Session>,
    llm: Arc<ScriptedLlm>,
    _dir: tempfile::TempDir,
}

/// `jobs` is the body of the `[jobs]` table.
async fn boot(jobs: &str, steps: Vec<Step>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws")).unwrap();
    let comp = Composition::new("wake-test").consume::<SleepTool>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    let text = format!("[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 2000\n[jobs]\n{jobs}");
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let allowed = aho_policy::preset("workspace").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed },
        vec![],
    )
    .unwrap();
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    let mut reg = Registry::default();
    reg.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let log: Arc<dyn LogStore> = Arc::new(JsonlLog::open(dir.path().join("s.jsonl")).unwrap());
    let session = Session::boot(Boot {
        id: SessionId(1),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed, preset: "workspace".into(), approval_ref: None }],
        log,
        approval: Arc::new(NeverApproval),
        operator: Operator::authenticate(OperatorProof::local_tty()),
        workspace: dir.path().join("ws").display().to_string(),
        home: dir.path().display().to_string(),
        spill_dir: dir.path().join("spill"),
        seed: reg,
    })
    .await
    .unwrap();
    Rig { session, llm, _dir: dir }
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

/// Wake prompts (`System("jobs")`) on the branch.
fn wakes(es: &[Entry]) -> Vec<usize> {
    es.iter()
        .enumerate()
        .filter(|(_, e)| e.by == Principal::system("jobs") && matches!(e.kind, EntryKind::UserMessage { form: Form::Prompt, .. }))
        .map(|(i, _)| i)
        .collect()
}

async fn until(s: &Session, what: &str, f: impl Fn(&[Entry]) -> bool) {
    for _ in 0..500 {
        if f(&entries(s)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn idle(es: &[Entry]) -> bool {
    matches!(es.last().map(|e| &e.kind), Some(EntryKind::TurnEnd { .. }))
}

fn replays(s: &Session) {
    verify_replay(&entries(s), &s.registry().entry_kinds, LateResults::SameCallId).unwrap();
}

fn bg(ms: u64, text: &str) -> serde_json::Value {
    json!({"ms": ms, "text": text, "background": true})
}

#[tokio::test]
async fn a_job_that_finishes_while_no_turn_runs_gets_a_wake_turn_that_delivers_it() {
    let r = boot(
        "",
        vec![
            Step::tool_call("sleep", bg(300, "build is green")).with_stop(ScriptedStop::ToolUse),
            Step::text("started the build").expect_tool_result("background as job"),
            Step::text("the build is green").expect_user("build is green"),
        ],
    )
    .await;
    let drivers = host_jobs(r.session.clone());
    assert_eq!(drivers.running(), vec!["jobs"]);
    assert_eq!(r.session.prompt("build it in the background", None).await.unwrap(), Transition::Completed);
    until(&r.session, "the wake turn to end", |es| wakes(es).len() == 1 && idle(es)).await;

    let es = entries(&r.session);
    let at = wakes(&es)[0];
    assert!(matches!(es[at - 1].kind, EntryKind::TurnStart { turn: TurnId(2) }), "a turn of its own");
    assert!(matches!(es[at + 1].kind, EntryKind::StepStart { step: StepId(1) }), "the wake turn's first step");
    assert!(matches!(&es[at + 2].kind, EntryKind::JobDelivered { .. }), "delivers the job");
    assert!(es[at..].iter().any(
        |e| matches!(&e.kind, EntryKind::AssistantMessage { content, .. } if content.iter().any(|b| b.as_text() == Some("the build is green")))
    ));
    assert_eq!(r.llm.remaining(), 0);
    assert!(drivers.shutdown().await.is_empty());
    replays(&r.session);
}

#[tokio::test]
async fn the_wake_driver_starts_no_turn_past_the_budget_and_the_next_prompt_delivers() {
    let r = boot(
        "wake_budget = 1\n",
        vec![
            Step::tool_call("sleep", bg(300, "first")).with_call(None, "sleep", bg(1_500, "second")).with_stop(ScriptedStop::ToolUse),
            Step::text("both started").expect_tool_result("background as job"),
            Step::text("first is in").expect_user("first"),
            Step::text("second is in").expect_user("second"),
        ],
    )
    .await;
    let mut events = r.session.subscribe();
    let drivers = host_jobs(r.session.clone());
    assert_eq!(r.session.prompt("start both", None).await.unwrap(), Transition::Completed);
    until(&r.session, "the first wake turn", |es| wakes(es).len() == 1 && idle(es)).await;
    let spent = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(SessionEvent::Error(e)) = events.recv().await
                && e.contains("jobs.wake_budget")
            {
                return e;
            }
        }
    })
    .await
    .expect("the session says the budget is spent");
    assert!(spent.contains("1 finished job(s) are delivered with the next prompt"), "{spent}");
    assert_eq!(r.session.fold().unwrap().turns, 2, "no second wake turn");
    assert_eq!(r.session.jobs_waiting(), 1);

    assert_eq!(r.session.prompt("and the other one?", None).await.unwrap(), Transition::Completed);
    assert_eq!(r.session.jobs_waiting(), 0);
    assert_eq!(r.llm.remaining(), 0);
    assert!(drivers.shutdown().await.is_empty());
    replays(&r.session);
}

#[tokio::test]
async fn under_never_and_operator_a_finished_job_starts_no_turn() {
    for mode in ["never", "operator"] {
        let r = boot(
            &format!("wake = \"{mode}\"\n"),
            vec![
                Step::tool_call("sleep", bg(200, "done")).with_stop(ScriptedStop::ToolUse),
                Step::text("started").expect_tool_result("background as job"),
                Step::text("it said done").expect_user("done"),
            ],
        )
        .await;
        let drivers = host_jobs(r.session.clone());
        assert!(drivers.running().is_empty(), "{mode}: no wake driver");
        let mut events = r.session.subscribe();
        assert_eq!(r.session.prompt("start it", None).await.unwrap(), Transition::Completed);
        let finished = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(SessionEvent::JobFinished { op }) = events.recv().await {
                    return op;
                }
            }
        })
        .await
        .expect("surfaces hear that the job finished");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(r.session.fold().unwrap().turns, 1, "{mode}: no turn started for it");
        assert_eq!(r.session.jobs_waiting(), 1);

        assert_eq!(r.session.prompt("what did it say?", None).await.unwrap(), Transition::Completed);
        let es = entries(&r.session);
        assert!(
            es.iter().any(|e| matches!(&e.kind, EntryKind::JobDelivered { op, .. } if *op == finished)),
            "{mode}: delivered with the prompt"
        );
        assert!(wakes(&es).is_empty());
        assert_eq!(r.llm.remaining(), 0);
        assert!(drivers.shutdown().await.is_empty());
        replays(&r.session);
    }
}
