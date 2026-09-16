//! `00`/`10` — workflows: the `ralph` loop. One prompt is fed to the session again after every
//! turn until a reply carries the completion promise or the iteration budget runs out.
//!
//! * `ralph_start {prompt, completion_promise, max_iterations?}` — starts a loop; the first
//!   iteration is fed when the current turn ends. One loop runs at a time.
//! * `ralph_stop {id?, reason?}` — ends it without finishing (agents stop loops agents started).
//! * `ralph_status {id?}` — the running loop (or the latest): iteration, budget, outcome.
//!
//! **Completion** is `<promise>PROMISE</promise>` in the final assistant text of a loop-fed turn
//! (whitespace inside the tags is ignored; case matters). The tags keep a reply that merely
//! *mentions* the promise ("I will print DONE when…") from ending the loop; every fed prompt
//! repeats the exact form. A promise said in the turn that started the loop does not count: only
//! iterations are checked.
//!
//! **Class `ReadOnly`** for the same reason as schedules: starting a loop appends one
//! `workflow/Started` entry; each iteration is an ordinary turn under the policy in force then.
//!
//! State is `Custom{ns: "workflow"}` ([`fold`]); the [`driver::Looper`] is hosted next to the
//! session like the goals round driver it follows (`aho_goals::Goals::next_round`): a pure
//! planner ([`fold::Workflows::next`]) plus an async loop over an `aho_control::PromptSink`.

#![forbid(unsafe_code)]

pub mod driver;
pub mod fold;

use std::sync::{Arc, Mutex, MutexGuard};

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, Config, ConfigError, Deps, ExecOutcome, ExecRequest, ExecView, HandleKind, Manifest,
    MissingDep, Registrar, Resolver, ToolDecl, ToolExec, cfg_u64, declare_capability,
};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_goals::Actor;
use aho_log::State;
use aho_types::{Consequence, ContentBlock, RootSchema, Timestamp, Value};
use serde_json::{Map, json};

pub use driver::{DRIVER, LoopLink, Looper, SERVICE};
pub use fold::{Event, LoopPlan, NS, Outcome, Phase, Workflow, WorkflowError, Workflows, fold_workflows, load, project, promise_in};

pub fn schema_start(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": "The task, given again verbatim after every turn until it is done."},
            "completion_promise": {"type": "string", "description": "Short phrase you will write as <promise>PHRASE</promise> once the task is truly complete, e.g. ALL TESTS PASS."},
            "max_iterations": {"type": "integer", "minimum": 1, "description": "Most iterations before the loop stops unfinished."}
        },
        "required": ["prompt", "completion_promise"]
    }))
}

pub fn schema_stop(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "The loop id; omit for the running loop."},
            "reason": {"type": "string", "description": "Why it stops; recorded in the log."}
        }
    }))
}

pub fn schema_status(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"id": {"type": "string", "description": "The loop id; omit for the running (or latest) loop."}}
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "ralph_start",
        "Start a ralph loop: after this turn ends, the session is given `prompt` again after every turn until a reply of yours ends \
         with <promise>COMPLETION_PROMISE</promise>, or max_iterations run out. Use it for long tasks that need many passes. One \
         loop runs at a time.",
        Consequence::ReadOnly,
    )
    .schema(schema_start),
    ToolDecl::new(
        "ralph_stop",
        "Stop the running ralph loop without completing it. You can stop loops an agent started; operator loops only the operator can.",
        Consequence::ReadOnly,
    )
    .schema(schema_stop),
    ToolDecl::new("ralph_status", "Show the running (or latest) ralph loop: iteration, budget, promise, outcome.", Consequence::ReadOnly)
        .schema(schema_status)
        .safe(),
];

/// `workflows.*` keys.
pub struct WorkflowsConfig {
    pub default_iterations: u32,
    pub max_iterations: u32,
}

impl Config for WorkflowsConfig {
    const PREFIX: &'static str = "workflows";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "workflows.default_iterations",
                Ty::Int,
                Some(json!(10)),
                "Iterations a ralph loop gets when the agent names none.",
            ),
            ConfigKey::new("workflows.max_iterations", Ty::Int, Some(json!(50)), "Most iterations any ralph loop may ask for."),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let int = |key: &str, d: u64| {
            u32::try_from(cfg_u64(l, key, d))
                .ok()
                .filter(|v| *v >= 1)
                .ok_or(ConfigError::Type { key: key.into(), expected: "a positive integer" })
        };
        let max_iterations = int("workflows.max_iterations", 50)?;
        let default_iterations = int("workflows.default_iterations", 10)?.min(max_iterations);
        Ok(WorkflowsConfig { default_iterations, max_iterations })
    }
}

/// The host's loop link, seeded as the [`SERVICE`] service.
pub struct WorkflowDeps {
    pub link: Option<LoopLink>,
}

impl Deps for WorkflowDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(WorkflowDeps { link: r.service(SERVICE) })
    }
}

pub struct ToolWorkflow {
    default_iterations: u32,
    max_iterations: u32,
    workflows: Arc<Mutex<Workflows>>,
}

impl Capability for ToolWorkflow {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = WorkflowDeps;
    type Config = WorkflowsConfig;

