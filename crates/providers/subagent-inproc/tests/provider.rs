//! The provider on its own: fork points, tool filters, delegated policy, depth, interrupt,
//! one-shot children. Real child Sessions with a scripted model, no parent Session needed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{CancelReason, CapState, EntryKind, LogStore, MemoryLog, OpKind, OpStatus, Transition};
use aho_policy::{Operator, OperatorProof, SandboxPolicy};
use aho_subagent::{Mode, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;
use subagent_inproc::{InProc, SubagentInproc, SubagentRuntime, child_plan, descriptor_depth, fork_point};

// ── test tools: `slow` (cancel-aware) + `quick` in one capability, `lonely` in another ──

struct SlowTools;
static SLOW_TOOLS: &[ToolDecl] = &[
    ToolDecl::new("slow", "Sleeps until cancelled.", Consequence::ReadOnly).safe().detachable().op(OpKind::Timer, false, false),
    ToolDecl::new("quick", "Returns at once.", Consequence::ReadOnly).safe(),
];
impl Capability for SlowTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SlowTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &SLOW_TOOLS[0],
            Arc::new(|req: ExecRequest| {
                Box::pin(async move {
                    for _ in 0..1500 {
                        if req.cancel.is_cancelled() {
                            return ExecOutcome::Cancelled;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    ExecOutcome::text("slept")
                })
            }),
        );
        r.tool(&SLOW_TOOLS[1], Arc::new(|_| Box::pin(async { ExecOutcome::text("quick") })));
    }
}
declare_capability!(SlowTools, "slow-tools", kind = Kind::Tool, provides = [], requires = [], tools = SLOW_TOOLS);

struct Lonely;
static LONELY: &[ToolDecl] = &[ToolDecl::new("lonely", "Alone.", Consequence::ReadOnly).safe()];
impl Capability for Lonely {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Lonely)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(&LONELY[0], Arc::new(|_| Box::pin(async { ExecOutcome::text("alone") })));
    }
}
declare_capability!(Lonely, "lonely-tool", kind = Kind::Tool, provides = [], requires = [], tools = LONELY);

struct Rig {
    comp: Composition,
    plan: Plan,
    loadout: aho_config::Loadout,
    dir: tempfile::TempDir,
}

fn rig(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let comp = Composition::new("inproc-test").provide::<SubagentInproc>().consume::<SlowTools>().consume::<Lonely>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!("[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 10000\nheartbeat_s = 60\n{extra}");
    let mut inputs = ResolveInputs::default();
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
    Rig { comp, plan, loadout, dir }
}

fn runtime(r: &Rig, llm: &Arc<ScriptedLlm>, log: Arc<dyn LogStore>, depth: u32) -> SubagentRuntime {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    SubagentRuntime {
        composition: r.comp.clone(),
        plan: r.plan.clone(),
        loadout: r.loadout.clone(),
        bootstrap: vec![
            EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: r.plan.allowed, preset: "workspace".into(), approval_ref: None },
            EntryKind::Note { text: "root bootstrap".into() },
        ],
        seed,
        operator: Arc::new(|| Operator::authenticate(OperatorProof::local_tty())),
        spill_dir: r.dir.path().join("spill"),
        session_dir: None,
        session: SessionId(1),
        log,
        depth,
    }
}

fn parent_policy(r: &Rig) -> SandboxPolicy {
    SandboxPolicy::new(r.plan.allowed, BoundWorld::host(), r.dir.path().display().to_string(), r.dir.path().display().to_string(), Epoch(1))
}

fn request(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "t".into(),
        prompt: prompt.into(),
        mode: Mode::Continuable,
        persona: None,
        tool_filter: vec![],
        model: None,
        decl_cut: ConsequenceSet::ALL,
        is_async: false,
        bubble: false,
        fork_at: None,
        depth: 0,
    }
}

fn user(log: &dyn LogStore, text: &str) -> EntryId {
    log.append(None, Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: text.into(), client_key: None }).unwrap().id
}

fn turn_start(log: &dyn LogStore, n: u64) {
    log.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(n) }).unwrap();
}

fn turn_end(log: &dyn LogStore, n: u64) -> EntryId {
    log.append(None, Principal::system("loop"), EntryKind::TurnEnd { turn: TurnId(n) }).unwrap().id
}

