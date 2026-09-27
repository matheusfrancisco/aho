//! The swarm task-graph tools over the `aho-tasks` engine.
//!
//! | tool | class | who |
//! |---|---|---|
//! | `task_seed` | ReadOnly | anyone: plan top-level work, or fill a gap under an expanded task (`parent`) |
//! | `task_expand` | ReadOnly | the agent holding a task: decompose it once; gates are inserted automatically |
//! | `task_claim` | ReadOnly | an agent: start the next ready task and receive its hydrated input |
//! | `task_complete` | ReadOnly | the holder: hand over the artifact (+ contract), or a gate's `inject` |
//! | `task_checkpoint` | ReadOnly | the holder: record progress a salvage can forward |
//! | `task_status` | ReadOnly, concurrency-safe | anyone: the graph, or one task with its artifact |
//! | `task_control` | TalksToHuman | `stop` (own subtree) and `reopen` (failed acceptance) directly; `retry`, `reassign`, `replace`, `salvage` only with the operator's approval (`ApprovalHandle`) |
//!
//! The graph tools are `ReadOnly`: they change only the session log, no world.
//!
//! # Contract with the session: `value.events`
//!
//! No tool writes the log. Every successful call returns
//!
//! ```text
//! ExecOutcome::Done { value: { "events": [ { "ns": "Task", "name": "<Event>", "body": { … } }, … ], …tool fields }, content, meta }
//! ```
//!
//! and the session appends each entry of `value.events`, in order, after the call's `ToolResult`
//! as `EntryKind::Custom { ns, name, body }` (`aho_session::tool_events`). `register` installs the
//! `Task` fold (`Registrar::entry_kind("Task", aho_tasks::fold_tasks)`), so every appended event is
//! validated on append and on replay; an illegal one fails the fold loudly. Refusals return
//! `ExecOutcome::Err` and no events.
//!
//! The tools validate against the folded graph: a `context` hook reads `State.custom["Task"]` before
//! every model request (the log is the source of truth, including after resume and fork), and each
//! call also applies its own events so exclusive calls later in the same step see them. The first
//! mutating call in a process whose folded graph still has running tasks prepends `Task/Resumed`:
//! work started before the resume is stale until its worker shows life.
//!
//! A verify gate's pass also names every invariant covering the files its scope handed over, with
//! its status as the session log has it at that moment ([`verify`]); a Fail or Stale one refuses
//! the pass (`GateError::InvariantNotPassing`).

#![forbid(unsafe_code)]

mod args;
mod verify;

use std::sync::{Arc, Mutex, MutexGuard};

use aho_approval::{ApprovalHandle, AskPolicy};
use aho_cap::{
    BuildError, Capability, CapabilityId, Config, ConfigError, Deps, ExecOutcome, ExecRequest, ExecView, HandleKind, Manifest, MissingDep,
    Registrar, Resolver, ToolDecl, ToolExec, cfg_bool, cfg_u64, declare_capability,
};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::{ApprovalOutcome, LogStore, State};
use aho_policy::{ApprovalCard, ChainOutcome, DecisionReason, render};
use aho_tasks::{
    Actor, Control, Dispatch, Event, GateKind, Graph, HandoffArtifact, Hydrate, InvariantSeen, Kind, NS, Settings, TaskError, TaskId,
    agent_label, fold_tasks, project,
};
use aho_types::{AgentId, Consequence, ContentBlock, Value};
use serde_json::{Map, json};

pub use args::{schema_checkpoint, schema_claim, schema_complete, schema_control, schema_expand, schema_seed, schema_status};

