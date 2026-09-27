//! Schedules: prompts this session receives later.
//!
//! * `schedule_create {prompt, at | every | cron, label?}`: `at` is one RFC 3339 instant, `every`
//!   an interval from now (`30m`, `1h30m`, `1d`), `cron` a 5-field expression evaluated in UTC
//!   ([`spec::Cron`]). Exactly one of the three.
//! * `schedule_list {}`: active schedules with their next fire time, recent finished ones, and
//!   the current UTC time (so the model can compute an `at`).
//! * `schedule_cancel {id}`: agents cancel what agents created; the operator anything.
//!
//! **Class `ReadOnly`**, like `create_goal` and `todo_write`: creating a schedule appends a
//! `schedule/Created` entry and nothing else. The consequence arrives later as an ordinary turn
//! whose every call goes through the decision chain under the policy in force *then*; a schedule
//! carries no authority of its own, reaches no human (`TalksToHuman` is for tools that ask the
//! operator something now) and has no external effect. It is agent self-direction over time,
//! as bounded as the agent's next turn. The fired prompt says who scheduled it.
//!
//! Firing is the [`driver::Scheduler`]'s job: it runs next to the session (daemon, CLI)
//! and fires through an `aho_control::PromptSink`. The host seeds [`driver::SERVICE`]; without it
//! this capability does not build, so a session that nothing will ever wake does not accept
//! schedules. Entries are `Custom{ns: "schedule"}` ([`fold`]); `register` installs
//! [`fold::fold_schedules`] and mirrors the folded set through a `context` hook, as `tool-goals` does.

#![forbid(unsafe_code)]

pub mod driver;
pub mod fold;
pub mod spec;

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

pub use driver::{DRIVER, SERVICE, Scheduler, SchedulerLink};
pub use fold::{Due, Event, Limits, NS, Schedule, ScheduleError, Schedules, Status, fold_schedules, load, project};
pub use spec::{Cron, Spec, SpecError, format_utc, parse_duration, parse_rfc3339};

pub fn schema_create(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": "The prompt this session receives when the schedule fires; write it as a complete instruction."},
            "at": {"type": "string", "description": "Fire once at this RFC 3339 time, e.g. 2026-09-27T09:00:00Z."},
            "every": {"type": "string", "description": "Fire repeatedly at this interval from now, e.g. 30m, 2h, 1h30m, 1d."},
            "cron": {"type": "string", "description": "Fire on a 5-field cron schedule in UTC: minute hour day-of-month month day-of-week, e.g. \"0 9 * * 1-5\"."},
            "label": {"type": "string", "description": "Short name shown in schedule_list."}
        },
        "required": ["prompt"]
    }))
}

pub fn schema_list(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {}}))
}

pub fn schema_cancel(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"id": {"type": "string", "description": "The schedule id from schedule_create or schedule_list."}},
        "required": ["id"]
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "schedule_create",
        "Schedule a prompt for this session later: once (`at`, RFC 3339), repeatedly (`every`, an interval), or on a UTC cron \
         expression (`cron`). Pass exactly one. The session gets the prompt as a new turn when it fires. Times are UTC.",
        Consequence::ReadOnly,
    )
    .schema(schema_create),
    ToolDecl::new(
        "schedule_list",
        "List this session's schedules with their next fire time (UTC), and the current UTC time.",
        Consequence::ReadOnly,
    )
    .schema(schema_list)
    .safe(),
    ToolDecl::new(
        "schedule_cancel",
        "Cancel a schedule so it never fires again. You can cancel schedules an agent created; operator schedules only the operator can.",
        Consequence::ReadOnly,
    )
    .schema(schema_cancel),
];

/// `schedules.*` keys.
pub struct SchedulesConfig {
    pub limits: Limits,
}

