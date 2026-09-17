//! End to end: a parent Session (scripted model) calls the subagent tools; `subagent-inproc`
//! boots real child Sessions over JSONL logs with their own scripted model.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aho_approval::{Approval, ChannelApproval, Decided, NeverApproval};
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{ApprovalOutcome, CustomFolds, Entry, EntryKind, GrantScope, JsonlLog, LogStore, OpKind, Transition};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;
use subagent_inproc::{SubagentInproc, SubagentRuntime};
use tool_subagent::ToolSubagent;

// A cancel-aware slow tool, so a child can be caught mid-run.
struct SlowTool;
static SLOW: &[ToolDecl] =
    &[ToolDecl::new("slow", "Sleeps until cancelled (test tool).", Consequence::ReadOnly).safe().op(OpKind::Timer, false, false)];
impl Capability for SlowTool {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SlowTool)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &SLOW[0],
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
    }
}
declare_capability!(SlowTool, "slow-tool", kind = Kind::Tool, provides = [], requires = [], tools = SLOW);

struct Rig {
    comp: Composition,
    plan: Plan,
    loadout: aho_config::Loadout,
    dir: tempfile::TempDir,
}

fn rig(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let comp = Composition::new("subagent-test").provide::<SubagentInproc>().consume::<ToolSubagent>().consume::<SlowTool>();
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

struct Parent {
    s: Arc<Session>,
    parent_llm: Arc<ScriptedLlm>,
    child_llm: Arc<ScriptedLlm>,
    sessions: PathBuf,
}

fn operator() -> Operator {
    Operator::authenticate(OperatorProof::local_tty())
}

async fn boot(r: &Rig, parent: Vec<Step>, child: Vec<Step>, approval: Arc<dyn Approval>) -> Parent {
    let parent_llm = Arc::new(ScriptedLlm::from_steps(parent));
    let child_llm = Arc::new(ScriptedLlm::from_steps(child));
    let sessions = r.dir.path().join("sessions");
    let log: Arc<dyn LogStore> = Arc::new(JsonlLog::open(sessions.join("7.jsonl")).unwrap());
    let bootstrap =
        vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: r.plan.allowed, preset: r.plan.preset.clone(), approval_ref: None }];
    let mut child_seed = Registry::default();
    child_seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(child_llm.clone())));
    let ws = r.dir.path().join("ws").display().to_string();
    let home = r.dir.path().join("home");
    let rt = SubagentRuntime {
        composition: r.comp.clone(),
        plan: r.plan.clone(),
        loadout: r.loadout.clone(),
        bootstrap: bootstrap.clone(),
        seed: child_seed,
        operator: Arc::new(operator),
        spill_dir: home.join("spill"),
        session_dir: Some(sessions.clone()),
        session: SessionId(7),
        log: log.clone(),
        depth: 0,
    };
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(parent_llm.clone())));
    rt.seed_into(&mut seed);
    let s = Session::boot(Boot {
        id: SessionId(7),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap,
        log,
        approval,
        operator: operator(),
        workspace: ws,
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    Parent { s, parent_llm, child_llm, sessions }
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

fn child_logs(dir: &Path) -> Vec<(SessionId, Vec<Entry>)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<(SessionId, Vec<Entry>)> = rd
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .map(|p| {
            let id: u128 = p.file_stem().unwrap().to_str().unwrap().parse().unwrap();
            let log = JsonlLog::open(&p).unwrap();
            (SessionId(id), log.read_branch(log.head().unwrap()).unwrap())
        })
        .collect();
    out.sort_by_key(|(_, es)| es[0].at);
    out
}

fn user_texts(es: &[Entry]) -> Vec<String> {
    es.iter().filter_map(|e| if let EntryKind::UserMessage { text, .. } = &e.kind { Some(text.clone()) } else { None }).collect()
}

#[tokio::test]
async fn parent_receives_the_childs_last_message_and_logs_its_lifecycle() {
    let r = rig("");
    let p = boot(
        &r,
        vec![
            Step::tool_call("subagent", json!({"prompt": "find the answer", "label": "finder", "persona": "You are terse."})),
            Step::text("the child says 42").expect_tool_result("the answer is 42"),
        ],
        vec![Step::text("the answer is 42")],
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(p.s.prompt("delegate it", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));

    let es = entries(&p.s);
    let result = es.iter().position(|e| matches!(&e.kind, EntryKind::ToolResult { .. })).unwrap();
    let started = es
        .iter()
        .position(|e| matches!(&e.kind, EntryKind::SubagentStarted { depth: 1, provider, .. } if provider == "subagent-inproc"))
        .unwrap();
    let ended = es
        .iter()
        .position(|e| matches!(&e.kind, EntryKind::SubagentEnded { stop: StopReason::EndTurn, last_message: Some(m), .. } if m == "the answer is 42"))
        .unwrap();
    assert!(result < started && started < ended, "lifecycle entries follow the tool result, Started before Ended");
    assert!(verify_replay(&es, &CustomFolds::new(), LateResults::SameCallId).unwrap() >= 2);

    let children = child_logs(&p.sessions.join("7"));
    assert_eq!(children.len(), 1, "one child log under <session.dir>/<parent>/");
    let (cid, ces) = &children[0];
    assert!(matches!(&es[started].kind, EntryKind::SubagentStarted { child, .. } if child == cid));
    assert!(
        matches!(&ces[0].kind, EntryKind::SubagentDescriptor { parent: SessionId(7), depth: 1, persona: Some(p), .. } if p == "You are terse.")
    );
    assert!(ces.iter().any(|e| matches!(e.kind, EntryKind::PolicyEpoch { .. }) && e.by == Principal::system("delegation")));
    assert!(
        !ces.iter().any(|e| matches!(e.kind, EntryKind::PolicyEpoch { .. }) && e.by != Principal::system("delegation")),
        "the parent's own epoch is not copied into a spawned child"
    );
    let texts = user_texts(ces);
    assert!(texts.contains(&"You are terse.".to_string()) && texts.contains(&"find the answer".to_string()));
    aho_log::fold(ces, &CustomFolds::new()).unwrap();
}

#[tokio::test]
async fn fork_child_sees_the_parent_prefix_but_not_the_open_turn() {
    let r = rig("");
    let p = boot(
        &r,
        vec![
            Step::text("noted"),
            Step::tool_call("subagent_fork", json!({"prompt": "what is the codeword?"})),
            Step::text("done").expect_tool_result("BLUE"),
        ],
        vec![Step::text("BLUE")],
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(p.s.prompt("the codeword is BLUE", None).await.unwrap(), Transition::Completed);
    assert_eq!(p.s.prompt("fork a helper", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));

    let children = child_logs(&p.sessions.join("7"));
    assert_eq!(children.len(), 1);
    let ces = &children[0].1;
    assert!(matches!(ces[0].kind, EntryKind::SubagentDescriptor { depth: 1, .. }), "the descriptor comes before the inherited prefix");
    let texts = user_texts(ces);
    assert!(texts.contains(&"the codeword is BLUE".to_string()), "completed turn inherited");
    assert!(texts.contains(&"what is the codeword?".to_string()));
    assert!(!texts.contains(&"fork a helper".to_string()), "the open turn is never inherited");
    assert!(!ces.iter().any(|e| matches!(&e.kind, EntryKind::ToolCall { tool, .. } if tool == "subagent_fork")));
    let state = aho_log::fold(ces, &CustomFolds::new()).unwrap();
    assert_eq!(state.turns, 2, "one inherited turn plus the child's own");
    assert!(state.turn.is_none());
}

#[tokio::test]
async fn async_unbubbled_child_cannot_ask_the_operator() {
    let r = rig("[policy]\nrules = [\"ask list_agents\"]\n");
    let (approval, mut cards) = ChannelApproval::new(4);
    let p = boot(
        &r,
        vec![
            Step::tool_call("subagent", json!({"prompt": "list your agents", "label": "bg", "async": true})),
            Step::text("ok").expect_tool_result("could not list"),
        ],
        vec![Step::tool_call("list_agents", json!({})), Step::text("could not list").expect_tool_result("APPROVAL")],
        Arc::new(approval),
    )
    .await;
    assert_eq!(p.s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));
    assert!(cards.try_recv().is_err(), "no card reached the parent's operator");

    let ces = &child_logs(&p.sessions.join("7"))[0].1;
    assert!(ces.iter().any(|e| matches!(e.kind, EntryKind::ApprovalDecided { outcome: ApprovalOutcome::Unavailable, .. })));
    let delegated = ces
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::PolicyEpoch { allowed, .. } if e.by == Principal::system("delegation") => Some(*allowed),
            _ => None,
        })
        .unwrap();
    assert!(!delegated.contains(Consequence::TalksToHuman), "an async child cannot talk to a human unless bubbled");
    assert!(delegated.is_subset(&r.plan.allowed));
}