// ───────────────────────────── declaration ─────────────────────────────

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "task_seed",
        "Seed the swarm task graph with top-level tasks (explore, code, synthesize), or fill a gap under an already-expanded task \
         with `parent`. Resending the same id and content is a no-op; the same id with other content is refused. A root gate over \
         all top-level tasks is inserted automatically.",
        Consequence::ReadOnly,
    )
    .schema(schema_seed),
    ToolDecl::new(
        "task_expand",
        "Decompose the task you hold into child tasks (once). A critique gate is inserted over explore/synthesize children and a \
         verify gate over code children; your task returns to the queue as their synthesis join and comes back to you when the \
         gates pass.",
        Consequence::ReadOnly,
    )
    .schema(schema_expand),
    ToolDecl::new(
        "task_claim",
        "Start the next ready task and receive its input, including the artifacts of its completed dependencies. If you already hold \
         a task you get it back.",
        Consequence::ReadOnly,
    )
    .schema(schema_claim),
    ToolDecl::new(
        "task_complete",
        "Hand over the task you hold with an artifact: findings, evidence, edge_cases, validation, open_questions, confidence, and \
         what_i_did_not_check (never empty); code/synthesize tasks in deep mode also need a contract. A gate passes with `covers` \
         naming every task in its scope, or turns misses into work with `inject` instead; a verify gate cannot pass while an \
         invariant covering its files fails or is stale. Ending your turn without completing requeues the task once, then fails it.",
        Consequence::ReadOnly,
    )
    .schema(schema_complete),
    ToolDecl::new(
        "task_checkpoint",
        "Record progress on the task you hold, so a replacement worker can be handed your checkpoint if you stop.",
        Consequence::ReadOnly,
    )
    .schema(schema_checkpoint),
    ToolDecl::new(
        "task_status",
        "Show the swarm task graph (status, blockers, growth), or one task with its full artifact.",
        Consequence::ReadOnly,
    )
    .schema(schema_status)
    .safe(),
    ToolDecl::new(
        "task_control",
        "Control a task. `stop` stops a task in your own subtree; `reopen` re-opens a producer whose contract acceptance check \
         failed for the task you hold. `retry`, `reassign`, `replace` and `salvage` are operator decisions: the operator is asked \
         to approve them.",
        Consequence::TalksToHuman,
    )
    .schema(schema_control),
];

/// The session's own log, seeded by `aho_session::Session::boot` (read-only use here).
pub const LOG_SERVICE: &str = "aho.log";

pub struct TasksDeps {
    pub approval: ApprovalHandle,
    /// Read when a verify gate completes: the invariants registered and checked so far. Absent
    /// outside a session; the gate then judges the checks the context hook last saw.
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for TasksDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Approval];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(TasksDeps { approval: r.handle()?, log: r.service(LOG_SERVICE) })
    }
}

/// `swarm.*` keys.
pub struct SwarmConfig {
    pub settings: Settings,
}

fn cap_u32(l: &Loadout, key: &'static str, default: u32, min: u32) -> Result<u32, ConfigError> {
    let v = cfg_u64(l, key, default as u64);
    u32::try_from(v).ok().filter(|v| *v >= min).ok_or(ConfigError::Type { key: key.into(), expected: "a positive integer" })
}

