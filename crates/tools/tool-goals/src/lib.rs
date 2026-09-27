//! The model-facing goal tools over `aho-goals`.
//!
//! * `create_goal {objective, title?, max_rounds?}`: an agent creates an agent goal; an operator a
//!   user goal (which pauses an active agent goal: user goals have priority).
//! * `get_goal {id?}`: without an id, the earliest Active goal, or a sentence saying none is
//!   active (and which goals are paused or blocked).
//! * `update_goal {id, action, revision?, objective?, title?, max_rounds?, reason?}`: `edit`,
//!   `pause`, `resume`, `complete`, `block`, `clear`. An agent updating a goal that is not the
//!   current one is told which goal is active. `revision`, when given, must be the current one.
//!
//! All three are `ReadOnly`: they change only the session log.
//!
//! # Contract with the session: `value.events`
//!
//! A successful mutation returns `ExecOutcome::Done { value: { "events": [ { "ns": "Goal",
//! "name": "Changed" | "Round", "body": … } ], "goal": <snapshot> }, … }`; the session appends each
//! entry after the `ToolResult` as `EntryKind::Custom`. `register` installs `aho_goals::fold_goals`
//! for the `Goal` namespace, which recomputes every transition and fails replay on a bad one. The
//! tools read the folded goals through a `context` hook before every model request and apply their
//! own events so later calls in the same step see them.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, MutexGuard};

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, Config, ConfigError, ExecOutcome, ExecRequest, ExecView, Manifest, Registrar,
    ToolDecl, ToolExec, cfg_u64, declare_capability,
};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_goals::{
    Actor, BlockReason, DEFAULT_MAX_ROUNDS, Event, Goal, GoalError, Goals, MODEL_REPORTED, NS, Op, Phase, fold_goals, project,
};
use aho_log::State;
use aho_types::{Consequence, ContentBlock, RootSchema, Timestamp, Value};
use serde_json::{Map, json};

pub fn schema_create(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "objective": {"type": "string", "description": "The concrete result that counts as done."},
            "title": {"type": "string", "description": "Short name; defaults to the objective's first line."},
            "max_rounds": {"type": "integer", "minimum": 1, "description": "Continuation rounds allowed before the goal blocks."}
        },
        "required": ["objective"]
    }))
}

pub fn schema_get(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"id": {"type": "string", "description": "A goal id; omit for the current (active) goal."}}
    }))
}

pub fn schema_update(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "The goal id from get_goal."},
            "action": {"type": "string", "enum": ["edit", "pause", "resume", "complete", "block", "clear"]},
            "revision": {"type": "integer", "description": "The revision from get_goal; refused if the goal moved on."},
            "objective": {"type": "string", "description": "edit: the new objective."},
            "title": {"type": "string", "description": "edit: the new title."},
            "max_rounds": {"type": "integer", "minimum": 1, "description": "edit: the new round budget."},
            "reason": {"type": "string", "description": "block: the concrete condition that blocks progress."}
        },
        "required": ["id", "action"]
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "create_goal",
        "Create a long-running goal for this session: one objective that takes several rounds of work. Only one goal is active at a \
         time. Do not create goals for routine single-turn work.",
        Consequence::ReadOnly,
    )
    .schema(schema_create),
    ToolDecl::new(
        "get_goal",
        "Read the current goal (or one goal by id): phase, revision, rounds used, objective. Call it before update_goal.",
        Consequence::ReadOnly,
    )
    .schema(schema_get)
    .safe(),
    ToolDecl::new(
        "update_goal",
        "Update the current goal: edit, pause, resume, complete (only once the objective is achieved), block (name the \
         concrete blocking condition in reason), or clear. You may complete or block a goal you are driving; edit, pause, resume \
         and clear only goals you created.",
        Consequence::ReadOnly,
    )
    .schema(schema_update),
];

/// `goals.*` keys.
pub struct GoalsConfig {
    pub max_rounds: u32,
}

impl Config for GoalsConfig {
    const PREFIX: &'static str = "goals";
    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            "goals.max_rounds",
            Ty::Int,
            Some(json!(DEFAULT_MAX_ROUNDS)),
            "Default continuation rounds a new goal may use before it blocks with round_limit.",
        )]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let v = cfg_u64(l, "goals.max_rounds", DEFAULT_MAX_ROUNDS as u64);
        let max_rounds = u32::try_from(v)
            .ok()
            .filter(|v| *v >= 1)
            .ok_or(ConfigError::Type { key: "goals.max_rounds".into(), expected: "a positive integer" })?;
        Ok(GoalsConfig { max_rounds })
    }
}