/// turn 1 balanced; turn 2 ends with a call still open; turn 3 open.
fn parent_log() -> (Arc<MemoryLog>, EntryId, EntryId, EntryId) {
    let log = Arc::new(MemoryLog::new());
    turn_start(&*log, 1);
    user(&*log, "remember: the codeword is BLUE");
    let end1 = turn_end(&*log, 1);
    turn_start(&*log, 2);
    let msg2 = user(&*log, "call something");
    log.append(
        None,
        Principal::system("model"),
        EntryKind::ToolCall { call: CallId("c1".into()), tool: "quick".into(), args: json!({}), class: Consequence::ReadOnly },
    )
    .unwrap();
    let end2 = turn_end(&*log, 2);
    turn_start(&*log, 3);
    user(&*log, "the open turn");
    (log, end1, end2, msg2)
}

fn child_entries(p: &InProc, id: SessionId) -> Vec<aho_log::Entry> {
    let s = p.child_session(id).unwrap();
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

#[test]
fn fork_point_is_the_last_balanced_turn_end() {
    let (log, end1, _, _) = parent_log();
    assert_eq!(fork_point(&*log), Some(end1), "turn 2 ended with an open call and turn 3 is still open");
    assert_eq!(fork_point(&MemoryLog::new()), None);
}

#[tokio::test]
async fn fork_rejects_points_that_are_not_balanced_turn_ends() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![]));
    let (log, _, end2, msg2) = parent_log();
    let p = InProc::new(runtime(&r, &llm, log, 0), Arc::new(NeverApproval), 3);
    for at in [end2, msg2] {
        let mut req = request("fork");
        req.fork_at = Some(at);
        match p.spawn(req, parent_policy(&r)).await {
            Err(SubagentError::Provider(m)) => assert!(m.contains("nothing was started"), "{m}"),
            other => panic!("fork at {at:?} must be refused, got {other:?}"),
        }
    }
    assert!(p.children().is_empty());
}

#[tokio::test]
async fn fork_inherits_completed_turns_only() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("BLUE")]));
    let (log, end1, _, _) = parent_log();
    let p = InProc::new(runtime(&r, &llm, log.clone(), 0), Arc::new(NeverApproval), 3);
    assert_eq!(p.fork_point(), Some(end1));
    let mut req = request("what is the codeword?");
    req.fork_at = p.fork_point();
    let id = p.spawn(req, parent_policy(&r)).await.unwrap();
    let res = p.wait(id).await.unwrap();
    assert_eq!(res.last_message.as_deref(), Some("BLUE"));
    let es = child_entries(&p, id);
    assert!(matches!(es[0].kind, EntryKind::SubagentDescriptor { depth: 1, parent: SessionId(1), .. }));
    let texts: Vec<&str> =
        es.iter().filter_map(|e| if let EntryKind::UserMessage { text, .. } = &e.kind { Some(text.as_str()) } else { None }).collect();
    assert!(texts.contains(&"remember: the codeword is BLUE"));
    assert!(!texts.contains(&"call something") && !texts.contains(&"the open turn"));
    assert_eq!(aho_log::fold(&es, &Default::default()).unwrap().turns, 2);
}

#[tokio::test]
async fn a_fork_while_a_job_runs_starts_the_child_with_no_open_job() {
    let r = rig("");
    let parent_llm = Arc::new(ScriptedLlm::from_steps(vec![
        Step::tool_call("slow", json!({"background": true})),
        Step::text("the job runs on").expect_tool_result("background as job"),
    ]));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(parent_llm)));
    let parent = aho_session::Session::boot(aho_session::Boot {
        id: SessionId(1),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![EntryKind::PolicyEpoch {
            epoch: Epoch(1),
            allowed: r.plan.allowed,
            preset: "workspace".into(),
            approval_ref: None,
        }],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: Operator::authenticate(OperatorProof::local_tty()),
        workspace: r.dir.path().display().to_string(),
        home: r.dir.path().display().to_string(),
        spill_dir: r.dir.path().join("spill"),
        seed,
    })
    .await
    .unwrap();
    assert_eq!(parent.prompt("start a job", None).await.unwrap(), Transition::Completed);
    let job = parent.jobs().unwrap()[0].op;

    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("forked")]));
    let p = InProc::new(runtime(&r, &llm, parent.log().clone(), 0), Arc::new(NeverApproval), 3);
    let last_end = parent.log().read_from(None).iter().rev().find(|e| matches!(e.kind, EntryKind::TurnEnd { .. })).unwrap().id;
    assert_eq!(p.fork_point(), Some(last_end), "a turn that ended with only a job running is a fork point");
    let mut req = request("go on");
    req.fork_at = p.fork_point();
    let id = p.spawn(req, parent_policy(&r)).await.unwrap();
    assert_eq!(p.wait(id).await.unwrap().last_message.as_deref(), Some("forked"));
    let es = child_entries(&p, id);
    let st = aho_log::fold(&es, &Default::default()).unwrap();
    assert!(st.open_ops.is_empty() && st.jobs.is_empty(), "the child inherits no live job");
    assert!(es.iter().any(|e| matches!(&e.kind,
        EntryKind::OpDone { op, status: OpStatus::Cancelled { reason: CancelReason::Fork } } if *op == job)));
    assert_eq!(parent.jobs().unwrap()[0].status, None, "the parent's job keeps running");
    parent.abort_scope(aho_session::AbortScope::All);
}

