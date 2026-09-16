//! `04`/`10` — plan mode: preset `read-only` plus a prompt section; enforcement is the lattice.
//!
//! * `enter_plan_mode {reason?}` — narrows the live session policy to `allowed ∩ {ReadOnly,
//!   TalksToHuman}` through the session ceiling (`aho_control::SessionCeiling`, logged as a
//!   `Policy/Epoch` by `System("plan-mode")`). Narrowing is a meet, so it needs no approval, and
//!   children spawned meanwhile inherit it (their policy derives from the session's). The policy
//!   in force before is recorded in `plan/Entered.restore`: that is what leaving restores.
//! * `exit_plan_mode {plan}` — presents the plan to the operator. The ceiling raises the card on
//!   the session's approval channel itself and widens back only on the operator's `AllowedOnce`
//!   (`Policy/Epoch{by: Operator, approval_ref}`); the tool never decides it was approved. A
//!   rejection, a cancel, or no approval channel leaves the session read-only and says so.
//!
//! On top of the lattice, a guard (decision-chain step 4, deny-only) refuses every call outside
//! `{ReadOnly, TalksToHuman}` while plan mode is on, so a write is refused outright with the way
//! out named, instead of turning into a per-call approval prompt (step 6 asks whenever the
//! policy still allows `TalksToHuman`). A `pre_execute` hook marks `enter_plan_mode` as entering
//! the moment it is admitted, so calls the model issued after it in the same step are refused too.
//!
//! Classes: `enter_plan_mode` is `ReadOnly` (it only takes authority away); `exit_plan_mode` is
//! `TalksToHuman` (it asks the operator). Entries are `Custom{ns: "plan"}` ([`fold`]).
//!
//! Needs the session to seed [`aho_control::CEILING_SERVICE`]; without it this capability does
//! not build (a plan mode that cannot narrow the policy would only pretend).

#![forbid(unsafe_code)]

pub mod fold;

use std::sync::{Arc, Mutex, MutexGuard};

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, Deps, ExecOutcome, ExecRequest, ExecView, HandleKind, Manifest, MissingDep,
    PreDecision, Registrar, Resolver, ToolDecl, ToolExec, declare_capability,
};
use aho_control::{CEILING_SERVICE, CeilingHandle, SessionCeiling, Widen};
use aho_goals::Actor;
use aho_log::{ApprovalOutcome, EntryKind, LogStore, State};
use aho_types::{CallId, Consequence, ConsequenceSet, ContentBlock, RootSchema, Timestamp, Value};
use serde_json::{Map, json};

pub use fold::{Event, NS, PlanError, PlanMode, fold_plan, load, project};

/// What runs in plan mode: reading, and asking the operator.
pub const PLAN_CUT: ConsequenceSet = ConsequenceSet::EMPTY.with(Consequence::ReadOnly).with(Consequence::TalksToHuman);
/// The principal of the narrowing epoch: `System("plan-mode")`.
pub const MODE: &str = "plan-mode";

pub fn schema_enter(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"reason": {"type": "string", "description": "Why you want to plan first; shown to the operator."}}
    }))
}

pub fn schema_exit(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"plan": {"type": "string", "description": "The plan: concrete steps, the files they touch, risks and how you will check the result."}},
        "required": ["plan"]
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "enter_plan_mode",
        "Switch this session to plan mode: only read-only tools (and questions to the operator) run until the operator approves a plan \
         you present with exit_plan_mode. Use it before large or risky changes.",
        Consequence::ReadOnly,
    )
    .schema(schema_enter),
    ToolDecl::new(
        "exit_plan_mode",
        "Present your plan to the operator and ask to leave plan mode. Only their approval restores the policy from before plan mode; \
         if they reject it, you stay in plan mode and should revise the plan.",
        Consequence::TalksToHuman,
    )
    .schema(schema_exit),
];

/// The session's ceiling, seeded as [`CEILING_SERVICE`].
pub struct PlanDeps {
    pub ceiling: Option<CeilingHandle>,
}

impl Deps for PlanDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(PlanDeps { ceiling: r.service(CEILING_SERVICE) })
    }
}

/// The fold's view plus this capability's own events whose call has not been logged yet (a
/// guard must not open between a tool's decision and the session appending its result).
#[derive(Default)]
struct Mirror {
    folded: PlanMode,
    local: Vec<(CallId, Event)>,
    entering: Option<CallId>,
}

impl Mirror {
    fn effective(&self) -> PlanMode {
        let mut m = self.folded.clone();
        for (_, e) in &self.local {
            if let Ok(n) = m.stage(e) {
                m = n;
            }
        }
        m
    }
    fn locked(&self) -> bool {
        self.entering.is_some() || self.effective().active
    }
}