pub struct ToolGoals {
    max_rounds: u32,
    goals: Arc<Mutex<Goals>>,
}

impl Capability for ToolGoals {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = GoalsConfig;

    fn build(_: (), cfg: GoalsConfig) -> Result<Self, BuildError> {
        Ok(ToolGoals { max_rounds: cfg.max_rounds, goals: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_goals);
        let goals = self.goals.clone();
        r.context(Arc::new(move |state: &State| {
            // a projection the fold produced always decodes; otherwise keep the last view
            if let Ok(g) = project(state.custom.get(NS)) {
                *lock(&goals) = g;
            }
            None
        }));
        for decl in TOOLS {
            r.tool(decl, self.exec(decl.name));
        }
    }
}

declare_capability!(
    ToolGoals,
    "tool-goals",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = ["Goal/Changed", "Goal/Round"],
    doc = "create_goal / get_goal / update_goal: one active goal, fold-validated transitions, user goals first."
);

fn lock(m: &Mutex<Goals>) -> MutexGuard<'_, Goals> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn code(e: &GoalError) -> &'static str {
    match e {
        GoalError::NotYours { .. } => "not_allowed",
        GoalError::AnotherActive { .. } => "another_active",
        GoalError::StaleRevision { .. } => "stale_revision",
        GoalError::UnknownGoal(_) | GoalError::NoGoal | GoalError::Cleared(_) => "unknown_goal",
        GoalError::Illegal { .. } | GoalError::RoundsExhausted { .. } => "illegal_transition",
        _ => "goal_refused",
    }
}

fn refused(e: GoalError) -> ExecOutcome {
    ExecOutcome::err(code(&e), e.to_string())
}

fn goal_json(g: Option<&Goal>) -> Value {
    match g.map(serde_json::to_value) {
        Some(Ok(v)) => v,
        _ => Value::Null,
    }
}

fn done(events: &[Event], text: String, goal: Option<&Goal>) -> ExecOutcome {
    let mut list = Vec::with_capacity(events.len());
    for e in events {
        match e.to_json() {
            Ok(v) => list.push(v),
            Err(err) => return refused(err),
        }
    }
    let mut value = Map::new();
    value.insert("events".into(), Value::Array(list));
    value.insert("goal".into(), goal_json(goal));
    ExecOutcome::Done { value: Value::Object(value), content: vec![ContentBlock::text(text)], meta: None }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Integers; strict-schema zero fillers count as omitted.
fn u32_arg(args: &Value, key: &str) -> Result<Option<u32>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(0) => Ok(None),
            Some(n) => u32::try_from(n).map(Some).map_err(|_| format!("`{key}` is too large")),
            None => Err(format!("`{key}` must be a positive integer")),
        },
    }
}

/// Plans `op`, stages it, commits it to the view, returns the events and the goal they leave.
fn commit(goals: &Mutex<Goals>, target: Option<&str>, op: Op, by: &Actor) -> Result<(Vec<Event>, Goals), GoalError> {
    let mut g = lock(goals);
    let events = g.plan(target, op, by, Timestamp::now())?;
    let next = g.stage(&events)?;
    *g = next.clone();
    Ok((events, next))
}

impl ToolGoals {
    fn exec(&self, name: &'static str) -> ToolExec {
        let goals = self.goals.clone();
        let max_rounds = self.max_rounds;
        Arc::new(move |req: ExecRequest| {
            let out = match name {
                "create_goal" => create(&goals, max_rounds, &req.view),
                "get_goal" => get(&goals, &req.view),
                "update_goal" => update(&goals, &req.view),
                other => ExecOutcome::err("unknown_tool", format!("tool-goals has no tool `{other}`")),
            };
            Box::pin(async move { out })
        })
    }
}

fn create(goals: &Mutex<Goals>, default_rounds: u32, view: &ExecView) -> ExecOutcome {
    let Some(objective) = str_arg(&view.args, "objective") else { return refused(GoalError::EmptyObjective) };
    let max_rounds = match u32_arg(&view.args, "max_rounds") {
        Ok(m) => m.unwrap_or(default_rounds),
        Err(e) => return ExecOutcome::err("bad_args", e),
    };
    let op = Op::Create { title: str_arg(&view.args, "title").unwrap_or_default(), objective, max_rounds, paused: false };
    let by = Actor::of(&view.by);
    match commit(goals, None, op, &by) {
        Ok((events, next)) => {
            let created = events.iter().rev().find_map(|e| match e {
                Event::Changed { op: Op::Create { .. }, goal, .. } => Some(goal.id.clone()),
                _ => None,
            });
            let g = created.as_deref().and_then(|id| next.get(id));
            let mut text = match g {
                Some(g) => format!("Created {}", g.summary()),
                None => "Created the goal.".into(),
            };
            if let Some(paused) = events.iter().find_map(|e| match e {
                Event::Changed { op: Op::Pause, goal, .. } => Some(goal.id.clone()),
                _ => None,
            }) {
                text.push_str(&format!("\n{paused} was paused: user goals take priority."));
            }
            done(&events, text, g)
        }
        Err(e) => refused(e),
    }
}