#[tokio::test]
async fn spawned_child_runs_to_idle_under_the_delegated_policy() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("done: read-only")]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let world = BoundWorld::host();
    let ro = aho_policy::preset("read-only").unwrap().cut(&world);
    let mut req = request("look around");
    req.persona = Some("You only read.".into());
    req.decl_cut = ro;
    let policy = aho_policy::child_policy(&parent_policy(&r), ro, true, false);
    let id = p.spawn(req, policy).await.unwrap();
    assert!(id.0 < (1u128 << 53), "child ids stay JSON-safe");
    let res = p.wait(id).await.unwrap();
    assert_eq!((res.stop, res.last_message.as_deref(), res.diagnostic.as_deref()), (StopReason::EndTurn, Some("done: read-only"), None));
    let info = p.child_info(id).unwrap();
    assert_eq!(
        (info.depth, info.running, info.last_stop, info.provider.as_str()),
        (1, false, Some(StopReason::EndTurn), "subagent-inproc")
    );

    let es = child_entries(&p, id);
    assert!(matches!(&es[0].kind, EntryKind::SubagentDescriptor { persona: Some(x), .. } if x == "You only read."));
    assert!(matches!(&es[1].kind, EntryKind::Note { text } if text == "root bootstrap"), "spawned children repeat the root bootstrap");
    let epochs: Vec<(ConsequenceSet, Principal)> = es
        .iter()
        .filter_map(|e| if let EntryKind::PolicyEpoch { allowed, .. } = &e.kind { Some((*allowed, e.by.clone())) } else { None })
        .collect();
    assert_eq!(epochs, vec![(ro, Principal::system("delegation"))], "only the delegation epoch; parent ⊓ read-only, no human");
    assert!(
        es.iter().any(|e| matches!(&e.kind, EntryKind::UserMessage { form: Form::Instructions, text, .. } if text == "You only read."))
    );
    assert_eq!(p.child_session(id).unwrap().policy_view().allowed, ro);
}

#[tokio::test]
async fn depth_is_monotone_and_capped() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("deep")]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 2), Arc::new(NeverApproval), 3);
    let id = p.spawn(request("go"), parent_policy(&r)).await.unwrap();
    p.wait(id).await.unwrap();
    let s = p.child_session(id).unwrap();
    assert_eq!(descriptor_depth(&**s.log()), Some(3));

    let at_cap = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 3), Arc::new(NeverApproval), 3);
    assert!(matches!(at_cap.spawn(request("go"), parent_policy(&r)).await, Err(SubagentError::DepthLimit(3))));
    let root = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let mut req = request("go");
    req.depth = 5;
    assert!(
        matches!(root.spawn(req, parent_policy(&r)).await, Err(SubagentError::DepthLimit(3))),
        "a requested depth never lowers the inherited one"
    );
}

#[tokio::test]
async fn interrupt_stops_a_running_child_and_reports_aborted() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::tool_call("slow", json!({})), Step::text("stopped while sleeping")]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let id = p.spawn(request("sleep"), parent_policy(&r)).await.unwrap();
    let s = p.child_session(id).unwrap();
    let mut in_tool = false;
    for _ in 0..250 {
        if s.log().read_from(None).iter().any(|e| matches!(e.kind, EntryKind::OpStarted { .. })) {
            in_tool = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(in_tool);
    assert!(p.child_info(id).unwrap().running);
    p.interrupt(id).await.unwrap();
    let res = p.wait(id).await.unwrap();
    assert_eq!(res.stop, StopReason::Aborted);
    assert_eq!(res.last_message, None, "the child said nothing before it was stopped");
    assert!(res.diagnostic.unwrap_or_default().contains("interrupted"));
    assert_eq!(llm.remaining(), 1, "an interrupted child makes no further model request");
}

#[tokio::test]
async fn one_shot_children_are_dropped_after_their_result() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("once")]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let mut req = request("go");
    req.mode = Mode::OneShot;
    let id = p.spawn(req, parent_policy(&r)).await.unwrap();
    assert_eq!(p.wait(id).await.unwrap().last_message.as_deref(), Some("once"));
    assert!(p.children().is_empty());
    assert!(matches!(p.send(id, "again".into()).await, Err(SubagentError::Provider(_))));
}

