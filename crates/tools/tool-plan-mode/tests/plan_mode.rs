//! Plan mode against a session ceiling that implements the `aho_control::SessionCeiling`
//! contract over the real lattice (`aho_policy::SandboxPolicy`), a real log and a real approval
//! channel whose other end plays the operator. The ceiling below is the reference for the
//! session-side implementation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use aho_approval::{Approval, AskPolicy, ChannelApproval, Decided, NeverApproval};
use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_control::{CEILING_SERVICE, CeilingHandle, SessionCeiling, Widen, Widened};
use aho_log::{ApprovalOutcome, CustomFolds, EntryKind, GrantScope, LogStore, MemoryLog, State, fold};
use aho_policy::{CallUnderReview, ChainOutcome, DecisionReason, Operator, OperatorProof, Preset, SandboxPolicy};
use aho_types::*;
use serde_json::json;
use tool_plan_mode::{NS, PLAN_CUT, ToolPlanMode, fold_plan};

struct PolicyCeiling {
    policy: Mutex<SandboxPolicy>,
    log: Arc<MemoryLog>,
    approval: Arc<dyn Approval>,
    operator: Operator,
}

fn folds() -> CustomFolds {
    [(NS.to_string(), fold_plan as aho_log::CustomFold)].into_iter().collect()
}

impl PolicyCeiling {
    fn policy(&self) -> std::sync::MutexGuard<'_, SandboxPolicy> {
        self.policy.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn state(&self) -> State {
        fold(&self.log.read_branch(self.log.head().unwrap()).unwrap(), &folds()).unwrap()
    }
}

impl SessionCeiling for PolicyCeiling {
    fn view(&self) -> PolicyView {
        self.policy().view()
    }
    fn narrow(&self, mode: &str, cut: ConsequenceSet) -> Result<PolicyView, String> {
        let mut p = self.policy();
        let allowed = p.allowed().intersect(cut);
        if allowed == p.allowed() {
            return Ok(p.view());
        }
        let next = SandboxPolicy::new(allowed, p.world().clone(), p.workspace().into(), p.home().into(), Epoch(p.epoch().0 + 1));
        self.log
            .append(
                None,
                Principal::system(mode),
                EntryKind::PolicyEpoch { epoch: next.epoch(), allowed, preset: next.preset().name().into(), approval_ref: None },
            )
            .map_err(|e| e.to_string())?;
        *p = next;
        Ok(p.view())
    }
    fn request_widen(&self, w: Widen) -> BoxFuture<'_, Widened> {
        Box::pin(async move {
            let view = || self.policy().view();
            if self.state().turn.is_none() || self.approval.policy() == AskPolicy::Never {
                return Widened { outcome: ApprovalOutcome::Unavailable, view: view() };
            }
            let (wants, card) = {
                let p = self.policy();
                let card = aho_policy::ApprovalCard {
                    tool: w.tool.clone(),
                    class: Consequence::TalksToHuman,
                    args: w.args.clone(),
                    reason: DecisionReason::ToolCheck(format!("`{}` asks to restore {}", w.tool, w.to)),
                    world: p.world().clone(),
                    suggested_cut: None,
                    rendered: w.rendered.clone(),
                };
                (ConsequenceSet(w.to.0 & !p.allowed().0), card)
            };
            let req = self
                .log
                .append(
                    None,
                    Principal::system("policy"),
                    EntryKind::ApprovalRequested { wants, for_cap: w.tool.clone(), call: None, reason: w.rendered.clone() },
                )
                .unwrap();
            let d = self.approval.request(card).await.normalized();
            self.log
                .append(None, Principal::Operator, EntryKind::ApprovalDecided { req: req.id, outcome: d.outcome, scope: d.scope })
                .unwrap();
            if d.outcome != ApprovalOutcome::AllowedOnce {
                return Widened { outcome: d.outcome, view: view() };
            }
            let mut p = self.policy();
            let n = p.widen_by_approval(&self.operator, req.id, w.to);
            self.log
                .append(
                    None,
                    Principal::Operator,
                    EntryKind::PolicyEpoch {
                        epoch: n.epoch(),
                        allowed: n.allowed(),
                        preset: n.preset().name().into(),
                        approval_ref: Some(req.id),
                    },
                )
                .unwrap();
            *p = n;
            Widened { outcome: ApprovalOutcome::AllowedOnce, view: p.view() }
        })
    }
}