impl Config for SchedulesConfig {
    const PREFIX: &'static str = "schedules";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("schedules.max_active", Ty::Int, Some(json!(20)), "Most schedules one session may have active at once."),
            ConfigKey::new(
                "schedules.min_interval_s",
                Ty::Int,
                Some(json!(60)),
                "Shortest interval a repeating schedule may fire at, in seconds (cron's own resolution is one minute).",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let max_active = usize::try_from(cfg_u64(l, "schedules.max_active", 20))
            .ok()
            .filter(|n| *n >= 1)
            .ok_or(ConfigError::Type { key: "schedules.max_active".into(), expected: "a positive integer" })?;
        let min_s = cfg_u64(l, "schedules.min_interval_s", 60);
        let min_interval_ms =
            min_s.checked_mul(1000).ok_or(ConfigError::Type { key: "schedules.min_interval_s".into(), expected: "a number of seconds" })?;
        Ok(SchedulesConfig { limits: Limits { max_active, min_interval_ms } })
    }
}

/// The host's scheduler link, seeded as the [`SERVICE`] service.
pub struct ScheduleDeps {
    pub link: Option<SchedulerLink>,
}

impl Deps for ScheduleDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ScheduleDeps { link: r.service(SERVICE) })
    }
}

pub struct ToolSchedule {
    limits: Limits,
    schedules: Arc<Mutex<Schedules>>,
}

impl Capability for ToolSchedule {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ScheduleDeps;
    type Config = SchedulesConfig;