    fn build(d: WorkflowDeps, cfg: WorkflowsConfig) -> Result<Self, BuildError> {
        if d.link.is_none() {
            return Err(BuildError::Unsupported(
                "no loop driver runs next to this session, so a ralph loop would never be fed again; the ralph tools are off. Run the session under a long-lived host (aho serve) to use them",
            ));
        }
        Ok(ToolWorkflow { default_iterations: cfg.default_iterations, max_iterations: cfg.max_iterations, workflows: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_workflows);
        let mirror = self.workflows.clone();
        r.context(Arc::new(move |state: &State| {
            if let Ok(w) = project(state.custom.get(NS)) {
                *lock(&mirror) = w;
            }
            None
        }));
        for decl in TOOLS {
            r.tool(decl, self.exec(decl.name));
        }
    }
}

declare_capability!(
    ToolWorkflow,
    "tool-workflow",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = ["workflow/Started", "workflow/Iteration", "workflow/Finished"],
    doc = "ralph_start / ralph_stop / ralph_status: re-feed one prompt after every turn until <promise>…</promise> or the budget ends (00, 10)."
);

fn lock(m: &Mutex<Workflows>) -> MutexGuard<'_, Workflows> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn refused(e: WorkflowError) -> ExecOutcome {
    let code = match &e {
        WorkflowError::AnotherRunning { .. } => "another_running",
        WorkflowError::NotYours { .. } => "not_allowed",
        WorkflowError::Unknown(_) | WorkflowError::NoneRunning => "unknown_loop",
        WorkflowError::TooMany { .. } | WorkflowError::Empty(_) => "bad_args",
        _ => "loop_refused",
    };
    ExecOutcome::err(code, e.to_string())
}

fn done(events: &[Event], text: String, loop_: Option<&Workflow>) -> ExecOutcome {
    let mut list = Vec::with_capacity(events.len());
    for e in events {
        match e.to_json() {
            Ok(v) => list.push(v),
            Err(err) => return refused(err),
        }
    }
    let mut value = Map::new();
    value.insert("events".into(), Value::Array(list));
    value.insert("loop".into(), loop_.and_then(|w| serde_json::to_value(w).ok()).unwrap_or(Value::Null));
    ExecOutcome::Done { value: Value::Object(value), content: vec![ContentBlock::text(text)], meta: None }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

fn commit(m: &Mutex<Workflows>, e: &Event) -> Result<Workflows, WorkflowError> {
    let mut g = lock(m);
    let next = g.stage(std::slice::from_ref(e))?;
    *g = next.clone();
    Ok(next)
}

impl ToolWorkflow {
    fn exec(&self, name: &'static str) -> ToolExec {
        let workflows = self.workflows.clone();
        let (default_iterations, max_iterations) = (self.default_iterations, self.max_iterations);
        Arc::new(move |req: ExecRequest| {
            let now = Timestamp::now();
            let out = match name {
                "ralph_start" => start(&workflows, default_iterations, max_iterations, &req.view, now),
                "ralph_stop" => stop(&workflows, &req.view, now),
                "ralph_status" => status(&workflows, &req.view),
                other => ExecOutcome::err("unknown_tool", format!("tool-workflow has no tool `{other}`")),
            };
            Box::pin(async move { out })
        })
    }
}

pub fn start(m: &Mutex<Workflows>, default_iterations: u32, max_iterations: u32, view: &ExecView, now: Timestamp) -> ExecOutcome {
    let prompt = str_arg(&view.args, "prompt").unwrap_or_default();
    let promise = str_arg(&view.args, "completion_promise").unwrap_or_default();
    let n = match view.args.get("max_iterations").and_then(Value::as_u64) {
        None | Some(0) => default_iterations,
        Some(n) => match u32::try_from(n).ok().filter(|n| *n <= max_iterations) {
            Some(n) => n,
            None => return refused(WorkflowError::TooMany { asked: n, max: max_iterations }),
        },
    };
    let by = Actor::of(&view.by);
    let planned = lock(m).start(&prompt, &promise, n, &by, now);
    match planned.and_then(|e| commit(m, &e).map(|w| (e, w))) {
        Ok((e, w)) => {
            let Event::Started { workflow } = &e else { return refused(WorkflowError::BadEvent("ralph_start planned no start".into())) };
            let text = format!(
                "Started {id}. When this turn ends the session is given the prompt again, up to {n} times, until a reply of yours ends with \
                 <promise>{p}</promise>. Write that only when the task is truly done. ralph_status shows progress; ralph_stop ends the loop.",
                id = workflow.id,
                n = workflow.max_iterations,
                p = workflow.promise
            );
            done(std::slice::from_ref(&e), text, w.get(&workflow.id))
        }
        Err(e) => refused(e),
    }
}

pub fn stop(m: &Mutex<Workflows>, view: &ExecView, now: Timestamp) -> ExecOutcome {
    let by = Actor::of(&view.by);
    let id = str_arg(&view.args, "id");
    let reason = str_arg(&view.args, "reason");
    let planned = lock(m).stop(id.as_deref(), &by, now, reason);
    match planned.and_then(|e| commit(m, &e).map(|w| (e, w))) {
        Ok((e, w)) => {
            let Event::Finished { id, .. } = &e else { return refused(WorkflowError::BadEvent("ralph_stop planned no stop".into())) };
            let text = format!("Stopped {id}; the prompt will not be given again.");
            done(std::slice::from_ref(&e), text, w.get(id))
        }
        Err(e) => refused(e),
    }
}

pub fn status(m: &Mutex<Workflows>, view: &ExecView) -> ExecOutcome {
    let w = lock(m).clone();
    let pick = match str_arg(&view.args, "id") {
        Some(id) => match w.get(&id) {
            Some(x) => Some(x),
            None => return refused(WorkflowError::Unknown(id)),
        },
        None => w.current().or_else(|| w.all().last()),
    };
    match pick {
        Some(x) => done(&[], x.summary(), Some(x)),
        None => done(&[], "No ralph loop has run in this session.".into(), None),
    }
}