struct Rig {
    reg: Registry,
    ceiling: Arc<PolicyCeiling>,
    log: Arc<MemoryLog>,
    calls: u32,
}

fn workspace() -> ConsequenceSet {
    Preset::Workspace.cut(&BoundWorld::host())
}

fn rig(approval: Arc<dyn Approval>, allowed: ConsequenceSet) -> Rig {
    let log = Arc::new(MemoryLog::new());
    log.append(
        None,
        Principal::system("bootstrap"),
        EntryKind::PolicyEpoch { epoch: Epoch(1), allowed, preset: "workspace".into(), approval_ref: None },
    )
    .unwrap();
    log.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(1) }).unwrap();
    let policy = SandboxPolicy::new(allowed, BoundWorld::host(), "/ws".into(), "/home".into(), Epoch(1));
    let ceiling = Arc::new(PolicyCeiling {
        policy: Mutex::new(policy),
        log: log.clone(),
        approval,
        operator: Operator::authenticate(OperatorProof::local_tty()),
    });
    let (reg, broken) = start(Some(ceiling.clone()));
    assert!(broken.is_empty(), "{broken:?}");
    Rig { reg, ceiling, log, calls: 0 }
}

fn start(ceiling: Option<Arc<PolicyCeiling>>) -> (Registry, Vec<(CapabilityId, String)>) {
    let comp = Composition::new("plan-test").consume::<ToolPlanMode>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let loadout = resolve(&ResolveInputs::default(), &schema, false).unwrap();
    let p = plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: workspace() },
        vec![],
    )
    .unwrap();
    let mut seed = Registry::default();
    if let Some(c) = ceiling {
        let c: Arc<dyn SessionCeiling> = c;
        seed.services.insert(CEILING_SERVICE.into(), Arc::new(CeilingHandle(c)));
    }
    comp.start_seeded(&p, &loadout, seed)
}

static WRITE: ToolDecl = ToolDecl::new("write", "write a file", Consequence::WorkspaceWrite);

impl Rig {
    fn view(&mut self, tool: &str, class: Consequence, args: Value) -> ExecView {
        self.calls += 1;
        ExecView {
            call: CallId(format!("c{}", self.calls)),
            tool: tool.into(),
            class,
            args,
            policy: self.ceiling.view(),
            by: Principal::Agent { id: AgentId(0), turn: TurnId(1) },
        }
    }
    /// Like the session: pre_execute, run, append the result's `value.events`, rebuild contexts.
    async fn run(&mut self, tool: &str, args: Value) -> ExecOutcome {
        let (decl, exec) = self.reg.tool(tool).unwrap();
        let view = self.view(tool, decl.class, args);
        for (_, pre) in &self.reg.pre {
            assert_eq!(pre(&view), PreDecision::Allow);
        }
        let out = exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }).await;
        if let ExecOutcome::Done { value, .. } = &out {
            for e in value["events"].as_array().unwrap() {
                self.log
                    .append(
                        None,
                        Principal::system(tool),
                        EntryKind::Custom {
                            ns: e["ns"].as_str().unwrap().into(),
                            name: e["name"].as_str().unwrap().into(),
                            body: e["body"].clone(),
                        },
                    )
                    .unwrap();
            }
        }
        self.contexts();
        out
    }
    fn contexts(&self) -> Vec<String> {
        let state = self.ceiling.state();
        self.reg.contexts.iter().filter_map(|(_, f)| f(&state)).collect()
    }
    fn guard(&mut self, class: Consequence) -> Option<String> {
        let v = self.view("write", class, json!({"path": "/ws/a.rs"}));
        self.reg.guards.iter().find_map(|(_, g)| g(&v))
    }
    /// The real decision chain for a workspace write under the live policy and these guards.
    fn chain_for_write(&mut self) -> ChainOutcome {
        let v = self.view("write", Consequence::WorkspaceWrite, json!({"path": "/ws/a.rs"}));
        let guard_denials: Vec<(String, String)> =
            self.reg.guards.iter().filter_map(|(c, g)| g(&v).map(|r| (c.name.to_string(), r))).collect();
        let policy = self.ceiling.policy().clone();
        aho_policy::decide(&CallUnderReview {
            decl: &WRITE,
            args: &v.args,
            by: &v.by,
            rules: &[],
            policy: &policy,
            guard_denials,
            escalate: vec![],
            tool_name: None,
        })
    }
    fn epochs(&self) -> Vec<(u32, ConsequenceSet, Principal, Option<EntryId>)> {
        self.log
            .read_branch(self.log.head().unwrap())
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::PolicyEpoch { epoch, allowed, approval_ref, .. } => Some((epoch.0, *allowed, e.by.clone(), *approval_ref)),
                _ => None,
            })
            .collect()
    }
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content[0].as_text().unwrap().to_string(),
        ExecOutcome::Err { code, reason } => format!("{code}: {reason}"),
        ExecOutcome::Cancelled => "cancelled".into(),
    }
}