#[derive(Clone)]
pub struct ToolPlanMode {
    ceiling: Arc<dyn SessionCeiling>,
    mirror: Arc<Mutex<Mirror>>,
}

fn lock(m: &Mutex<Mirror>) -> MutexGuard<'_, Mirror> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Capability for ToolPlanMode {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = PlanDeps;
    type Config = ();

    fn build(d: PlanDeps, _: ()) -> Result<Self, BuildError> {
        let ceiling = d.ceiling.ok_or(BuildError::Unsupported(
            "this session does not expose its policy ceiling (`aho.ceiling`), so plan mode could not narrow it and is off; a read-only preset (`aho preset read-only`) gives the same restriction",
        ))?;
        Ok(ToolPlanMode { ceiling: ceiling.0, mirror: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_plan);
        let mirror = self.mirror.clone();
        r.context(Arc::new(move |state: &State| {
            let mut m = lock(&mirror);
            if let Ok(p) = project(state.custom.get(NS)) {
                m.folded = p;
            }
            m.local.retain(|(c, _)| state.open_calls.contains(c));
            if m.entering.as_ref().is_some_and(|c| !state.open_calls.contains(c)) {
                m.entering = None;
            }
            context_text(&m.folded)
        }));
        let mirror = self.mirror.clone();
        r.pre_execute(Arc::new(move |view: &ExecView| {
            if view.tool == "enter_plan_mode" {
                lock(&mirror).entering = Some(view.call.clone());
            }
            PreDecision::Allow
        }));
        let mirror = self.mirror.clone();
        r.guard(Arc::new(move |view: &ExecView| {
            (lock(&mirror).locked() && !PLAN_CUT.contains(view.class)).then(|| {
                format!(
                    "plan mode is on: `{}` is {} and only read-only tools and questions to the operator run until the operator approves a plan; call exit_plan_mode with your plan",
                    view.tool,
                    view.class.name()
                )
            })
        }));
        for decl in TOOLS {
            let me = self.clone();
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move {
                    match req.view.tool.as_str() {
                        "enter_plan_mode" => me.enter(&req.view),
                        "exit_plan_mode" => me.exit(&req.view).await,
                        other => ExecOutcome::err("unknown_tool", format!("tool-plan-mode has no tool `{other}`")),
                    }
                })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolPlanMode,
    "tool-plan-mode",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = ["plan/Entered", "plan/Reviewed"],
    doc = "enter_plan_mode / exit_plan_mode: narrow the session to read-only; only an operator-approved plan widens it back (04, 10)."
);

/// The dynamic prompt section: logged only when it changes, so it costs nothing while it holds.
pub fn context_text(m: &PlanMode) -> Option<String> {
    if m.active {
        let why = m.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
        Some(format!(
            "Plan mode is on (entered by {}{why}). You may read, search and ask the operator questions; calls that would change the \
             workspace or the world are refused. Explore, then call exit_plan_mode with a concrete plan — steps, files, risks, how you \
             will check the result. The operator approves it before anything changes; if they reject it, revise it and ask again.",
            m.entered_by.as_ref().map(|a| a.to_string()).unwrap_or_else(|| "?".into())
        ))
    } else if m.times_entered > 0 {
        Some(format!("Plan mode is off: {}. The session policy applies again.", m.left_how()))
    } else {
        None
    }
}

fn done(event: &Event, text: String) -> ExecOutcome {
    let v = match event.to_json() {
        Ok(v) => v,
        Err(e) => return ExecOutcome::err("plan_refused", e.to_string()),
    };
    let mut value = Map::new();
    value.insert("events".into(), Value::Array(vec![v]));
    ExecOutcome::Done { value: Value::Object(value), content: vec![ContentBlock::text(text)], meta: None }
}

impl ToolPlanMode {
    /// The effective plan mode (fold plus this capability's not-yet-logged decisions).
    pub fn state(&self) -> PlanMode {
        lock(&self.mirror).effective()
    }

    pub fn enter(&self, view: &ExecView) -> ExecOutcome {
        let out = self.try_enter(view);
        let mut m = lock(&self.mirror);
        if m.entering.as_ref() == Some(&view.call) {
            m.entering = None;
        }
        out
    }

    fn try_enter(&self, view: &ExecView) -> ExecOutcome {
        if self.state().active {
            return ExecOutcome::err("already_on", "plan mode is already on; explore, then call exit_plan_mode with your plan");
        }
        let before = self.ceiling.view();
        if !before.allowed.contains(Consequence::TalksToHuman) {
            return ExecOutcome::err(
                "no_approver",
                "plan mode ends only when the operator approves a plan, and this session's policy does not allow asking the operator, so plan mode was not entered. Work within the current policy",
            );
        }
        let reason = view.args.get("reason").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let after = match self.ceiling.narrow(MODE, PLAN_CUT) {
            Ok(v) => v,
            Err(e) => {
                return ExecOutcome::err(
                    "narrow_failed",
                    format!("the session policy could not be narrowed ({e}); plan mode was not entered"),
                );
            }
        };
        let event = match self.state().enter(&Actor::of(&view.by), Timestamp::now(), reason, before.allowed, after.allowed) {
            Ok(e) => e,
            Err(e) => {
                return ExecOutcome::err(
                    "plan_refused",
                    format!(
                        "the policy was narrowed but plan mode could not be recorded ({e}); the session stays narrowed until a preset change"
                    ),
                );
            }
        };
        lock(&self.mirror).local.push((view.call.clone(), event.clone()));
        let text = format!(
            "Plan mode is on. Allowed now: {}. Read and explore; when you have a plan, call exit_plan_mode with it. The operator's approval restores: {}.",
            after.allowed, before.allowed
        );
        done(&event, text)
    }

    pub async fn exit(&self, view: &ExecView) -> ExecOutcome {
        let m = self.state();
        if !m.active {
            return ExecOutcome::err("not_on", "plan mode is not on; nothing to leave");
        }
        let plan = view.args.get("plan").and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
        if plan.is_empty() {
            return ExecOutcome::err("bad_args", "`plan` is empty, so nothing was sent to the operator; write the plan you want approved");
        }
        let rendered = format!(
            "The agent finished planning and asks to leave plan mode.\n\n{plan}\n\nApprove to restore {} and let it carry out the plan. Reject to keep it read-only; it will revise the plan.",
            m.restore
        );
        let w = self
            .ceiling
            .request_widen(Widen { tool: "exit_plan_mode".into(), to: m.restore, args: json!({ "plan": plan }), rendered })
            .await;
        let event = match m.review(Timestamp::now(), &plan, w.outcome, w.view.allowed) {
            Ok(e) => e,
            Err(e) => {
                return ExecOutcome::err(
                    "plan_refused",
                    format!("the operator's answer could not be recorded ({e}); plan mode stays on and writes stay refused"),
                );
            }
        };
        lock(&self.mirror).local.push((view.call.clone(), event.clone()));
        let text = match w.outcome {
            ApprovalOutcome::AllowedOnce => {
                format!("The operator approved the plan. Plan mode is off; allowed again: {}. Carry out the plan.", w.view.allowed)
            }
            ApprovalOutcome::Rejected => "The operator rejected this plan, so plan mode stays on and nothing that changes the workspace will run. \
                 Ask what they want changed, revise the plan, and call exit_plan_mode again."
                .into(),
            ApprovalOutcome::Cancelled => {
                "The approval was cancelled before anyone answered, so plan mode stays on. Present the plan again when the operator is back.".into()
            }
            ApprovalOutcome::Unavailable => "No approval channel is available, so the plan could not be approved and plan mode stays on. \
                 Do not ask again; tell the user the plan and that approving it needs an interactive session."
                .into(),
        };
        done(&event, text)
    }
}

/// Operator command (`/plan`): narrows the session and records `plan/Entered` by the operator.
/// `log` is the session's own log (`Session::log`), read to fold the current mode; `announce`
/// appends by the operator through the session so attached surfaces see it (`Session::append_entry`).
/// Leaving still goes through the agent's `exit_plan_mode` card or a preset change (`aho preset …`):
/// a widening needs a logged decision.
pub fn enter_by_operator(
    ceiling: &dyn SessionCeiling,
    log: &dyn LogStore,
    announce: &dyn Fn(EntryKind) -> Result<(), String>,
    reason: Option<String>,
) -> Result<String, String> {
    let current = fold::load(&aho_control::branch(log)?)?;
    if current.active {
        return Ok("Plan mode is already on.".into());
    }
    let before = ceiling.view();
    if !before.allowed.contains(Consequence::TalksToHuman) {
        return Err("this session's policy does not allow asking the operator, so a plan could never be approved; plan mode was not entered. Use `aho preset read-only` instead".into());
    }
    let after =
        ceiling.narrow(MODE, PLAN_CUT).map_err(|e| format!("the session policy could not be narrowed ({e}); plan mode was not entered"))?;
    let recorded = current
        .enter(&Actor::Operator, Timestamp::now(), reason, before.allowed, after.allowed)
        .and_then(|e| e.to_entry())
        .map_err(|e| e.to_string())
        .and_then(announce);
    recorded.map_err(|e| {
        format!("the policy was narrowed but plan mode could not be recorded ({e}); the session stays narrowed until a preset change")
    })?;
    Ok(format!("Plan mode is on. Allowed now: {}.", after.allowed))
}