#[tokio::test]
async fn bubbled_async_child_asks_through_the_parents_channel() {
    let r = rig("[policy]\nrules = [\"ask list_agents\"]\n");
    let (approval, mut cards) = ChannelApproval::new(4);
    let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some((card, reply)) = cards.recv().await {
            let _ = seen_tx.send(card.tool.clone());
            let _ = reply.send(Decided::allowed(GrantScope::Once));
        }
    });
    let p = boot(
        &r,
        vec![
            Step::tool_call("subagent", json!({"prompt": "list your agents", "label": "bg", "async": true, "bubble": true})),
            Step::text("ok").expect_tool_result("listed"),
        ],
        vec![Step::tool_call("list_agents", json!({})), Step::text("listed").expect_tool_result("No subagents")],
        Arc::new(approval),
    )
    .await;
    assert_eq!(p.s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));
    assert_eq!(seen.try_recv().ok(), Some("list_agents".to_string()));
    assert!(seen.try_recv().is_err(), "exactly one card");
}

#[tokio::test]
async fn depth_cap_refuses_a_grandchild() {
    let r = rig("[subagent]\nmax_depth = 1\n");
    let p = boot(
        &r,
        vec![
            Step::tool_call("subagent", json!({"prompt": "go deeper", "label": "c1"})),
            Step::text("ok").expect_tool_result("cannot go deeper"),
        ],
        vec![
            Step::tool_call("subagent", json!({"prompt": "deeper still", "label": "gc"})),
            Step::text("cannot go deeper").expect_tool_result("DEPTH_LIMIT"),
        ],
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(p.s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));
    let children = child_logs(&p.sessions.join("7"));
    assert_eq!(children.len(), 1);
    assert!(child_logs(&p.sessions.join(children[0].0.0.to_string())).is_empty(), "no grandchild log was created");
    assert!(children[0].1.iter().any(
        |e| matches!(&e.kind, EntryKind::ToolResult { outcome: aho_log::ToolOutcome::Err { code, .. }, .. } if code == "DEPTH_LIMIT")
    ));
}

