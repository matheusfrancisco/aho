//! Harness system test: a real client↔loop round trip against a scripted model, the host
//! world, and the fs/bash/jobs tools. Every scenario also proves replay equality.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::Path;
use std::sync::Arc;

use aho_approval::{ChannelApproval, Decided, NeverApproval};
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{ApprovalOutcome, EntryKind, GrantScope, JsonlLog, LogStore, MemoryLog, OpStatus, RewindMode, Transition};
use aho_session::{Boot, Session, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedError, ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

fn composition() -> Composition {
    Composition::new("system-test")
        .provide::<world_host::HostWorld>()
        .consume::<tool_fs::ToolFs>()
        .consume::<tool_bash::ToolBash>()
        .consume::<tool_jobs::ToolJobs>()
}

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
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp = composition();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
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
    env.binaries.insert("git".into());
    let world = aho_cap::BoundWorld::host();
    let allowed = aho_policy::preset(preset).unwrap().cut(&world);
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: preset.into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, _dir: dir }
}

async fn boot(
    r: &Rig,
    steps: Vec<Step>,
    log: Arc<dyn LogStore>,
    approval: Arc<dyn aho_approval::Approval>,
) -> (Arc<Session>, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let s = Session::boot(Boot {
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
        approval,
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

fn replay_ok(s: &Session) -> usize {
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    verify_replay(&entries, &Default::default(), LateResults::SameCallId).unwrap()
}

fn results(s: &Session) -> Vec<(CallId, aho_log::ToolOutcome, String, bool)> {
    s.log()
        .read_branch(s.log().head().unwrap())
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::ToolResult { call, outcome, content, late, .. } => {
                Some((call, outcome, content.iter().filter_map(|b| b.as_text()).collect::<String>(), late))
            }
            _ => None,
        })
        .collect()
}

fn write(root: &Path, name: &str, text: &str) {
    std::fs::write(root.join(name), text).unwrap();
}

#[tokio::test]
async fn tool_call_loop_reads_file() {
    let r = rig("workspace", "");
    write(&r.root, "a.txt", "hello from disk\n");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "a.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::text("it says hello").expect_tool_result("hello from disk"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("read a.txt", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    assert!(replay_ok(&s) >= 2);
}

#[tokio::test]
async fn tool_error_flows_through_loop() {
    let r = rig("workspace", "");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "missing.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::text("the file is missing").expect_tool_result("missing.txt"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("read it", None).await.unwrap(), Transition::Completed);
    assert!(matches!(results(&s)[0].1, aho_log::ToolOutcome::Err { .. }));
}

#[tokio::test]
async fn slow_call_shows_placeholder_then_late_result_wakes_the_model() {
    let r = rig("workspace", "");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("bash", json!({"command": "echo fast-done"}))
                .with_call(None, "bash", json!({"command": "sleep 1; echo slow-done"}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("fast is done, waiting for slow").expect_tool_result("still running").with_stop(ScriptedStop::EndTurn),
            Step::text("finished").expect_tool_result("slow-done"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("build", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let rs = results(&s);
    assert!(rs[0].2.contains("fast-done") && !rs[0].3, "the fast call landed inside the grace window");
    assert!(rs[1].2.contains("slow-done") && rs[1].3, "the slow result is late: a request happened after its call");
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn abort_while_waiting_on_a_call_ends_the_turn_without_another_model_request() {
    let r = rig("workspace", "");
    let (s, llm) = boot(
        &r,
        vec![
            Step::tool_call("bash", json!({"command": "sleep 5; echo never"})).with_stop(ScriptedStop::ToolUse),
            Step::text("the aborted turn must not reach this step"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    let turn = tokio::spawn({
        let s = s.clone();
        async move { s.prompt("build", None).await }
    });
    // Past the 150 ms grace window with no result to report: the loop sleeps on the running call.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(llm.remaining(), 1, "setup: the loop is waiting on the running call");
    s.abort();
    let t = tokio::time::timeout(std::time::Duration::from_secs(3), turn).await.expect("abort ends the turn promptly").unwrap().unwrap();
    assert!(matches!(t, Transition::Aborted(_)), "{t:?}");
    assert_eq!(llm.remaining(), 1, "no model request after abort");
    let ends: Vec<Transition> = s
        .log()
        .read_branch(s.log().head().unwrap())
        .unwrap()
        .into_iter()
        .filter_map(|e| if let EntryKind::StepEnd { transition, .. } = e.kind { Some(transition) } else { None })
        .collect();
    assert!(matches!(ends.last(), Some(Transition::Aborted(_))), "the waiting step is closed as aborted: {ends:?}");
    let rs = results(&s);
    assert!(
        matches!(rs.as_slice(), [(_, aho_log::ToolOutcome::Cancelled { .. }, _, _)]),
        "the aborted turn closes its call with a Cancelled result: {rs:?}"
    );
    assert!(s.fold().unwrap().open_calls.is_empty());
}

#[tokio::test]
async fn read_only_preset_denies_writes_with_a_rendered_reason() {
    let r = rig("read-only", "");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("write", json!({"path": "x.txt", "content": "nope"})).with_stop(ScriptedStop::ToolUse),
            Step::text("ok, not allowed").expect_tool_result("read-only"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("write", None).await.unwrap(), Transition::Completed);
    assert!(!r.root.join("x.txt").exists());
}

#[tokio::test]
async fn a_session_grant_for_an_allowed_class_keeps_the_epoch_and_preset() {
    // `workspace` allows WorkspaceWrite; only the content rule asks. Nothing widens, so no epoch.
    let r = rig("workspace", "rules = [\"ask write(*)\"]\n");
    let (approval, mut rx) = ChannelApproval::new(4);
    tokio::spawn(async move {
        while let Some((card, reply)) = rx.recv().await {
            assert_eq!(card.tool, "write");
            let _ = reply.send(Decided { outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Session });
        }
    });
    let (s, _) = boot(
        &r,
        vec![Step::tool_call("write", json!({"path": "y.txt", "content": "yes"})).with_stop(ScriptedStop::ToolUse), Step::text("written")],
        Arc::new(MemoryLog::new()),
        Arc::new(approval),
    )
    .await;
    assert_eq!(s.prompt("write", None).await.unwrap(), Transition::Completed);
    assert_eq!(std::fs::read_to_string(r.root.join("y.txt")).unwrap(), "yes");
    let st = s.fold().unwrap();
    assert_eq!((st.policy.epoch, st.policy.preset.as_str()), (Epoch(1), "workspace"), "no redundant, relabeled epoch");
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    let req = entries.iter().position(|e| matches!(e.kind, EntryKind::ApprovalRequested { .. })).unwrap();
    let dec = entries.iter().position(|e| matches!(e.kind, EntryKind::ApprovalDecided { .. })).unwrap();
    let start = entries.iter().rposition(|e| matches!(e.kind, EntryKind::TurnStart { .. })).unwrap();
    assert!(start < req && req < dec, "approval pair sits inside the open turn");
}

#[tokio::test]
async fn a_session_grant_widens_once_and_later_calls_do_not_ask() {
    // ReadOnly plus a human to ask: a write is outside the policy but may be granted.
    let mut r = rig("read-only", "");
    r.plan.allowed = ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::TalksToHuman]);
    r.plan.preset = "custom".into();
    let (approval, mut rx) = ChannelApproval::new(4);
    let cards = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = cards.clone();
    tokio::spawn(async move {
        while let Some((_, reply)) = rx.recv().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = reply.send(Decided { outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Session });
        }
    });
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("write", json!({"path": "a.txt", "content": "a"})).with_stop(ScriptedStop::ToolUse),
            Step::tool_call("write", json!({"path": "b.txt", "content": "b"})).with_stop(ScriptedStop::ToolUse),
            Step::text("both written"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(approval),
    )
    .await;
    assert_eq!(s.prompt("write twice", None).await.unwrap(), Transition::Completed);
    assert!(r.root.join("a.txt").exists() && r.root.join("b.txt").exists(), "{:?}", results(&s));
    assert_eq!(cards.load(std::sync::atomic::Ordering::SeqCst), 1, "the session grant covers the second call");
    let st = s.fold().unwrap();
    assert_eq!(st.policy.epoch, Epoch(2), "exactly one widening epoch");
    assert!(st.policy.allowed.contains(Consequence::WorkspaceWrite));
}

#[tokio::test]
async fn unknown_tool_points_at_aho_why() {
    let r = rig("workspace", "");
    let (s, _) = boot(
        &r,
        vec![Step::tool_call("vm_start", json!({})).with_stop(ScriptedStop::ToolUse), Step::text("no vm").expect_tool_result("aho_why")],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("start a vm", None).await.unwrap(), Transition::Completed);
}

#[tokio::test]
async fn context_overflow_retries_the_step() {
    let r = rig("workspace", "");
    let (s, llm) = boot(
        &r,
        vec![Step::error(ScriptedError::ContextWindow), Step::text("ok after retry")],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("hi", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    assert!(entries.iter().any(|e| matches!(&e.kind, EntryKind::StepEnd { transition: Transition::ReactiveCompactRetry, .. })));
}

#[tokio::test]
async fn resume_marks_external_mid_dispatch_ops_uncertain() {
    let r = rig("workspace", "");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    {
        let log = JsonlLog::open(&path).unwrap();
        log.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(1) }).unwrap();
        log.append(
            None,
            Principal::system("model"),
            EntryKind::AssistantMessage {
                content: vec![ContentBlock::ToolUse { id: CallId("p1".into()), name: "bash".into(), args: json!({"command": "git push"}) }],
                usage: Usage::default(),
                stop: StopReason::ToolUse,
            },
        )
        .unwrap();
        log.append(
            None,
            Principal::system("model"),
            EntryKind::ToolCall {
                call: CallId("p1".into()),
                tool: "bash".into(),
                args: json!({"command": "git push"}),
                class: Consequence::SpawnsProcess,
            },
        )
        .unwrap();
        log.append(
            None,
            Principal::system("ops"),
            EntryKind::OpStarted {
                op: OpId(1),
                call: CallId("p1".into()),
                kind: aho_log::OpKind::Process,
                rerunnable: false,
                external_effect: true,
            },
        )
        .unwrap();
        log.append(None, Principal::system("loop"), EntryKind::TurnEnd { turn: TurnId(1) }).unwrap();
    }
    let (s, _) = boot(&r, vec![], Arc::new(JsonlLog::open(&path).unwrap()), Arc::new(NeverApproval)).await;
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    assert!(entries.iter().any(|e| matches!(&e.kind, EntryKind::OpDone { status: OpStatus::Uncertain { .. }, .. })));
    let rs = results(&s);
    assert!(rs[0].2.contains("may or may not have happened"));
}

#[tokio::test]
async fn rewind_code_restores_the_workspace_and_tells_the_model() {
    let r = rig("workspace", "");
    write(&r.root, "f.txt", "v1\n");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "f.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::tool_call("write", json!({"path": "f.txt", "content": "v2\n"})).with_stop(ScriptedStop::ToolUse),
            Step::text("changed"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    let c1 = s.checkpoint_now().await.expect("host world provides snapshots");
    assert_eq!(s.prompt("change it", None).await.unwrap(), Transition::Completed);
    assert_eq!(std::fs::read_to_string(r.root.join("f.txt")).unwrap(), "v2\n");
    let op = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    s.rewind(&op, c1, RewindMode::CodeOnly).await.unwrap();
    assert_eq!(std::fs::read_to_string(r.root.join("f.txt")).unwrap(), "v1\n");
    let st = s.fold().unwrap();
    assert!(st.checkpoints.len() >= 3, "c1, the write, and the PreRewind checkpoint");
}

#[tokio::test]
async fn client_key_dedupes_a_prompt_delivered_twice() {
    let r = rig("workspace", "");
    let (s, llm) = boot(&r, vec![Step::text("once")], Arc::new(MemoryLog::new()), Arc::new(NeverApproval)).await;
    s.prompt("hello", Some("k1".into())).await.unwrap();
    s.prompt("hello", Some("k1".into())).await.unwrap();
    assert_eq!(llm.remaining(), 0);
    assert_eq!(s.fold().unwrap().turns, 1);
}

fn dynamic_seed(llm: Arc<ScriptedLlm>) -> Registry {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm)));
    let exec: aho_cap::ToolExec = Arc::new(|req: aho_cap::ExecRequest| {
        Box::pin(async move { aho_cap::ExecOutcome::text(format!("echo:{}", req.view.args["text"].as_str().unwrap_or(""))) })
    });
    seed.dynamic_tools.push((
        aho_cap::CapabilityId { name: "mcp-stdio", major: 0 },
        ToolSpec {
            name: "mcp__fix__echo".into(),
            description: "echo".into(),
            input_schema: RootSchema(json!({"type": "object"})),
            class: Consequence::ReadOnly,
        },
        exec,
    ));
    seed
}

async fn boot_dynamic(r: &Rig, steps: Vec<Step>) -> Arc<Session> {
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    Session::boot(Boot {
        id: SessionId(9),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed: dynamic_seed(llm),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn runtime_registered_tools_run_through_the_loop_and_rules_see_their_names() {
    let r = rig("workspace", "");
    let s = boot_dynamic(
        &r,
        vec![
            Step::tool_call("mcp__fix__echo", json!({"text": "hi"})).with_stop(ScriptedStop::ToolUse),
            Step::text("got it").expect_tool_result("echo:hi"),
        ],
    )
    .await;
    assert_eq!(s.prompt("echo", None).await.unwrap(), Transition::Completed);

    let r = rig("workspace", "rules = [\"deny mcp__fix__*\"]\n");
    let s = boot_dynamic(
        &r,
        vec![
            Step::tool_call("mcp__fix__echo", json!({"text": "hi"})).with_stop(ScriptedStop::ToolUse),
            Step::text("denied").expect_tool_result("deny mcp__fix__*"),
        ],
    )
    .await;
    assert_eq!(s.prompt("echo", None).await.unwrap(), Transition::Completed);
    assert!(matches!(results(&s)[0].1, aho_log::ToolOutcome::Err { .. }));
}

#[tokio::test]
async fn a_stop_hook_continues_the_turn_once_with_its_reason() {
    let r = rig("workspace", "");
    let llm =
        Arc::new(ScriptedLlm::from_steps(vec![Step::text("done?"), Step::text("now really done").expect_user("run the tests first")]));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let hook = aho_cap::HookSpec {
        on: &[aho_cap::HookEvent::TurnStopping],
        if_class: None,
        if_tool: None,
        timeout_ms: 1000,
        once: false,
        on_error: aho_cap::OnError::Deny,
        run: Arc::new(|_, payload: &serde_json::Value| {
            let active = payload["stop_hook_active"].as_bool().unwrap_or(false);
            Box::pin(async move {
                aho_cap::HookOutput {
                    decision: (!active).then(|| aho_cap::Decision::Deny("run the tests first".into())),
                    ..Default::default()
                }
            })
        }),
    };
    seed.hooks.push((aho_cap::CapabilityId { name: "hooks-test", major: 0 }, hook));
    let s = Session::boot(Boot {
        id: SessionId(11),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![],
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
    assert_eq!(s.prompt("fix it", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0, "exactly one continuation; the guard stops a second");
}

#[tokio::test]
async fn tool_initiated_approvals_are_logged_inside_the_turn() {
    // a tool that asks through the seeded ApprovalHandle, like invariant_add
    let r = rig("workspace", "");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![
        Step::tool_call("ask_me", json!({})).with_stop(ScriptedStop::ToolUse),
        Step::text("approved").expect_tool_result("allowed_once"),
    ]));
    let (approval, mut rx) = ChannelApproval::new(2);
    tokio::spawn(async move {
        while let Some((_, reply)) = rx.recv().await {
            let _ = reply.send(Decided { outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Once });
        }
    });
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm)));
    // the tool is registered dynamically and resolves the approval handle lazily from a shared slot
    let slot: Arc<std::sync::Mutex<Option<aho_approval::ApprovalHandle>>> = Arc::default();
    let slot2 = slot.clone();
    let exec: aho_cap::ToolExec = Arc::new(move |_req| {
        let h = slot2.lock().unwrap().clone();
        Box::pin(async move {
            let h = h.expect("handle");
            let card = aho_policy::ApprovalCard {
                tool: "ask_me".into(),
                class: Consequence::TalksToHuman,
                args: json!({}),
                reason: aho_policy::DecisionReason::Passthrough,
                world: aho_cap::BoundWorld::host(),
                suggested_cut: None,
                rendered: "register invariant inv.x?".into(),
            };
            let d = h.0.request(card).await;
            aho_cap::ExecOutcome::text(format!("{:?}", d.outcome).to_lowercase().replace("allowedonce", "allowed_once"))
        })
    });
    seed.dynamic_tools.push((
        aho_cap::CapabilityId { name: "t", major: 0 },
        ToolSpec {
            name: "ask_me".into(),
            description: "".into(),
            input_schema: RootSchema(json!({"type": "object"})),
            class: Consequence::ReadOnly,
        },
        exec,
    ));
    let s = Session::boot(Boot {
        id: SessionId(12),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(approval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    *slot.lock().unwrap() = s.registry().handle::<aho_approval::ApprovalHandle>();
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    let entries = s.log().read_branch(s.log().head().unwrap()).unwrap();
    let kinds: Vec<&str> = entries.iter().map(|e| e.kind.tag()).collect();
    let req = kinds.iter().position(|k| *k == "ApprovalRequested").expect("request logged");
    let dec = kinds.iter().position(|k| *k == "ApprovalDecided").expect("decision logged");
    let start = kinds.iter().rposition(|k| *k == "TurnStart").unwrap();
    let end = kinds.iter().rposition(|k| *k == "TurnEnd").unwrap();
    assert!(start < req && req < dec && dec < end);
}

#[tokio::test]
async fn rewinding_into_a_turn_closes_it_and_the_next_prompt_folds() {
    let r = rig("workspace", "");
    write(&r.root, "g.txt", "v1\n");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "g.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::tool_call("write", json!({"path": "g.txt", "content": "v2\n"})).with_stop(ScriptedStop::ToolUse),
            Step::text("changed"),
            Step::text("fresh start after rewind"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    s.prompt("change it", None).await.unwrap();
    // the checkpoint taken right after the write lands mid-turn
    let mid = s
        .fold()
        .unwrap()
        .checkpoints
        .iter()
        .find(|c| {
            let e = s.log().get(c.2).unwrap();
            matches!(e.kind, EntryKind::WorkspaceCheckpoint { cause: aho_log::CheckpointCause::ToolResult(_), .. })
        })
        .map(|c| c.0)
        .expect("a mid-turn checkpoint");
    let op = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    s.rewind(&op, mid, RewindMode::Both).await.unwrap();
    let st = s.fold().unwrap();
    assert!(st.turn.is_none() && st.open_calls.is_empty() && st.open_ops.is_empty());
    assert_eq!(s.prompt("again", None).await.unwrap(), Transition::Completed);
    assert!(s.fold().is_ok());
}

#[tokio::test]
async fn allow_lifts_the_preset_for_one_capability_but_not_safety() {
    let r = rig("read-only", "[caps.tool-fs]\nallow = true\n");
    let (s, _) = boot(
        &r,
        vec![
            Step::tool_call("write", json!({"path": "z.txt", "content": "ok"})).with_stop(ScriptedStop::ToolUse),
            Step::tool_call("bash", json!({"command": "echo nope"})).with_stop(ScriptedStop::ToolUse),
            Step::text("done").expect_tool_result("read-only"),
        ],
        Arc::new(MemoryLog::new()),
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!(std::fs::read_to_string(r.root.join("z.txt")).unwrap(), "ok", "tool-fs is allowed past read-only");
    assert!(matches!(results(&s)[1].1, aho_log::ToolOutcome::Err { .. }), "tool-bash is not");
}

#[tokio::test]
async fn operator_preset_changes_are_durable_and_spec_scope_survives_resume() {
    let r = rig("workspace", "");
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let (session, _) = boot(&r, vec![], log.clone(), Arc::new(NeverApproval)).await;
    let operator = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    let command = |name: &str| {
        log.append(None, Principal::Operator, EntryKind::CommandRun { name: "preset".into(), args: vec![name.into()], key: None })
            .unwrap()
            .id
    };
    session.set_preset(&operator, "read-only", command("read-only")).unwrap();
    assert!(!session.policy_view().allowed.contains(Consequence::WorkspaceWrite));
    session.set_preset(&operator, "dev", command("dev")).unwrap();
    assert!(session.policy_view().allowed.contains(Consequence::NetworkEgress), "an operator preset may widen");
    session.set_preset(&operator, "spec", command("spec")).unwrap();
    let policy = session.fold().unwrap().policy;
    assert_eq!(policy.preset, "spec");
    let head = log.head();
    assert!(session.set_preset(&operator, "unknown", EntryId(0)).is_err());
    assert_eq!(log.head(), head, "invalid presets must not change the log");
    drop(session);
    let (resumed, _) = boot(
        &r,
        vec![
            Step::tool_call("write", json!({"path": "outside-spec.txt", "content": "must be refused"})).with_stop(ScriptedStop::ToolUse),
            Step::text("done"),
        ],
        log,
        Arc::new(NeverApproval),
    )
    .await;
    resumed.prompt("try to write outside spec", None).await.unwrap();
    assert!(!r.root.join("outside-spec.txt").exists(), "resuming must retain the spec content restriction");
    assert_eq!(resumed.fold().unwrap().policy.preset, "spec");
}