/// The operator's side of the channel: answers each card in order, returning the cards seen.
fn operator(answers: Vec<ApprovalOutcome>) -> (Arc<dyn Approval>, tokio::task::JoinHandle<Vec<aho_policy::ApprovalCard>>) {
    let (a, mut rx) = ChannelApproval::new(4);
    let task = tokio::spawn(async move {
        let mut seen = Vec::new();
        for o in answers {
            let (card, reply) = rx.recv().await.unwrap();
            seen.push(card);
            let scope = if o == ApprovalOutcome::AllowedOnce { GrantScope::Session } else { GrantScope::Once };
            reply.send(Decided { outcome: o, scope }).unwrap();
        }
        seen
    });
    (Arc::new(a), task)
}

#[tokio::test]
async fn a_rejected_plan_keeps_the_session_read_only_and_only_approval_restores_it() {
    let (approval, answered) = operator(vec![ApprovalOutcome::Rejected, ApprovalOutcome::AllowedOnce]);
    let mut r = rig(approval, workspace());
    assert!(r.contexts().is_empty(), "no plan-mode section before plan mode was ever on");
    assert!(r.chain_for_write().is_allow());

    let out = r.run("enter_plan_mode", json!({"reason": "big refactor"})).await;
    assert!(text(&out).starts_with("Plan mode is on. Allowed now: ReadOnly · TalksToHuman."), "{}", text(&out));
    assert_eq!(r.ceiling.view().allowed, PLAN_CUT);
    assert_eq!(r.epochs()[1], (2, PLAN_CUT, Principal::system("plan-mode"), None), "narrowing is an epoch nobody had to approve");
    assert!(r.contexts()[0].starts_with("Plan mode is on (entered by agent:0: big refactor)."), "{:?}", r.contexts());
    let refused = r.guard(Consequence::WorkspaceWrite).unwrap();
    assert!(refused.contains("plan mode is on: `write` is WorkspaceWrite") && refused.contains("exit_plan_mode"), "{refused}");
    assert!(r.guard(Consequence::SpawnsProcess).is_some());
    assert!(r.guard(Consequence::ReadOnly).is_none() && r.guard(Consequence::TalksToHuman).is_none());
    assert!(
        matches!(r.chain_for_write(), ChainOutcome::Deny(DecisionReason::Guard(g)) if g.starts_with("tool-plan-mode: plan mode is on"))
    );

    // the operator says no
    let out = r.run("exit_plan_mode", json!({"plan": "1. split parser.rs\n2. run the tests"})).await;
    assert!(text(&out).starts_with("The operator rejected this plan, so plan mode stays on"), "{}", text(&out));
    assert_eq!(r.ceiling.view().allowed, PLAN_CUT, "a rejection widens nothing");
    assert_eq!(r.epochs().len(), 2);
    assert!(r.guard(Consequence::WorkspaceWrite).is_some(), "still read-only");
    assert!(r.contexts()[0].starts_with("Plan mode is on"));

    // revised plan, approved
    let out = r.run("exit_plan_mode", json!({"plan": "1. split parser.rs into lexer.rs and parser.rs\n2. cargo test"})).await;
    assert!(
        text(&out).starts_with("The operator approved the plan. Plan mode is off; allowed again: ReadOnly · WorkspaceWrite"),
        "{}",
        text(&out)
    );
    assert_eq!(r.ceiling.view().allowed, workspace());
    let epochs = r.epochs();
    assert_eq!(epochs.len(), 3);
    assert_eq!((epochs[2].0, epochs[2].1, &epochs[2].2), (3, workspace(), &Principal::Operator));
    assert!(epochs[2].3.is_some(), "the widening names the operator's decision");
    assert!(r.guard(Consequence::WorkspaceWrite).is_none());
    assert!(r.chain_for_write().is_allow());
    assert_eq!(r.contexts(), vec!["Plan mode is off: the operator approved the plan. The session policy applies again.".to_string()]);

    let cards = answered.await.unwrap();
    assert!(cards[0].rendered.contains("2. run the tests") && cards[0].rendered.contains("Approve to restore"), "{}", cards[0].rendered);
    assert_eq!((cards[1].tool.as_str(), cards[1].class), ("exit_plan_mode", Consequence::TalksToHuman));

    // the whole log replays: epochs, the approval pair, the plan fold
    let state = r.ceiling.state();
    assert_eq!(state.policy.allowed, workspace());
    let plan = tool_plan_mode::project(state.custom.get(NS)).unwrap();
    assert_eq!((plan.active, plan.reviews, plan.times_entered), (false, 2, 1));
}