#[tokio::test]
async fn send_message_continues_the_child_and_list_and_interrupt_see_it() {
    let r = rig("");
    let p = boot(
        &r,
        vec![
            Step::tool_call("subagent", json!({"prompt": "first task", "label": "w"})),
            Step::tool_call("send_message", json!({"child": "w", "text": "second task"})).expect_tool_result("one"),
            Step::tool_call("list_agents", json!({})).expect_tool_result("two"),
            Step::tool_call("interrupt_agent", json!({"child": "w"})).expect_tool_result("idle (last run: end_turn)"),
            Step::text("fine").expect_tool_result("nothing to interrupt"),
        ],
        vec![Step::text("one"), Step::text("two").expect_user("second task")],
        Arc::new(NeverApproval),
    )
    .await;
    assert_eq!(p.s.prompt("go", None).await.unwrap(), Transition::Completed);
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (0, 0));
    let ends: Vec<Option<String>> = entries(&p.s)
        .into_iter()
        .filter_map(|e| if let EntryKind::SubagentEnded { last_message, .. } = e.kind { Some(last_message) } else { None })
        .collect();
    assert_eq!(ends, vec![Some("one".to_string()), Some("two".to_string())], "every run of the child is closed in the parent log");
    assert_eq!(child_logs(&p.sessions.join("7")).len(), 1, "send_message continues the same child");
}

#[tokio::test]
async fn parent_abort_interrupts_the_child_and_still_closes_its_run() {
    let r = rig("");
    let p = boot(
        &r,
        vec![Step::tool_call("subagent", json!({"prompt": "sleep", "label": "s"})), Step::text("an aborted turn never reaches this")],
        vec![Step::tool_call("slow", json!({})), Step::text("an interrupted child never reaches this")],
        Arc::new(NeverApproval),
    )
    .await;
    let s = p.s.clone();
    let turn = tokio::spawn(async move { s.prompt("go", None).await });
    // wait until the child is inside `slow` (raw text: the child is still appending)
    let dir = p.sessions.join("7");
    let mut in_tool = false;
    for _ in 0..250 {
        let texts: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| std::fs::read_to_string(e.path()).ok()).collect())
            .unwrap_or_default();
        if texts.iter().any(|t| t.contains("\"type\":\"op_started\"")) {
            in_tool = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(in_tool, "the child reached its slow tool");
    p.s.abort();
    assert!(matches!(turn.await.unwrap().unwrap(), Transition::Aborted(_)));
    assert_eq!((p.parent_llm.remaining(), p.child_llm.remaining()), (1, 1), "neither parent nor child asks the model again");
    // The subagent call was cancelled with the parent's turn; the child's run is still closed in the
    // parent log once its result lands (late, at the next turn boundary or on resume).
    let es = entries(&p.s);
    assert!(
        es.iter().any(|e| matches!(&e.kind, EntryKind::SubagentEnded { stop: StopReason::Aborted, .. })),
        "the aborted child's run is closed in the parent log"
    );
}

#[test]
fn lifecycle_events_are_accepted_by_the_session() {
    let v =
        json!({"events": tool_subagent::lifecycle_events(SessionId(123), "subagent-inproc", 1, StopReason::EndTurn, Some("hi".into()))});
    let got = aho_session::tool_events(&v);
    assert_eq!(got.len(), 2, "{v}");
}