impl Config for SwarmConfig {
    const PREFIX: &'static str = "swarm";
    fn keys() -> Vec<ConfigKey> {
        let d = Settings::default();
        vec![
            ConfigKey::new(
                "swarm.deep",
                Ty::Bool,
                Some(json!(d.deep)),
                "Deep mode: code and synthesize hand-offs must carry a contract (outputs, units, version, acceptance checks).",
            ),
            ConfigKey::new(
                "swarm.max_members",
                Ty::Int,
                Some(json!(d.max_members)),
                "Hard cap on distinct agents that may take swarm tasks.",
            ),
            ConfigKey::new(
                "swarm.max_live_workers",
                Ty::Int,
                Some(json!(d.max_live_workers)),
                "Live-worker budget: agents holding a running task at the same time.",
            ),
            ConfigKey::new(
                "swarm.max_reclaims",
                Ty::Int,
                Some(json!(d.max_reclaims)),
                "Times a task is reclaimed from a dead worker before it fails.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let d = Settings::default();
        Ok(SwarmConfig {
            settings: Settings {
                deep: cfg_bool(l, "swarm.deep", d.deep),
                max_members: cap_u32(l, "swarm.max_members", d.max_members, 1)?,
                max_live_workers: cap_u32(l, "swarm.max_live_workers", d.max_live_workers, 1)?,
                max_reclaims: cap_u32(l, "swarm.max_reclaims", d.max_reclaims, 0)?,
            },
        })
    }
}

/// The folded graph (and the invariant checks) as the tools last saw them.
#[derive(Default)]
struct Ledger {
    graph: Graph,
    evidence: verify::Evidence,
    synced: bool,
    resume_pending: bool,
}

pub struct ToolTasks {
    approval: ApprovalHandle,
    settings: Settings,
    log: Option<Arc<dyn LogStore>>,
    ledger: Arc<Mutex<Ledger>>,
}

impl Capability for ToolTasks {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = TasksDeps;
    type Config = SwarmConfig;

    fn build(deps: TasksDeps, cfg: SwarmConfig) -> Result<Self, BuildError> {
        Ok(ToolTasks { approval: deps.approval, settings: cfg.settings, log: deps.log, ledger: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_tasks);
        let ledger = self.ledger.clone();
        r.context(Arc::new(move |state: &State| {
            sync(&ledger, state);
            None
        }));
        for decl in TOOLS {
            r.tool(decl, self.exec(decl.name));
        }
    }
}

declare_capability!(
    ToolTasks,
    "tool-tasks",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = [
        "Task/Configured",
        "Task/Seeded",
        "Task/Rescoped",
        "Task/Expanded",
        "Task/Started",
        "Task/Heartbeat",
        "Task/Checkpoint",
        "Task/Completed",
        "Task/Injected",
        "Task/Released",
        "Task/Requeued",
        "Task/Failed",
        "Task/Reclaimed",
        "Task/Control",
        "Task/Reopened",
        "Task/Stopped",
        "Task/Resumed"
    ],
    doc = "Swarm task graph: seed, expand, claim, complete with typed hand-off artifacts, structural gates, operator control."
);

fn lock(m: &Mutex<Ledger>) -> MutexGuard<'_, Ledger> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Adopts the folded projection. A graph the fold produced always decodes; if it ever did not,
/// the tools keep their last view rather than act on nothing.
fn sync(ledger: &Mutex<Ledger>, state: &State) {
    let Ok(g) = project(state.custom.get(NS)) else { return };
    let mut l = lock(ledger);
    if !l.synced {
        l.synced = true;
        l.resume_pending = g.has_running();
    }
    l.graph = g;
    l.evidence = verify::Evidence::from_state(state);
}

// ───────────────────────────── plumbing ─────────────────────────────

fn code(e: &TaskError) -> &'static str {
    match e {
        TaskError::ThinArtifact(_) | TaskError::BadArtifact(_) | TaskError::ContractRequired(_) => "thin_artifact",
        TaskError::Gate { .. } => "gate_refused",
        TaskError::NotOwner { .. } | TaskError::NotInSubtree { .. } | TaskError::OperatorOnly(_) | TaskError::PinnedTo { .. } => {
            "not_allowed"
        }
        TaskError::Cycle(_) => "cycle",
        TaskError::DuplicateNode(_) => "duplicate_task",
        TaskError::UnknownTask(_) | TaskError::UnknownDependency { .. } => "unknown_task",
        TaskError::AtCapacity(_) => "at_capacity",
        _ => "task_refused",
    }
}

fn refused(e: TaskError) -> ExecOutcome {
    ExecOutcome::err(code(&e), e.to_string())
}

fn bad_args(why: String) -> ExecOutcome {
    ExecOutcome::err("bad_args", why)
}

fn done(events: &[Event], text: String, extra: Map<String, Value>) -> ExecOutcome {
    let mut list = Vec::with_capacity(events.len());
    for e in events {
        match e.to_json() {
            Ok(v) => list.push(v),
            Err(err) => return refused(err),
        }
    }
    let mut value = extra;
    value.insert("events".into(), Value::Array(list));
    ExecOutcome::Done { value: Value::Object(value), content: vec![ContentBlock::text(text)], meta: None }
}

fn need_agent(view: &ExecView) -> Result<AgentId, ExecOutcome> {
    Actor::of(&view.by).agent().ok_or_else(|| {
        ExecOutcome::err("not_allowed", format!("{} works tasks only as an agent; this call came from {}", view.tool, Actor::of(&view.by)))
    })
}

/// The graph as the next mutating call will see it: with a pending `Resumed` applied.
fn preview(ledger: &Mutex<Ledger>) -> Graph {
    let l = lock(ledger);
    if !l.resume_pending {
        return l.graph.clone();
    }
    match l.graph.stage(&l.graph.running_stale()) {
        Ok(g) => g,
        Err(_) => l.graph.clone(),
    }
}

/// Runs one mutating request against the ledger: pending `Resumed` / `Configured` events first,
/// then `op`, all staged; commits and returns every event only if all of them are legal. `op`'s
/// second value is handed to `reply` together with the committed graph.
fn mutate<T>(
    ledger: &Mutex<Ledger>,
    settings: &Settings,
    op: impl FnOnce(&Graph) -> Result<(Vec<Event>, T), TaskError>,
    reply: impl FnOnce(&Graph, T) -> (String, Map<String, Value>),
) -> ExecOutcome {
    let mut l = lock(ledger);
    let mut events = if l.resume_pending { l.graph.running_stale() } else { vec![] };
    let staged = l.graph.stage(&events).and_then(|g| {
        let cfg = g.configure(&Actor::System("swarm".into()), settings)?;
        let g = g.stage(&cfg)?;
        events.extend(cfg);
        let (own, out) = op(&g)?;
        let g = g.stage(&own)?;
        events.extend(own);
        Ok((g, out))
    });
    match staged {
        Ok((g, out)) => {
            let (text, extra) = reply(&g, out);
            l.graph = g;
            l.resume_pending = false;
            done(&events, text, extra)
        }
        Err(e) => refused(e),
    }
}

fn plain(r: Result<Vec<Event>, TaskError>) -> Result<(Vec<Event>, ()), TaskError> {
    r.map(|e| (e, ()))
}

fn ready_line(g: &Graph) -> String {
    let ready = g.ready();
    if ready.is_empty() {
        return "Nothing else is ready yet.".into();
    }
    let ids: Vec<String> = ready.iter().map(TaskId::to_string).collect();
    format!("Ready now: {}.", ids.join(", "))
}

fn ids_json(ids: &[TaskId]) -> Value {
    Value::Array(ids.iter().map(|i| Value::String(i.to_string())).collect())
}

impl ToolTasks {
    fn exec(&self, name: &'static str) -> ToolExec {
        let ledger = self.ledger.clone();
        let settings = self.settings.clone();
        let approval = self.approval.clone();
        let log = self.log.clone();
        Arc::new(move |req: ExecRequest| {
            let view = req.view;
            if name == "task_control" {
                let (ledger, settings, approval) = (ledger.clone(), settings.clone(), approval.clone());
                return Box::pin(async move { control(&ledger, &settings, &approval, &view).await });
            }
            let out = match name {
                "task_seed" => seed(&ledger, &settings, &view),
                "task_expand" => expand(&ledger, &settings, &view),
                "task_claim" => claim(&ledger, &settings, &view),
                "task_complete" => complete(&ledger, &settings, log.as_ref(), &view),
                "task_checkpoint" => checkpoint(&ledger, &settings, &view),
                "task_status" => status(&ledger, &view),
                other => ExecOutcome::err("unknown_tool", format!("tool-tasks has no tool `{other}`")),
            };
            Box::pin(async move { out })
        })
    }
}

// ───────────────────────────── tools ─────────────────────────────

fn seed(ledger: &Mutex<Ledger>, settings: &Settings, view: &ExecView) -> ExecOutcome {
    let specs = match args::object(&view.args).and_then(args::seeds) {
        Ok(s) => s,
        Err(e) => return bad_args(e),
    };
    let asked: Vec<TaskId> = specs.iter().map(|s| s.id.clone()).collect();
    let by = Actor::of(&view.by);
    let op = |g: &Graph| {
        let events = g.seed(&by, specs)?;
        let new: Vec<TaskId> = events
            .iter()
            .filter_map(|e| match e {
                Event::Seeded { id, kind, .. } if !kind.is_gate() => Some(id.clone()),
                _ => None,
            })
            .collect();
        Ok((events, new))
    };
    mutate(ledger, settings, op, |g, new| {
        let same: Vec<TaskId> = asked.iter().filter(|a| !new.contains(a)).cloned().collect();
        let mut text = if new.is_empty() {
            "Nothing new: every task was already seeded with the same content.".to_string()
        } else {
            format!("Seeded {}: {}.", new.len(), new.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", "))
        };
        if !same.is_empty() && !new.is_empty() {
            text.push_str(&format!(" Already present: {}.", same.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", ")));
        }
        text.push(' ');
        text.push_str(&ready_line(g));
        let mut extra = Map::new();
        extra.insert("seeded".into(), ids_json(&new));
        extra.insert("unchanged".into(), ids_json(&same));
        (text, extra)
    })
}