    fn build(d: ScheduleDeps, cfg: SchedulesConfig) -> Result<Self, BuildError> {
        if d.link.is_none() {
            return Err(BuildError::Unsupported(
                "no scheduler runs next to this session, so schedules could never fire; the schedule tools are off. Run the session under a long-lived host (aho serve) to use them",
            ));
        }
        Ok(ToolSchedule { limits: cfg.limits, schedules: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_schedules);
        let mirror = self.schedules.clone();
        r.context(Arc::new(move |state: &State| {
            // a projection the fold produced always decodes; otherwise keep the last view
            if let Ok(s) = project(state.custom.get(NS)) {
                *lock(&mirror) = s;
            }
            None
        }));
        for decl in TOOLS {
            r.tool(decl, self.exec(decl.name));
        }
    }
}

declare_capability!(
    ToolSchedule,
    "tool-schedule",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = ["schedule/Created", "schedule/Fired", "schedule/Cancelled"],
    doc = "schedule_create / schedule_list / schedule_cancel: prompts this session receives later (at | every | UTC cron), fired by a hosted Scheduler."
);

fn lock(m: &Mutex<Schedules>) -> MutexGuard<'_, Schedules> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn code(e: &ScheduleError) -> &'static str {
    match e {
        ScheduleError::NotYours { .. } => "not_allowed",
        ScheduleError::Unknown(_) => "unknown_schedule",
        ScheduleError::NotActive { .. } => "not_active",
        ScheduleError::TooMany { .. } => "too_many",
        ScheduleError::TooFrequent { .. } => "too_frequent",
        ScheduleError::EmptyPrompt | ScheduleError::NoWhen | ScheduleError::TwoWhens | ScheduleError::Spec(_) => "bad_args",
        ScheduleError::InPast { .. } | ScheduleError::NeverFires(_) => "never_fires",
        ScheduleError::BadEvent(_) | ScheduleError::Tampered { .. } => "schedule_refused",
    }
}

fn refused(e: ScheduleError) -> ExecOutcome {
    ExecOutcome::err(code(&e), e.to_string())
}

fn done(events: &[Event], text: String, extra: Value) -> ExecOutcome {
    let mut list = Vec::with_capacity(events.len());
    for e in events {
        match e.to_json() {
            Ok(v) => list.push(v),
            Err(err) => return refused(err),
        }
    }
    let mut value = match extra {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    value.insert("events".into(), Value::Array(list));
    ExecOutcome::Done { value: Value::Object(value), content: vec![ContentBlock::text(text)], meta: None }
}

/// A string argument; strict-schema empty fillers count as omitted.
fn str_arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Exactly one of `at`, `every`, `cron`.
pub fn spec_from_args(args: &Value, now: Timestamp) -> Result<Spec, ScheduleError> {
    let given: Vec<(&str, String)> = ["at", "every", "cron"].into_iter().filter_map(|k| str_arg(args, k).map(|v| (k, v))).collect();
    match given.as_slice() {
        [] => Err(ScheduleError::NoWhen),
        [("at", v)] => Ok(Spec::At { at: parse_rfc3339(v)? }),
        [("every", v)] => Ok(Spec::Every { every_ms: parse_duration(v)?, anchor: now }),
        [(_, v)] => {
            Cron::parse(v)?;
            Ok(Spec::Cron { expr: v.split_whitespace().collect::<Vec<_>>().join(" ") })
        }
        _ => Err(ScheduleError::TwoWhens),
    }
}

impl ToolSchedule {
    fn exec(&self, name: &'static str) -> ToolExec {
        let schedules = self.schedules.clone();
        let limits = self.limits;
        Arc::new(move |req: ExecRequest| {
            let now = Timestamp::now();
            let out = match name {
                "schedule_create" => create(&schedules, limits, &req.view, now),
                "schedule_list" => list(&schedules, now),
                "schedule_cancel" => cancel(&schedules, &req.view, now),
                other => ExecOutcome::err("unknown_tool", format!("tool-schedule has no tool `{other}`")),
            };
            Box::pin(async move { out })
        })
    }
}

/// Stages `e` on the mirror so later calls in the same step see it.
fn commit(m: &Mutex<Schedules>, e: &Event) -> Result<Schedules, ScheduleError> {
    let mut g = lock(m);
    let next = g.stage(std::slice::from_ref(e))?;
    *g = next.clone();
    Ok(next)
}

pub fn create(m: &Mutex<Schedules>, limits: Limits, view: &ExecView, now: Timestamp) -> ExecOutcome {
    let prompt = str_arg(&view.args, "prompt").unwrap_or_default();
    let label = str_arg(&view.args, "label").unwrap_or_default();
    let spec = match spec_from_args(&view.args, now) {
        Ok(s) => s,
        Err(e) => return refused(e),
    };
    let by = Actor::of(&view.by);
    let planned = lock(m).create(&prompt, &label, spec, &by, now, limits);
    let e = match planned.and_then(|e| commit(m, &e).map(|_| e)) {
        Ok(e) => e,
        Err(e) => return refused(e),
    };
    let Event::Created { schedule: s } = &e else { return refused(ScheduleError::BadEvent("schedule_create planned no creation".into())) };
    let first = s.next_due.map(format_utc).unwrap_or_default();
    let label = if s.label.is_empty() { String::new() } else { format!(" \"{}\"", s.label) };
    let text = format!(
        "Scheduled {}{label}: {}; first fire {first} (UTC). This session receives the prompt as a new turn when it fires. schedule_cancel with id {} stops it.",
        s.id,
        s.spec.describe(),
        s.id
    );
    let snapshot = serde_json::to_value(s).unwrap_or(Value::Null);
    done(std::slice::from_ref(&e), text, json!({ "schedule": snapshot }))
}

pub fn list(m: &Mutex<Schedules>, now: Timestamp) -> ExecOutcome {
    let s = lock(m).clone();
    let active: Vec<&Schedule> = s.active().collect();
    let finished: Vec<&Schedule> = s.all().iter().filter(|x| x.status != Status::Active).rev().take(10).collect();
    let mut text = format!("Now: {} (UTC).\n", format_utc(now));
    if active.is_empty() {
        text.push_str("No active schedules.");
    } else {
        text.push_str("Active:\n");
        for x in &active {
            text.push_str(&format!("- {}\n", x.summary()));
        }
    }
    if !finished.is_empty() {
        text.push_str("\nRecently finished:\n");
        for x in &finished {
            text.push_str(&format!("- {}\n", x.summary()));
        }
    }
    let value = json!({ "now": format_utc(now), "schedules": serde_json::to_value(s.all()).unwrap_or(Value::Null) });
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text.trim_end().to_string())], meta: None }
}

pub fn cancel(m: &Mutex<Schedules>, view: &ExecView, now: Timestamp) -> ExecOutcome {
    let Some(id) = str_arg(&view.args, "id") else { return ExecOutcome::err("bad_args", "`id` is required; schedule_list shows the ids") };
    let by = Actor::of(&view.by);
    let planned = lock(m).cancel(&id, &by, now);
    match planned.and_then(|e| commit(m, &e).map(|_| e)) {
        Ok(e) => done(std::slice::from_ref(&e), format!("Cancelled {id}; it will not fire again."), json!({ "id": id })),
        Err(e) => refused(e),
    }
}