#[tokio::test]
async fn calls_after_enter_in_the_same_step_are_refused_before_it_runs() {
    let (approval, _answered) = operator(vec![]);
    let mut r = rig(approval, workspace());
    let (decl, exec) = r.reg.tool("enter_plan_mode").unwrap();
    let enter = r.view("enter_plan_mode", decl.class, json!({}));
    for (_, pre) in &r.reg.pre {
        pre(&enter);
    }
    // the session reviews the next call of the batch before the enter body has run
    assert!(r.guard(Consequence::WorkspaceWrite).is_some(), "entering already refuses writes");
    let out = exec(ExecRequest { view: enter, cancel: CancelToken::new(), progress: None }).await;
    assert!(text(&out).starts_with("Plan mode is on."));
    // its result is not logged yet (call still open) and the fold does not show plan mode: the guard stays shut
    r.log
        .append(
            None,
            Principal::Agent { id: AgentId(0), turn: TurnId(1) },
            EntryKind::ToolCall {
                call: CallId("c1".into()),
                tool: "enter_plan_mode".into(),
                args: json!({}),
                class: Consequence::ReadOnly,
            },
        )
        .unwrap();
    r.contexts();
    assert!(r.guard(Consequence::WorkspaceWrite).is_some());
    let again = r.run("enter_plan_mode", json!({})).await;
    assert!(matches!(&again, ExecOutcome::Err { code, .. } if code == "already_on"), "{}", text(&again));
}