fn expand(ledger: &Mutex<Ledger>, settings: &Settings, view: &ExecView) -> ExecOutcome {
    let by = match need_agent(view) {
        Ok(a) => a,
        Err(o) => return o,
    };
    let (parent, children) = match args::object(&view.args).and_then(|o| Ok((TaskId::new(args::req_str(o, "id")?), args::children(o)?))) {
        Ok(x) => x,
        Err(e) => return bad_args(e),
    };
    mutate(
        ledger,
        settings,
        |g| plain(g.expand(by, &parent, children)),
        |g, ()| {
            let gates: Vec<TaskId> = match g.node(&parent) {
                Some(p) => p.depends_on.iter().filter(|d| g.node(d).is_some_and(|n| n.kind.is_gate())).cloned().collect(),
                None => vec![],
            };
            let text = format!(
                "Expanded {parent}. Gates {} review the children before {parent} comes back to you as their synthesis join. {} Call \
             task_claim to take one.",
                gates.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", "),
                ready_line(g)
            );
            let mut extra = Map::new();
            extra.insert("gates".into(), ids_json(&gates));
            (text, extra)
        },
    )
}

fn claim(ledger: &Mutex<Ledger>, settings: &Settings, view: &ExecView) -> ExecOutcome {
    let worker = match need_agent(view) {
        Ok(a) => a,
        Err(o) => return o,
    };
    let op = |g: &Graph| {
        let d = g.dispatch(worker)?;
        let events = match &d {
            Dispatch::Started { events, .. } | Dispatch::Released { events, .. } => events.clone(),
            _ => vec![],
        };
        Ok((events, d))
    };
    mutate(ledger, settings, op, |g, decision| {
        let mut extra = Map::new();
        let text = match &decision {
            Dispatch::Started { id, .. } | Dispatch::Holding(id) => {
                extra.insert("id".into(), json!(id.as_str()));
                let held = if matches!(decision, Dispatch::Holding(_)) { "You already hold this task.\n\n" } else { "" };
                match g.assemble_input(id, Hydrate::Reference) {
                    Ok(input) => format!("{held}{input}"),
                    Err(e) => e.to_string(),
                }
            }
            Dispatch::Released { id, .. } => format!(
                "Gate {id} went back to the queue: work entered its scope after you started it. Nothing else is ready for you; call \
                 task_claim again once running work completes."
            ),
            Dispatch::Idle { running: 0, blocked, reserved: 0 } => format!(
                "Nothing is ready: {blocked} tasks wait on work that failed or was stopped. task_status shows which; the operator can \
                 retry them."
            ),
            Dispatch::Idle { running, blocked, reserved } => format!(
                "Nothing is ready for you: {running} running, {blocked} waiting on dependencies, {reserved} reassigned to other \
                 agents. Call task_claim again after running work completes."
            ),
            Dispatch::AtCapacity(why) => format!("Not started: {why}. Call task_claim again after a worker finishes."),
            Dispatch::Finished => "Every task is completed, failed or stopped. task_status shows the results.".into(),
        };
        (text, extra)
    })
}