#[test]
fn tool_filter_hides_unnamed_capabilities_and_denies_unnamed_tools() {
    let r = rig("[policy]\nrules = [\"ask lonely\"]\n");
    let (plan, loadout) = child_plan(&r.plan, &r.loadout, &r.comp.manifests(), &["slow".to_string()], Some("other-model")).unwrap();
    assert!(
        matches!(plan.row("lonely-tool").unwrap().state, CapState::Disabled { .. }),
        "a capability none of whose tools are named never starts"
    );
    assert!(matches!(plan.row("slow-tools").unwrap().state, CapState::Active));
    assert!(matches!(plan.row("subagent-inproc").unwrap().state, CapState::Active), "providers are untouched");
    let rules: Vec<&str> = loadout.get("policy.rules").unwrap().as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(rules, vec!["ask lonely", "deny quick"], "operator rules survive; unnamed tools are denied");
    let winner = &loadout.config["policy.rules"];
    assert!(winner.winner.render().contains("subagent tool filter") && !winner.shadowed.is_empty());
    assert_eq!(loadout.str("model.name"), Some("other-model"));
    assert_ne!(plan.hash, r.plan.hash);

    match child_plan(&r.plan, &r.loadout, &r.comp.manifests(), &["bsh".to_string()], None) {
        Err(SubagentError::Provider(m)) => assert!(m.contains("bsh") && m.contains("slow"), "{m}"),
        other => panic!("unknown tool names are refused, got {:?}", other.map(|_| ())),
    }
    assert!(matches!(
        child_plan(&r.plan, &r.loadout, &r.comp.manifests(), &["mcp__gh__search".to_string()], None),
        Err(SubagentError::Unsupported(_))
    ));
    let (same, _) = child_plan(&r.plan, &r.loadout, &r.comp.manifests(), &[], None).unwrap();
    assert_eq!(same.hash, r.plan.hash);
}

#[tokio::test]
async fn filtered_child_sees_only_the_named_tools() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![
        Step::tool_call("quick", json!({})),
        Step::text("quick was refused").expect_tool_result("deny quick"),
    ]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let mut req = request("try quick");
    req.tool_filter = vec!["slow".into()];
    let id = p.spawn(req, parent_policy(&r)).await.unwrap();
    assert_eq!(p.wait(id).await.unwrap().last_message.as_deref(), Some("quick was refused"));
    let header_tools = child_entries(&p, id)
        .into_iter()
        .find_map(|e| if let EntryKind::RequestHeader { tools_visible, .. } = e.kind { Some(tools_visible) } else { None })
        .unwrap();
    assert!(!header_tools.contains(&"lonely".to_string()), "{header_tools:?}");
    assert!(header_tools.contains(&"slow".to_string()));
}

#[tokio::test]
async fn a_childs_jobs_stop_when_the_provider_goes_with_its_session() {
    let r = rig("");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![
        Step::tool_call("slow", json!({"background": true})),
        Step::text("left it running").expect_tool_result("background as job"),
    ]));
    let p = InProc::new(runtime(&r, &llm, Arc::new(MemoryLog::new()), 0), Arc::new(NeverApproval), 3);
    let id = p.spawn(request("start something slow"), parent_policy(&r)).await.unwrap();
    assert_eq!(p.wait(id).await.unwrap().last_message.as_deref(), Some("left it running"));
    let child = p.child_session(id).unwrap();
    assert_eq!(child.jobs().unwrap()[0].status, None, "the child's run ended; its job runs on");

    drop(p);
    let mut status = None;
    for _ in 0..250 {
        status = child.jobs().unwrap()[0].status.clone();
        if status.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, Some(OpStatus::Cancelled { reason: CancelReason::Aborted }), "no child job outlives its parent session");
}
