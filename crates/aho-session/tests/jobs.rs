//! Session jobs end to end: a scripted model, the host world, and test tools whose bodies sleep.
//! Detach (`background`, `after:<dur>`, operator), delivery at request boundaries, abort scopes,
//! `Interrupt::Cancel`, restart recovery and rewind. Every scenario also proves replay equality.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Request, Response, Sink};
use aho_log::{
    CancelReason, DetachCause, Entry, EntryKind, JsonlLog, LogStore, MemoryLog, OpKind, OpStatus, RewindMode, ToolOutcome, Transition,
};
use aho_session::{AbortScope, Boot, Session, SessionError, SessionEvent, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

// ── test tools: each sleeps `ms` (honouring cancel), then answers `text` and the args it saw ──

struct JobTools;
static JOB_TOOLS: &[ToolDecl] = &[
    ToolDecl::new("sleep", "Sleeps.", Consequence::ReadOnly).safe().schema(schema).detachable().op(OpKind::Timer, false, false),
    ToolDecl::new("redo", "Sleeps; re-runnable.", Consequence::ReadOnly).safe().schema(schema).detachable().op(OpKind::Timer, true, false),
    ToolDecl::new("watch", "Waits unless interrupted.", Consequence::ReadOnly).safe().schema(schema).cancel_on_interrupt(),
    ToolDecl::new("nap", "Sleeps; never a job.", Consequence::ReadOnly).safe().schema(schema),
];

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {"ms": {"type": "integer"}, "text": {"type": "string"}}}))
}

fn body() -> ToolExec {
    Arc::new(|req: ExecRequest| {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(req.view.args["ms"].as_u64().unwrap_or(0));
            while tokio::time::Instant::now() < deadline {
                if req.cancel.is_cancelled() {
                    return ExecOutcome::Cancelled;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            ExecOutcome::text(format!("{} args={}", req.view.args["text"].as_str().unwrap_or(""), req.view.args))
        })
    })
}

impl Capability for JobTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(JobTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        for d in JOB_TOOLS {
            r.tool(d, body());
        }
    }
}
declare_capability!(JobTools, "job-tools", kind = Kind::Tool, provides = [], requires = [], tools = JOB_TOOLS);

/// The scripted model, recording the tool schemas each request offered; `.2` = `(n, ms)`: the
/// n-th request (from 0) fails as rate limited with that retry delay, consuming no step.
struct Recording(ScriptedLlm, Mutex<Vec<Vec<ToolSpec>>>, Mutex<Option<(usize, u64)>>);

impl Llm for Recording {
    fn info(&self) -> AdapterInfo {
        self.0.info()
    }
    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        let n = {
            let mut seen = self.1.lock().unwrap_or_else(PoisonError::into_inner);
            seen.push(req.tools.clone());
            seen.len() - 1
        };
        let mut limit = self.2.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, ms)) = *limit
            && at == n
        {
            *limit = None;
            return Box::pin(async move { Err(LlmError::RateLimited { retry_after_ms: ms }) });
        }
        self.0.stream(req, sink)
    }
}

// ── rig ──

struct Rig {
    comp: Composition,
    plan: Plan,
    loadout: aho_config::Loadout,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    dir: tempfile::TempDir,
}

fn rig(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp = Composition::new("jobs-test").provide::<world_host::HostWorld>().consume::<JobTools>();
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
    let allowed = aho_policy::preset("workspace").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, dir }
}