/// For a verify gate, every invariant covering the files its scope handed over, with its status
/// as the log has it now (the context hook's view when there is no log); empty for other tasks.
fn invariant_evidence(ledger: &Mutex<Ledger>, log: Option<&Arc<dyn LogStore>>, id: &TaskId) -> Result<Vec<InvariantSeen>, ExecOutcome> {
    let g = preview(ledger);
    if g.node(id).map(|n| n.kind) != Some(Kind::Gate(GateKind::Verify)) {
        return Ok(vec![]);
    }
    let unreadable = |e: String| {
        ExecOutcome::err(
            "task_refused",
            format!(
                "the session log could not be read ({e}), so the invariants covering `{id}` are unknown; the verdict was not recorded \
                 and the gate stays running. `aho session verify` shows where the log breaks"
            ),
        )
    };
    let evidence = match log {
        None => lock(ledger).evidence.clone(),
        Some(log) => {
            let entries = match log.head() {
                Some(h) => log.read_branch(h).map_err(|e| unreadable(e.to_string()))?,
                None => vec![],
            };
            verify::Evidence::from_entries(&entries).map_err(unreadable)?
        }
    };
    Ok(evidence.covering(&g.gate_files(id)))
}

fn complete(ledger: &Mutex<Ledger>, settings: &Settings, log: Option<&Arc<dyn LogStore>>, view: &ExecView) -> ExecOutcome {
    let by = match need_agent(view) {
        Ok(a) => a,
        Err(o) => return o,
    };
    let o = match args::object(&view.args) {
        Ok(o) => o,
        Err(e) => return bad_args(e),
    };
    let id = match args::req_str(o, "id") {
        Ok(s) => TaskId::new(s),
        Err(e) => return bad_args(e),
    };
    match (o.get("artifact"), o.get("inject")) {
        (Some(a), None) if !a.is_null() => {
            let artifact = match HandoffArtifact::parse(a) {
                Ok(a) => a,
                Err(e) => return refused(e),
            };
            let conf = artifact.confidence;
            let invariants = match invariant_evidence(ledger, log, &id) {
                Ok(i) => i,
                Err(o) => return o,
            };
            let judged: Vec<String> = invariants.iter().map(|i| format!("{} ({})", i.id, i.status.as_str())).collect();
            mutate(
                ledger,
                settings,
                |g| plain(g.complete(by, &id, artifact, invariants)),
                |g, ()| {
                    let unblocked: Vec<TaskId> = g.ready();
                    let mut text = format!("Completed {id} (confidence {conf}).");
                    if !judged.is_empty() {
                        text.push_str(&format!(" Invariants covering it: {}.", judged.join(", ")));
                    }
                    text.push(' ');
                    text.push_str(&ready_line(g));
                    let mut extra = Map::new();
                    extra.insert("id".into(), json!(id.as_str()));
                    extra.insert("ready".into(), ids_json(&unblocked));
                    (text, extra)
                },
            )
        }
        (None, Some(_)) => {
            let work = match args::injections(o) {
                Ok(w) => w,
                Err(e) => return bad_args(e),
            };
            let new: Vec<TaskId> = work.iter().map(|w| w.id.clone()).collect();
            mutate(
                ledger,
                settings,
                |g| plain(g.inject_from_gate(by, &id, work)),
                |_, ()| {
                    let text = format!(
                        "Gate {id} injected {}; it returns to the queue and reviews again once that work completes.",
                        new.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", ")
                    );
                    let mut extra = Map::new();
                    extra.insert("injected".into(), ids_json(&new));
                    (text, extra)
                },
            )
        }
        _ => bad_args("pass exactly one of `artifact` (hand-off) or `inject` (a gate turning misses into work)".into()),
    }
}

