//! Jobs policy end to end on a scripted model: `jobs.deliver = on_wait` holds finished jobs until
//! a job-awaiting call (or the next turn), `jobs.max` refuses a detach beyond it, an operator jobs
//! lease raises the limit and its expiry cancels the jobs it covered, and `Session::wake` runs a
//! wake turn within `jobs.wake_budget`. Every scenario also proves replay equality.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{CancelReason, Entry, EntryKind, MemoryLog, OpKind, OpStatus, Transition};
use aho_session::{Boot, Session, SessionError, Woke, jobs_lease_subject, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

// ── test tools: each sleeps `ms` (honouring cancel), then answers `text` ──

struct WaitTools;
static WAIT_TOOLS: &[ToolDecl] = &[
    ToolDecl::new("sleep", "Sleeps; may run as a job.", Consequence::ReadOnly).safe().schema(schema).detachable().op(
        OpKind::Timer,
        false,
        false,
    ),
    ToolDecl::new("nap", "Sleeps; never a job.", Consequence::ReadOnly).safe().schema(schema),
    ToolDecl::new("hold", "Waits on jobs.", Consequence::ReadOnly).safe().schema(schema).awaits_jobs(),
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
            ExecOutcome::text(req.view.args["text"].as_str().unwrap_or("").to_string())
        })
    })
}

impl Capability for WaitTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(WaitTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        for d in WAIT_TOOLS {
            r.tool(d, body());
        }
    }
}
declare_capability!(WaitTools, "wait-tools", kind = Kind::Tool, provides = [], requires = [], tools = WAIT_TOOLS);

// ── rig ──

struct Rig {
    session: Arc<Session>,
    llm: Arc<ScriptedLlm>,
    _dir: tempfile::TempDir,
}

/// A session over `steps` whose tool grace window is `grace_ms`; `jobs` is the `[jobs]` table body.
async fn boot(grace_ms: u64, jobs: &str, steps: Vec<Step>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp = Composition::new("wake-test").provide::<world_host::HostWorld>().consume::<WaitTools>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = {grace_ms}\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n[jobs]\n{jobs}",
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
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let session = Session::boot(Boot {
        id: SessionId(12),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None }],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    Rig { session, llm, _dir: dir }
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

fn position(es: &[Entry], f: impl Fn(&EntryKind) -> bool) -> usize {
    es.iter().position(|e| f(&e.kind)).unwrap()
}

/// The result entry of the first call to `tool`.
fn result_at(es: &[Entry], tool: &str) -> usize {
    let call = es
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::ToolCall { call, tool: t, .. } if t == tool => Some(call.clone()),
            _ => None,
        })
        .unwrap();
    position(es, |k| matches!(k, EntryKind::ToolResult { call: c, .. } if *c == call))
}