async fn try_boot(r: &Rig, steps: Vec<Step>, log: Arc<dyn LogStore>) -> Result<(Arc<Session>, Arc<Recording>), SessionError> {
    let llm = Arc::new(Recording(ScriptedLlm::from_steps(steps), Mutex::default(), Mutex::default()));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let s = Session::boot(Boot {
        id: SessionId(11),
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
    .await?;
    Ok((s, llm))
}

async fn boot(r: &Rig, steps: Vec<Step>) -> (Arc<Session>, Arc<Recording>) {
    try_boot(r, steps, Arc::new(MemoryLog::new())).await.unwrap()
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

fn replay_ok(s: &Session) -> usize {
    verify_replay(&entries(s), &Default::default(), LateResults::SameCallId).unwrap()
}

fn text(content: &[ContentBlock]) -> String {
    content.iter().filter_map(|b| b.as_text()).collect()
}

/// `(op, call, cause)` of every `OpDetached`.
fn detached(s: &Session) -> Vec<(OpId, CallId, DetachCause)> {
    entries(s)
        .into_iter()
        .filter_map(|e| if let EntryKind::OpDetached { op, call, cause } = e.kind { Some((op, call, cause)) } else { None })
        .collect()
}

/// `(op, status, content text)` of every `JobDelivered`.
fn delivered(s: &Session) -> Vec<(OpId, OpStatus, String)> {
    entries(s)
        .into_iter()
        .filter_map(|e| {
            if let EntryKind::JobDelivered { op, status, content, .. } = e.kind { Some((op, status, text(&content))) } else { None }
        })
        .collect()
}

fn result_of(s: &Session, call: &CallId) -> (ToolOutcome, String) {
    entries(s)
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::ToolResult { call: c, outcome, content, .. } if &c == call => Some((outcome, text(&content))),
            _ => None,
        })
        .unwrap()
}

fn call_of(s: &Session, tool: &str) -> CallId {
    entries(s)
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::ToolCall { call, tool: t, .. } if t == tool => Some(call),
            _ => None,
        })
        .unwrap()
}

/// The first op started for `call`.
fn op_of(s: &Session, call: &CallId) -> OpId {
    entries(s)
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::OpStarted { op, call: c, .. } if &c == call => Some(op),
            _ => None,
        })
        .unwrap()
}