fn checkpoint(ledger: &Mutex<Ledger>, settings: &Settings, view: &ExecView) -> ExecOutcome {
    let by = match need_agent(view) {
        Ok(a) => a,
        Err(o) => return o,
    };
    let (id, summary, recent) = match args::object(&view.args)
        .and_then(|o| Ok((TaskId::new(args::req_str(o, "id")?), args::req_str(o, "summary")?, args::str_list(o, "recent"))))
    {
        Ok(x) => x,
        Err(e) => return bad_args(e),
    };
    mutate(
        ledger,
        settings,
        |g| plain(g.checkpoint(by, &id, &summary, recent)),
        |_, ()| (format!("Checkpoint recorded for {id}. Keep going; finish with task_complete."), Map::new()),
    )
}

fn status(ledger: &Mutex<Ledger>, view: &ExecView) -> ExecOutcome {
    let g = preview(ledger);
    let id = args::object(&view.args).ok().and_then(|o| args::opt_str(o, "id"));
    let Some(id) = id else {
        let mut extra = Map::new();
        extra.insert("growth".into(), json!(g.growth().to_string()));
        extra.insert("ready".into(), ids_json(&g.ready()));
        return done(&[], g.render_status(), extra);
    };
    let id = TaskId::new(id);
    let Some(n) = g.node(&id) else { return refused(TaskError::UnknownTask(id)) };
    let derived = match g.derived(&id) {
        Some(aho_tasks::Derived::Blocked(on)) => format!("blocked on {}", on.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", ")),
        Some(aho_tasks::Derived::Ready) => "ready".into(),
        _ => n.status.to_string(),
    };
    let mut text = format!("{} ({}): {derived}\n{}", n.id, n.kind, n.content);
    if !n.depends_on.is_empty() {
        text.push_str(&format!("\nDepends on: {}", n.depends_on.iter().map(TaskId::to_string).collect::<Vec<_>>().join(", ")));
    }
    if let Some(o) = n.owner.or(n.last_owner) {
        text.push_str(&format!("\nWorker: {}", agent_label(o)));
    }
    let node = match serde_json::to_value(n) {
        Ok(v) => v,
        Err(e) => return ExecOutcome::err("task_refused", format!("task `{id}` did not serialize: {e}")),
    };
    if let Some(a) = node.get("artifact") {
        text.push_str(&format!("\nArtifact:\n{a:#}"));
    }
    let mut extra = Map::new();
    extra.insert("task".into(), node);
    done(&[], text, extra)
}