fn get(goals: &Mutex<Goals>, view: &ExecView) -> ExecOutcome {
    let g = lock(goals).clone();
    if let Some(id) = str_arg(&view.args, "id") {
        return match g.get(&id) {
            Some(goal) => done(&[], goal.summary(), Some(goal)),
            None => refused(GoalError::UnknownGoal(id)),
        };
    }
    if let Some(goal) = g.current() {
        return done(&[], goal.summary(), Some(goal));
    }
    let waiting: Vec<String> = g
        .all()
        .iter()
        .filter(|x| !x.cleared && matches!(x.phase, Phase::Paused | Phase::Blocked))
        .map(|x| format!("{} (\"{}\") is {}", x.id, x.title, x.phase))
        .collect();
    let text = if waiting.is_empty() {
        "No goal is active.".to_string()
    } else {
        format!("No goal is active. {}; update_goal with action resume re-activates one.", waiting.join("; "))
    };
    done(&[], text, None)
}

fn update(goals: &Mutex<Goals>, view: &ExecView) -> ExecOutcome {
    let args = &view.args;
    let Some(id) = str_arg(args, "id") else { return refused(GoalError::NeedsId("update_goal")) };
    let Some(action) = str_arg(args, "action") else {
        return ExecOutcome::err("bad_args", "`action` is required: edit, pause, resume, complete, block or clear");
    };
    let by = Actor::of(&view.by);
    {
        let g = lock(goals);
        let Some(target) = g.get(&id) else { return refused(GoalError::UnknownGoal(id)) };
        if !by.is_user()
            && let Some(cur) = g.current().filter(|c| c.id != id)
        {
            return ExecOutcome::err(
                "not_current",
                format!(
                    "`{id}` is not the current goal: `{}` (\"{}\") is active; nothing was changed. Call get_goal and update the \
                         active goal, or ask the operator to switch goals",
                    cur.id, cur.title
                ),
            );
        }
        match u32_arg(args, "revision") {
            Ok(Some(rev)) if rev != target.revision => {
                return refused(GoalError::StaleRevision { id: id.clone(), have: target.revision, sent: rev });
            }
            Ok(_) => {}
            Err(e) => return ExecOutcome::err("bad_args", e),
        }
    }
    let op = match action.as_str() {
        "edit" => match u32_arg(args, "max_rounds") {
            Ok(max_rounds) => Op::Edit { title: str_arg(args, "title"), objective: str_arg(args, "objective"), max_rounds },
            Err(e) => return ExecOutcome::err("bad_args", e),
        },
        "pause" => Op::Pause,
        "resume" => Op::Resume,
        "complete" => Op::Complete,
        "block" | "blocked" => {
            let Some(message) = str_arg(args, "reason") else { return refused(GoalError::EmptyReason) };
            let code = if by.is_user() { "user_reported" } else { MODEL_REPORTED };
            Op::Block { reason: BlockReason { code: code.into(), message } }
        }
        "clear" => Op::Clear,
        other => {
            return ExecOutcome::err("bad_args", format!("unknown action `{other}`: use edit, pause, resume, complete, block or clear"));
        }
    };
    let tag = op.tag();
    match commit(goals, Some(&id), op, &by) {
        Ok((events, next)) => {
            let g = next.get(&id);
            let mut text = match g {
                Some(g) => {
                    let verb = match tag {
                        "edit" => "edited",
                        "pause" => "paused",
                        "resume" => "resumed",
                        "complete" => "completed",
                        "block" => "blocked",
                        "clear" => "cleared",
                        _ => "updated",
                    };
                    format!("Goal {verb}: {}", g.summary())
                }
                None => format!("Goal {id} updated."),
            };
            if events.len() > 1 {
                text.push_str("\nAnother agent goal was paused: user goals take priority.");
            }
            done(&events, text, g)
        }
        Err(e) => refused(e),
    }
}