/// Every `JobDelivered` sits in a step's preamble: the loop entry before it is that step's
/// `StepStart` (no request in flight, no call waiting on its result), and the one after it is
/// that step's request.
fn deliveries_sit_at_boundaries(s: &Session) {
    let loop_entry = |e: &&Entry| {
        matches!(
            e.kind,
            EntryKind::TurnStart { .. }
                | EntryKind::StepStart { .. }
                | EntryKind::StepEnd { .. }
                | EntryKind::RequestHeader { .. }
                | EntryKind::AssistantMessage { .. }
                | EntryKind::ToolCall { .. }
        )
    };
    let es = entries(s);
    for (i, e) in es.iter().enumerate() {
        if !matches!(e.kind, EntryKind::JobDelivered { .. }) {
            continue;
        }
        let before = es[..i].iter().rev().find(loop_entry).unwrap();
        assert!(matches!(before.kind, EntryKind::StepStart { .. }), "a delivery opens a step, not after {:?}", before.kind);
        let after = es[i + 1..].iter().find(loop_entry);
        assert!(after.is_none_or(|a| matches!(a.kind, EntryKind::RequestHeader { .. })), "then that step's request: {after:?}");
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn job_status(s: &Session, op: OpId) -> Option<OpStatus> {
    s.jobs().unwrap().into_iter().find(|j| j.op == op).and_then(|j| j.status)
}

// ── scenarios ──

#[tokio::test]
async fn a_background_call_answers_with_its_job_and_the_outcome_lands_at_the_next_boundary() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 300, "text": "bg-done", "background": true}))
                .with_call(None, "nap", json!({"ms": 900, "text": "nap-done"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("both are in").expect_user("bg-done"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);

    let call = call_of(&s, "sleep");
    let op = op_of(&s, &call);
    let (outcome, shown) = result_of(&s, &call);
    assert_eq!(outcome, ToolOutcome::Ok);
    assert!(shown.contains(&format!("background as job {}", op.0)), "{shown}");
    assert_eq!(detached(&s), vec![(op, call.clone(), DetachCause::Requested)]);
    let es = entries(&s);
    let at = es.iter().position(|e| matches!(&e.kind, EntryKind::OpDetached { .. })).unwrap();
    assert!(
        matches!(&es[at + 1].kind, EntryKind::ToolResult { call: c, .. } if *c == call),
        "the call's result follows its detach at once"
    );

    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].0, &got[0].1), (op, &OpStatus::Completed { code: 0 }));
    assert!(got[0].2.contains("bg-done") && !got[0].2.contains("background"), "the body never sees `background`: {}", got[0].2);
    let done = es.iter().position(|e| matches!(&e.kind, EntryKind::OpDone { op: o, .. } if *o == op)).unwrap();
    assert!(matches!(es[done - 1].kind, EntryKind::JobDelivered { .. }), "OpDone follows its delivery");
    deliveries_sit_at_boundaries(&s);

    let offered = llm.1.lock().unwrap_or_else(PoisonError::into_inner)[0].clone();
    let props = |name: &str| offered.iter().find(|t| t.name == name).unwrap().input_schema.0["properties"].clone();
    assert_eq!(props("sleep")["background"]["type"], "boolean", "a detachable tool offers `background`");
    assert!(props("nap").get("background").is_none(), "a tool that is never a job does not");
    let st = s.fold().unwrap();
    assert!(st.open_ops.is_empty() && st.jobs.is_empty() && st.open_calls.is_empty());
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn a_job_that_finishes_while_idle_is_announced_and_delivered_when_the_next_turn_starts() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 1500, "text": "idle-done", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("started").expect_tool_result("background as job"),
            Step::text("got it").expect_user("idle-done"),
        ],
    )
    .await;
    let mut events = s.subscribe();
    assert_eq!(s.prompt("start it", None).await.unwrap(), Transition::Completed, "the turn ends while its job still runs");
    let op = op_of(&s, &call_of(&s, "sleep"));
    let st = s.fold().unwrap();
    assert!(st.open_calls.is_empty() && st.jobs.contains_key(&op) && st.open_ops.contains_key(&op));
    assert_eq!(s.jobs().unwrap()[0].status, None, "running");

    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::JobFinished { op }) = events.recv().await {
                return op;
            }
        }
    })
    .await
    .expect("an idle session announces the finished job");
    assert_eq!(finished, op);
    assert_eq!(job_status(&s, op), Some(OpStatus::Completed { code: 0 }), "finished, waiting for delivery");
    assert!(delivered(&s).is_empty(), "nothing is logged while no turn runs");

    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);
    let es = entries(&s);
    let d = es.iter().position(|e| matches!(e.kind, EntryKind::JobDelivered { .. })).unwrap();
    assert!(matches!(es[d - 1].kind, EntryKind::StepStart { step: StepId(1) }), "it opens the next turn's first step");
    assert!(matches!(&es[d - 2].kind, EntryKind::UserMessage { text, .. } if text == "what happened?"), "after the turn's prompt");
    assert!(matches!(es[d - 3].kind, EntryKind::TurnStart { turn: TurnId(2) }));
    assert!(matches!(es[d + 1].kind, EntryKind::OpDone { .. }));
    deliveries_sit_at_boundaries(&s);
    assert!(s.jobs().unwrap().is_empty());
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn after_a_configured_duration_a_running_call_becomes_a_job() {
    let r = rig("[jobs.detach]\nsleep = \"after:200ms\"\n");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 900, "text": "late-done"})).with_stop(ScriptedStop::ToolUse),
            Step::text("moved on").expect_tool_result("background as job"),
            Step::text("now I know").expect_user("late-done"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let call = call_of(&s, "sleep");
    let op = op_of(&s, &call);
    assert_eq!(detached(&s), vec![(op, call, DetachCause::After(200))]);
    assert_eq!(job_status(&s, op), None, "the turn completed while the promoted op still runs");
    eventually("the job to finish", || job_status(&s, op).is_some()).await;
    assert_eq!(s.prompt("and?", None).await.unwrap(), Transition::Completed);
    assert_eq!(delivered(&s).len(), 1);
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn the_operator_detaches_a_waited_on_call_and_the_turn_completes() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("nap", json!({"ms": 3000}))
                .with_call(None, "sleep", json!({"ms": 3000, "text": "x"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("fine").expect_tool_result("background as job"),
            Step::text("nap is gone").expect_tool_result("aborted"),
        ],
    )
    .await;
    let turn = tokio::spawn({
        let s = s.clone();
        async move { s.prompt("go", None).await }
    });
    eventually("both ops to run", || s.ops().running().len() == 2).await;
    let sleep = s.ops().list().into_iter().find(|i| i.tool == "sleep").unwrap().op;
    let nap = s.ops().list().into_iter().find(|i| i.tool == "nap").unwrap().op;
    assert!(matches!(s.detach(nap, DetachCause::Operator), Err(SessionError::Refused(_))), "a tool that is never a job");
    s.detach(sleep, DetachCause::Operator).unwrap();
    eventually("the driver to release the call", || !detached(&s).is_empty()).await;
    assert_eq!(detached(&s)[0].2, DetachCause::Operator);
    eventually("the model to hear about the job", || llm.0.remaining() == 1).await;
    s.abort_scope(AbortScope::Op(nap));
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), turn).await.unwrap().unwrap().unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);
    assert_eq!(job_status(&s, sleep), None, "the turn completed; the job still runs");
    s.abort_scope(AbortScope::All);
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn abort_turn_leaves_jobs_running_and_abort_all_cancels_them() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 5000, "text": "a", "background": true}))
                .with_call(None, "nap", json!({"ms": 5000}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("noted").expect_user("cancelled"),
        ],
    )
    .await;
    let turn = tokio::spawn({
        let s = s.clone();
        async move { s.prompt("go", None).await }
    });
    eventually("the turn to wait on nap", || !detached(&s).is_empty() && s.ops().running().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.abort();
    let t = tokio::time::timeout(Duration::from_secs(5), turn).await.unwrap().unwrap().unwrap();
    assert!(matches!(t, Transition::Aborted(_)), "{t:?}");
    let job = detached(&s)[0].0;
    let nap = call_of(&s, "nap");
    assert!(matches!(result_of(&s, &nap).0, ToolOutcome::Cancelled { .. }), "the turn's own op is cancelled");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(job_status(&s, job), None, "abort(Turn) leaves the job running");

    assert!(s.abort_scope(AbortScope::All));
    eventually("the job to stop", || job_status(&s, job).is_some()).await;
    assert_eq!(job_status(&s, job), Some(OpStatus::Cancelled { reason: CancelReason::Aborted }));
    assert_eq!(s.prompt("status?", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);
    assert_eq!(delivered(&s), vec![(job, OpStatus::Cancelled { reason: CancelReason::Aborted }, "Error: tool call aborted".into())]);
    deliveries_sit_at_boundaries(&s);
    assert!(s.fold().unwrap().open_ops.is_empty());
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn abort_op_cancels_one_job_only() {
    let r = rig("");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 5000, "text": "a", "background": true}))
                .with_call(None, "sleep", json!({"ms": 600, "text": "b", "background": true}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("two jobs"),
            Step::text("one cancelled, one done"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let jobs: Vec<OpId> = detached(&s).into_iter().map(|d| d.0).collect();
    assert_eq!(jobs.len(), 2);
    assert!(s.abort_scope(AbortScope::Op(jobs[0])));
    eventually("both jobs to finish", || jobs.iter().all(|j| job_status(&s, *j).is_some())).await;
    assert_eq!(job_status(&s, jobs[0]), Some(OpStatus::Cancelled { reason: CancelReason::Aborted }));
    assert_eq!(job_status(&s, jobs[1]), Some(OpStatus::Completed { code: 0 }));
    assert_eq!(s.prompt("status?", None).await.unwrap(), Transition::Completed);
    let mut got: Vec<(OpId, OpStatus)> = delivered(&s).into_iter().map(|d| (d.0, d.1)).collect();
    got.sort_by_key(|d| d.0);
    assert_eq!(got, vec![(jobs[0], OpStatus::Cancelled { reason: CancelReason::Aborted }), (jobs[1], OpStatus::Completed { code: 0 })]);
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn input_interrupts_cancel_on_interrupt_turn_ops_only() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("watch", json!({"ms": 10000, "text": "w"}))
                .with_call(None, "nap", json!({"ms": 1800, "text": "nap-done"}))
                .with_call(None, "sleep", json!({"ms": 1000, "text": "job-done", "background": true}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("heard you").expect_user("stop waiting"),
            // the interrupted call's result lands after that request: a late result
            Step::text("watch is done").expect_tool_result("Interrupted by operator input"),
            // nap lands last; the job finished before it and is delivered at that boundary
            Step::text("all in").expect_user("job-done"),
        ],
    )
    .await;
    let turn = tokio::spawn({
        let s = s.clone();
        async move { s.prompt("go", None).await }
    });
    eventually("the turn to wait", || s.ops().running().len() == 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.steer("stop waiting", false);
    assert_eq!(tokio::time::timeout(Duration::from_secs(8), turn).await.unwrap().unwrap().unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);

    let watch = call_of(&s, "watch");
    let (outcome, shown) = result_of(&s, &watch);
    assert_eq!(outcome, ToolOutcome::Ok, "an interrupted call is not an error");
    assert_eq!(shown, "Interrupted by operator input before it finished.");
    let watch_op = op_of(&s, &watch);
    assert!(entries(&s).iter().any(|e| matches!(&e.kind,
        EntryKind::OpDone { op, status: OpStatus::Cancelled { reason: CancelReason::UserInterrupted } } if *op == watch_op)));
    assert!(result_of(&s, &call_of(&s, "nap")).1.contains("nap-done"), "an op that blocks is untouched");
    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, OpStatus::Completed { code: 0 }, "a job is never interrupted by input");
    deliveries_sit_at_boundaries(&s);
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn a_restart_re_dispatches_rerunnable_jobs_and_delivers_the_rest_as_failed() {
    let r = rig("");
    let first = r.dir.path().join("first.jsonl");
    let (s, _) = try_boot(
        &r,
        vec![
            Step::tool_call("redo", json!({"ms": 5000, "text": "redone", "background": true}))
                .with_call(None, "sleep", json!({"ms": 5000, "text": "lost", "background": true}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("both running"),
        ],
        Arc::new(JsonlLog::open(&first).unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let old: Vec<OpId> = detached(&s).into_iter().map(|d| d.0).collect();
    let (redo_old, sleep_old) = (op_of(&s, &call_of(&s, "redo")), op_of(&s, &call_of(&s, "sleep")));
    // The process "dies" here: a copy of the log is what the next boot finds.
    let crashed = r.dir.path().join("crashed.jsonl");
    std::fs::copy(&first, &crashed).unwrap();
    s.abort_scope(AbortScope::All);

    let steps = vec![Step::text("one was lost"), Step::text("the other was stopped")];
    let (s, _) = try_boot(&r, steps, Arc::new(JsonlLog::open(&crashed).unwrap())).await.unwrap();
    let st = s.fold().unwrap();
    assert!(!st.open_ops.contains_key(&redo_old), "the re-runnable job's old op is closed");
    let (redo_new, _, cause) = detached(&s).into_iter().find(|d| !old.contains(&d.0)).expect("re-dispatched as a job");
    assert_eq!(cause, DetachCause::Requested);
    assert!(redo_new.0 > old.iter().map(|o| o.0).max().unwrap(), "new op ids never reuse a logged one");
    assert!(st.jobs.contains_key(&sleep_old) && st.jobs.contains_key(&redo_new));

    assert_eq!(s.prompt("after the restart", None).await.unwrap(), Transition::Completed);
    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].0, &got[0].1), (sleep_old, &OpStatus::Failed { err: "interrupted by a restart".into() }));
    assert!(got[0].2.contains("restarted while this job was running"), "{}", got[0].2);
    s.abort_scope(AbortScope::Op(redo_new));
    eventually("the re-run job to stop", || job_status(&s, redo_new).is_some()).await;
    assert_eq!(s.prompt("and the other?", None).await.unwrap(), Transition::Completed);
    assert!(s.fold().unwrap().open_ops.is_empty());
    assert_eq!(delivered(&s)[1].0, redo_new);
    assert!(replay_ok(&s) >= 2);
}

async fn job_then_checkpoint(extra: &str) -> (Rig, Arc<Session>, OpId) {
    let r = rig(extra);
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 1200, "text": "kept", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("running"),
            Step::text("after the rewind"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let op = detached(&s)[0].0;
    let c = s.checkpoint_now().await.expect("host world provides snapshots");
    let operator = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    s.rewind(&operator, c, RewindMode::ChatOnly).await.unwrap();
    (r, s, op)
}

#[tokio::test]
async fn rewind_cancels_jobs_by_default() {
    let (_r, s, op) = job_then_checkpoint("").await;
    assert!(entries(&s).iter().any(|e| matches!(&e.kind,
        EntryKind::OpDone { op: o, status: OpStatus::Cancelled { reason: CancelReason::Rewind } } if *o == op)));
    assert!(s.fold().unwrap().jobs.is_empty());
    eventually("the cancelled job to stop", || s.ops().status(op) != Some(None)).await;
    assert_eq!(s.prompt("next", None).await.unwrap(), Transition::Completed);
    assert!(delivered(&s).is_empty(), "a rewound-away job delivers nothing");
    assert!(replay_ok(&s) >= 1);
}

#[tokio::test]
async fn rewind_keeps_jobs_the_branch_holds_when_configured() {
    let (_r, s, op) = job_then_checkpoint("[jobs]\non_rewind = \"keep\"\n").await;
    assert!(s.fold().unwrap().jobs.contains_key(&op), "the new branch still holds the job");
    assert_eq!(s.ops().status(op), Some(None), "and it keeps running");
    eventually("the kept job to finish", || job_status(&s, op).is_some()).await;
    assert_eq!(s.prompt("next", None).await.unwrap(), Transition::Completed);
    assert_eq!(delivered(&s).into_iter().map(|d| (d.0, d.1)).collect::<Vec<_>>(), vec![(op, OpStatus::Completed { code: 0 })]);
    assert!(replay_ok(&s) >= 1);
}

#[tokio::test]
async fn rewind_keep_closes_a_target_job_whose_outcome_landed_after_the_target() {
    let r = rig("[jobs]\non_rewind = \"keep\"\n");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 200, "text": "early", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("running"),
            Step::text("in").expect_user("early"),
            Step::text("after the rewind"),
        ],
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let op = detached(&s)[0].0;
    let c = s.checkpoint_now().await.expect("host world provides snapshots");
    eventually("the job to finish", || job_status(&s, op).is_some()).await;
    assert_eq!(s.prompt("and?", None).await.unwrap(), Transition::Completed);
    assert_eq!(delivered(&s).len(), 1, "setup: its outcome landed after the checkpoint");

    let operator = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    s.rewind(&operator, c, RewindMode::ChatOnly).await.unwrap();
    let st = s.fold().unwrap();
    assert!(st.jobs.is_empty() && st.open_ops.is_empty(), "nothing is left to deliver it: the new branch closes it");
    assert!(entries(&s).iter().any(|e| matches!(&e.kind,
        EntryKind::OpDone { op: o, status: OpStatus::Cancelled { reason: CancelReason::Rewind } } if *o == op)));
    assert_eq!(s.prompt("next", None).await.unwrap(), Transition::Completed);
    assert!(delivered(&s).is_empty() && s.fold().unwrap().open_ops.is_empty());
    assert!(replay_ok(&s) >= 1);
}

#[tokio::test]
async fn a_job_that_finishes_while_a_rate_limited_request_waits_lands_when_the_retry_step_starts() {
    let r = rig("");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("sleep", json!({"ms": 150, "text": "rl-done", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("heard it").expect_user("rl-done"),
        ],
    )
    .await;
    // the request after the detach is rate limited for longer than the job runs
    *llm.2.lock().unwrap_or_else(PoisonError::into_inner) = Some((1, 500));
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);
    assert_eq!(delivered(&s).len(), 1);
    deliveries_sit_at_boundaries(&s);
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn a_bad_jobs_key_refuses_to_boot_with_the_key_named() {
    let r = rig("[jobs.detach]\nsleep = \"sometimes\"\n");
    match try_boot(&r, vec![], Arc::new(MemoryLog::new())).await {
        Err(SessionError::Boot(e)) => assert!(e.contains("jobs.detach.sleep"), "{e}"),
        other => panic!("expected a boot error, got {:?}", other.map(|_| ())),
    }
}