async fn control(ledger: &Mutex<Ledger>, settings: &Settings, approval: &ApprovalHandle, view: &ExecView) -> ExecOutcome {
    let o = match args::object(&view.args) {
        Ok(o) => o,
        Err(e) => return bad_args(e),
    };
    let (id, op) = match (args::req_str(o, "id"), args::req_str(o, "op")) {
        (Ok(i), Ok(op)) => (TaskId::new(i), op),
        (Err(e), _) | (_, Err(e)) => return bad_args(e),
    };
    let by = Actor::of(&view.by);
    let reason = args::opt_str(o, "reason");
    match op.as_str() {
        "stop" => mutate(ledger, settings, |g| plain(g.stop(&by, &id)), |_, ()| (format!("Stopped {id}."), Map::new())),
        "reopen" => {
            let agent = match need_agent(view) {
                Ok(a) => a,
                Err(o) => return o,
            };
            let Some(reason) = reason else {
                return bad_args("`reason` is required for reopen: name the acceptance check that failed and its output".into());
            };
            let consumer = match args::opt_str(o, "consumer") {
                Some(c) => TaskId::new(c),
                None => match preview(ledger).held_by(agent) {
                    Some(n) => n.id.clone(),
                    None => return refused(TaskError::BadEvent("you hold no task; name the consuming task in `consumer`".into())),
                },
            };
            mutate(
                ledger,
                settings,
                |g| plain(g.reopen(agent, &consumer, &id, &reason)),
                |_, ()| (format!("Re-opened {id}; {consumer} returns to the queue until {id} delivers again."), Map::new()),
            )
        }
        other => {
            let Some(ctl) = Control::parse(other) else {
                return bad_args(format!("unknown op `{other}`: use stop, reopen, retry, reassign, replace or salvage"));
            };
            let to = match args::agent(o, "to") {
                Ok(t) => t,
                Err(e) => return bad_args(e),
            };
            if by != Actor::Operator {
                // do not ask a human to approve something the graph would refuse anyway
                if let Err(e) = preview(ledger).control(&Actor::Operator, &id, ctl, to, None) {
                    return refused(e);
                }
                if approval.0.policy() == AskPolicy::Never {
                    return ExecOutcome::err(
                        "approval_off",
                        format!(
                            "{} needs the operator's approval and prompts are off (policy.ask = never); nothing was changed. The \
                             operator can run it from their surface",
                            ctl.as_str()
                        ),
                    );
                }
                let reason = DecisionReason::ToolCheck(format!("{} on task `{id}` is an operator decision.", ctl.as_str()));
                let rendered = render(&ChainOutcome::Ask(reason.clone()), &view.tool);
                let card = ApprovalCard {
                    tool: view.tool.to_string(),
                    class: view.class,
                    args: view.args.clone(),
                    reason,
                    world: view.policy.world.clone(),
                    suggested_cut: None,
                    rendered,
                };
                let decided = approval.0.request(card).await;
                match decided.outcome {
                    ApprovalOutcome::AllowedOnce => {}
                    ApprovalOutcome::Rejected => {
                        return ExecOutcome::err(
                            "approval_rejected",
                            format!("the operator declined to {} {id}; nothing was changed", ctl.as_str()),
                        );
                    }
                    ApprovalOutcome::Cancelled => {
                        return ExecOutcome::err(
                            "approval_cancelled",
                            format!("the approval for {} {id} was cancelled; nothing was changed", ctl.as_str()),
                        );
                    }
                    ApprovalOutcome::Unavailable => {
                        return ExecOutcome::err(
                            "approval_unavailable",
                            format!("no operator could be reached to approve {} {id}; nothing was changed", ctl.as_str()),
                        );
                    }
                }
            }
            let target = to.map_or(String::new(), |t| format!(" for {}", agent_label(t)));
            let why = reason.map_or(String::new(), |r| format!(" ({r})"));
            mutate(
                ledger,
                settings,
                |g| plain(g.control(&Actor::Operator, &id, ctl, to, None)),
                |_, ()| (format!("{} applied to {id}{target}{why}; it is queued again.", ctl.as_str()), Map::new()),
            )
        }
    }
}