#[tokio::test]
async fn without_an_approver_plan_mode_is_not_entered_or_is_never_left_silently() {
    // a session whose policy cannot ask the operator refuses to enter
    let mut r = rig(Arc::new(NeverApproval), ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]));
    let out = r.run("enter_plan_mode", json!({})).await;
    assert!(matches!(&out, ExecOutcome::Err { code, .. } if code == "no_approver"), "{}", text(&out));
    assert_eq!(r.epochs().len(), 1, "nothing narrowed");

    // prompts off: the exit is Unavailable and plan mode stays on
    let mut r = rig(Arc::new(NeverApproval), workspace());
    r.run("enter_plan_mode", json!({})).await;
    let out = r.run("exit_plan_mode", json!({"plan": "do it"})).await;
    assert!(text(&out).starts_with("No approval channel is available"), "{}", text(&out));
    assert_eq!(r.ceiling.view().allowed, PLAN_CUT);
    assert!(r.guard(Consequence::WorkspaceWrite).is_some());
    let empty = r.run("exit_plan_mode", json!({"plan": "  "})).await;
    assert!(matches!(&empty, ExecOutcome::Err { code, .. } if code == "bad_args"));
}

#[tokio::test]
async fn the_operator_can_enter_plan_mode_between_turns() {
    let (approval, _answered) = operator(vec![]);
    let r = rig(approval, workspace());
    let announce = |kind| r.log.append(None, Principal::Operator, kind).map(|_| ()).map_err(|e| e.to_string());
    let said = tool_plan_mode::enter_by_operator(&*r.ceiling, &*r.log, &announce, Some("review first".into())).unwrap();
    assert_eq!(said, "Plan mode is on. Allowed now: ReadOnly · TalksToHuman.");
    assert!(r.contexts()[0].starts_with("Plan mode is on (entered by operator: review first)."));
    assert_eq!(tool_plan_mode::enter_by_operator(&*r.ceiling, &*r.log, &announce, None).unwrap(), "Plan mode is already on.");
}

#[test]
fn plan_mode_is_off_without_the_session_ceiling() {
    let (reg, broken) = start(None);
    assert!(reg.tool("exit_plan_mode").is_none());
    assert!(broken[0].1.contains("does not expose its policy ceiling"), "{}", broken[0].1);
    let classes: Vec<Consequence> = ToolPlanMode::MANIFEST.tools.iter().map(|t| t.class).collect();
    assert_eq!(classes, vec![Consequence::ReadOnly, Consequence::TalksToHuman]);
}

#[test]
fn the_fold_keeps_plan_mode_on_until_an_approval_restores_the_policy() {
    use tool_plan_mode::{Event, PlanError, PlanMode};
    let ws = workspace();
    let m = PlanMode::default();
    let by = aho_goals::Actor::Agent(AgentId(0));
    assert!(matches!(m.enter(&by, Timestamp(1), None, ws, ws), Err(PlanError::NotNarrowed { .. })), "entering must narrow to plan's cut");
    let e = m.enter(&by, Timestamp(1), None, ws, PLAN_CUT).unwrap();
    let on = m.stage(&e).unwrap();
    assert!(matches!(on.stage(&e), Err(PlanError::AlreadyOn)));
    for o in [ApprovalOutcome::Rejected, ApprovalOutcome::Cancelled, ApprovalOutcome::Unavailable] {
        assert!(on.stage(&Event::Reviewed { at: Timestamp(2), plan: "p".into(), outcome: o, allowed: PLAN_CUT }).unwrap().active);
    }
    let not_restored = Event::Reviewed { at: Timestamp(2), plan: "p".into(), outcome: ApprovalOutcome::AllowedOnce, allowed: PLAN_CUT };
    assert!(matches!(on.stage(&not_restored), Err(PlanError::NotRestored { .. })), "an approval that restored nothing is not an exit");
    let off =
        on.stage(&Event::Reviewed { at: Timestamp(3), plan: "p".into(), outcome: ApprovalOutcome::AllowedOnce, allowed: ws }).unwrap();
    assert!(!off.active);
    assert!(matches!(
        off.stage(&Event::Reviewed { at: Timestamp(4), plan: "p".into(), outcome: ApprovalOutcome::Rejected, allowed: ws }),
        Err(PlanError::NotOn)
    ));
    // round trip through the CustomFold
    let v = fold_plan(None, "Entered", &e.body().unwrap()).unwrap();
    assert_eq!(tool_plan_mode::project(Some(&v)).unwrap(), on);
    assert!(fold_plan(None, "Exited", &json!({})).is_err());
}