/// `(op, status)` of every `JobDelivered`, in log order.
fn delivered(s: &Session) -> Vec<(OpId, OpStatus)> {
    entries(s)
        .into_iter()
        .filter_map(|e| if let EntryKind::JobDelivered { op, status, .. } = e.kind { Some((op, status)) } else { None })
        .collect()
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..800 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn bg(ms: u64, text: &str) -> serde_json::Value {
    json!({"ms": ms, "text": text, "background": true})
}

// ── jobs.deliver ──

#[tokio::test]
async fn on_wait_holds_a_finished_job_past_boundaries_until_a_job_awaiting_call_returns() {
    let r = boot(
        5_000,
        "deliver = \"on_wait\"\n",
        vec![
            Step::tool_call("sleep", bg(20, "bg-done"))
                .with_call(None, "nap", json!({"ms": 400, "text": "nap-done"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::tool_call("hold", json!({"ms": 10, "text": "held"})).expect_tool_result("nap-done").with_stop(ScriptedStop::ToolUse),
            Step::text("the job said bg-done").expect_user("bg-done"),
        ],
    )
    .await;
    let s = &r.session;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!(r.llm.remaining(), 0);
    let es = entries(s);
    let delivery = position(&es, |k| matches!(k, EntryKind::JobDelivered { .. }));
    let requests: Vec<usize> =
        es.iter().enumerate().filter(|(_, e)| matches!(e.kind, EntryKind::RequestHeader { .. })).map(|(i, _)| i).collect();
    assert_eq!(requests.len(), 3);
    // The job finished ~380ms before the nap: the boundary before the second request held it.
    assert!(delivery > requests[1], "held past the boundary after the nap's result");
    assert!(delivery > result_at(&es, "hold") && delivery < requests[2], "released at the boundary right after `hold` returned");
    assert_eq!(delivered(s).len(), 1);
    assert!(replay_ok(s) >= 3);
}

#[tokio::test]
async fn on_wait_hands_a_job_held_at_the_turns_end_to_the_next_turns_start() {
    let r = boot(
        5_000,
        "deliver = \"on_wait\"\nwake = \"never\"\n",
        vec![
            Step::tool_call("sleep", bg(20, "bg-done"))
                .with_call(None, "nap", json!({"ms": 300, "text": "nap-done"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("nap is done").expect_tool_result("nap-done"),
            Step::text("and the job said bg-done").expect_user("bg-done"),
        ],
    )
    .await;
    let s = &r.session;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert!(delivered(s).is_empty(), "no job-awaiting call: nothing is delivered inside the turn");
    assert_eq!(s.jobs_waiting(), 1);
    assert_eq!(s.prompt("anything else?", None).await.unwrap(), Transition::Completed);
    let es = entries(s);
    let delivery = position(&es, |k| matches!(k, EntryKind::JobDelivered { .. }));
    assert!(matches!(es[delivery - 1].kind, EntryKind::StepStart { step: StepId(1) }), "at the next turn's first step");
    assert!(matches!(&es[delivery - 2].kind, EntryKind::UserMessage { text, .. } if text == "anything else?"), "of the next turn");
    assert_eq!(r.llm.remaining(), 0);
    assert!(replay_ok(s) >= 3);
}

// ── jobs.max and the jobs lease ──

#[tokio::test]
async fn a_detach_beyond_jobs_max_is_refused_and_the_call_stays_in_its_turn() {
    let r = boot(
        5_000,
        "max = 1\n",
        vec![
            Step::tool_call("sleep", bg(800, "first-done"))
                .with_call(None, "sleep", bg(150, "second-done"))
                .with_call(None, "sleep", json!({"ms": 400, "text": "third-done"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("two ran here").expect_tool_result("third-done"),
        ],
    )
    .await;
    let s = r.session.clone();
    let turn = tokio::spawn({
        let s = s.clone();
        async move { s.prompt("go", None).await }
    });
    eventually("three ops to run", || s.ops().list().len() == 3).await;
    let third = s.ops().list().into_iter().max_by_key(|i| i.op.0).unwrap().op;
    match s.detach(third, aho_log::DetachCause::Operator) {
        Err(SessionError::Refused(why)) => assert!(why.contains("`jobs.max` is 1"), "{why}"),
        other => panic!("the operator's detach is refused at the limit: {other:?}"),
    }
    assert_eq!(turn.await.unwrap().unwrap(), Transition::Completed);
    assert_eq!(r.llm.remaining(), 0);

    let es = entries(&s);
    let jobs: Vec<OpId> = es.iter().filter_map(|e| if let EntryKind::OpDetached { op, .. } = e.kind { Some(op) } else { None }).collect();
    assert_eq!(jobs.len(), 1, "only the first call became a job");
    let second = es
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolResult { content, .. } => Some(text(content)),
            _ => None,
        })
        .find(|t| t.contains("second-done"));
    assert!(second.is_some(), "the refused call answered with its own result in the turn");
    let note = es.iter().find_map(|e| match &e.kind {
        EntryKind::UserMessage { form: Form::Notice, text, .. } if text.starts_with("Not run in the background") => Some(text.clone()),
        _ => None,
    });
    assert!(note.as_deref().is_some_and(|n| n.contains("1 job(s) already run") && n.contains("`jobs.max` is 1")), "{note:?}");
    s.abort_scope(aho_session::AbortScope::All);
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn a_jobs_lease_raises_the_limit_and_its_expiry_cancels_the_jobs_it_covered() {
    let r = boot(
        5_000,
        "max = 1\nwake = \"never\"\n",
        vec![
            Step::tool_call("sleep", bg(10_000, "a")).with_call(None, "sleep", bg(10_000, "b")).with_stop(ScriptedStop::ToolUse),
            Step::text("both run").expect_tool_result("background as job"),
            Step::text("both were cancelled").expect_user("Background job"),
        ],
    )
    .await;
    let s = &r.session;
    assert_eq!(s.job_limit().unwrap().0, 1);
    let expires = Timestamp(Timestamp::now().0 + 1_500);
    s.append_entry(Principal::Operator, EntryKind::LeaseGranted { lease: LeaseId(1), subject: jobs_lease_subject(2), expires }).unwrap();
    let (limit, lease) = s.job_limit().unwrap();
    assert_eq!((limit, lease.map(|l| l.max)), (2, Some(2)), "the lease replaces `jobs.max`");

    assert_eq!(s.prompt("start two", None).await.unwrap(), Transition::Completed);
    let jobs: Vec<OpId> =
        entries(s).iter().filter_map(|e| if let EntryKind::OpDetached { op, .. } = e.kind { Some(op) } else { None }).collect();
    assert_eq!(jobs.len(), 2, "both calls became jobs under the lease");
    assert_eq!(s.running_jobs(), 2);

    eventually("the lease to end its jobs", || s.running_jobs() == 0).await;
    assert!(Timestamp::now().0 >= expires.0, "not before the lease expired");
    assert_eq!(s.job_limit().unwrap(), (1, None), "`jobs.max` applies again");
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);
    let expired = OpStatus::Cancelled { reason: CancelReason::LeaseExpired };
    let mut got = delivered(s);
    got.sort_by_key(|d| d.0);
    assert_eq!(got, vec![(jobs[0], expired.clone()), (jobs[1], expired)]);
    assert_eq!(r.llm.remaining(), 0);
    assert!(replay_ok(s) >= 3);
}

#[tokio::test]
async fn revoking_the_jobs_lease_cancels_its_running_jobs_at_once() {
    let r = boot(
        5_000,
        "wake = \"never\"\n",
        vec![Step::tool_call("sleep", bg(10_000, "a")).with_stop(ScriptedStop::ToolUse), Step::text("runs")],
    )
    .await;
    let s = &r.session;
    let expires = Timestamp(Timestamp::now().0 + 3_600_000);
    s.append_entry(Principal::Operator, EntryKind::LeaseGranted { lease: LeaseId(4), subject: jobs_lease_subject(3), expires }).unwrap();
    assert_eq!(s.prompt("start", None).await.unwrap(), Transition::Completed);
    assert_eq!(s.running_jobs(), 1);
    s.append_entry(Principal::Operator, EntryKind::LeaseRevoked { lease: LeaseId(4), proof_next_attempt: None }).unwrap();
    eventually("the revoked lease to end its job", || s.running_jobs() == 0).await;
    assert_eq!(s.jobs().unwrap()[0].status, Some(OpStatus::Cancelled { reason: CancelReason::LeaseExpired }));
}

// ── wake ──

#[tokio::test]
async fn wake_runs_one_turn_as_the_jobs_principal_and_stops_at_the_budget() {
    let r = boot(
        5_000,
        "wake = \"never\"\nwake_budget = 1\n",
        vec![
            Step::tool_call("sleep", bg(300, "first")).with_call(None, "sleep", bg(1_200, "second")).with_stop(ScriptedStop::ToolUse),
            Step::text("started").expect_tool_result("background as job"),
            Step::text("first is in").expect_user("first"),
            Step::text("second is in").expect_user("second"),
        ],
    )
    .await;
    let s = &r.session;
    assert_eq!(s.wake().await.unwrap(), Woke::Nothing, "nothing waits yet");
    assert_eq!(s.prompt("start both", None).await.unwrap(), Transition::Completed);
    eventually("the first job to finish", || s.jobs_waiting() == 1).await;
    assert_eq!(s.wake().await.unwrap(), Woke::Ran(Transition::Completed));
    let es = entries(s);
    let prompt = position(
        &es,
        |k| matches!(k, EntryKind::UserMessage { form: Form::Prompt, text, .. } if text.starts_with("Background jobs finished")),
    );
    assert_eq!(es[prompt].by, Principal::system("jobs"));
    assert!(matches!(es[prompt - 1].kind, EntryKind::TurnStart { turn: TurnId(2) }));
    assert!(matches!(es[prompt + 1].kind, EntryKind::StepStart { step: StepId(1) }));
    assert!(matches!(es[prompt + 2].kind, EntryKind::JobDelivered { .. }), "the wake turn's first step delivers");
    assert_eq!(s.wake_turns().unwrap(), 1);

    eventually("the second job to finish", || s.jobs_waiting() == 1).await;
    assert_eq!(s.wake().await.unwrap(), Woke::BudgetSpent { waiting: 1 });
    assert_eq!(s.fold().unwrap().turns, 2, "no turn opened past the budget");
    assert_eq!(s.prompt("and the other?", None).await.unwrap(), Transition::Completed);
    assert_eq!(s.wake_turns().unwrap(), 0, "the operator's prompt resets the budget");
    assert_eq!(delivered(s).len(), 2);
    assert_eq!(r.llm.remaining(), 0);
    assert!(replay_ok(s) >= 4);
}
