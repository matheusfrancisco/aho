//! `10`/`12` — goals. Transitions are validated in the fold; a bad event fails replay loudly.
//!
//! * Phases `active | paused | blocked | complete`; ops `create | edit | pause | resume | complete |
//!   block | clear`. Every op bumps `revision` and preserves `created_at` / `rounds_started`; `edit`
//!   never changes phase; `clear` leaves a tombstone (the id is never reused).
//! * One Active goal at a time. User goals have priority: a user activating a goal pauses an Active
//!   agent goal; an agent never displaces another Active goal.
//! * An agent may complete or block only what it drives (it created it, or a round was queued for
//!   it); edit, pause, resume and clear only goals it created. Operators may do anything.
//! * Events are `EntryKind::Custom{ns: "Goal", name, body}`: `Goal/Changed{op, goal, by}` carries
//!   the full snapshot after the op, and [`fold_goals`] recomputes it from the previous state — any
//!   mismatch (illegal phase change, skipped revision, tampered counter) is an `Err`.
//!   `Goal/Round{id, revision, round, driver}` records a queued continuation round.
//! * Round driver [`Goals::next_round`]: while active and armed, queue round n+1 with
//!   `source: Goal{id, revision, round}`; hitting `max_rounds` blocks with `round_limit`.

#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;

use aho_log::EntryKind;
use aho_types::{AgentId, Principal, Timestamp, Value};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const NS: &str = "Goal";
pub const DEFAULT_MAX_ROUNDS: u32 = 20;
/// `blocked_reason.code` written by the round driver when the budget runs out.
pub const ROUND_LIMIT: &str = "round_limit";
/// `blocked_reason.code` for a block the model reported.
pub const MODEL_REPORTED: &str = "model_reported";
const TITLE_CHARS: usize = 60;

// ───────────────────────────────── data ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Active => "active",
            Phase::Paused => "paused",
            Phase::Blocked => "blocked",
            Phase::Complete => "complete",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockReason {
    pub code: String,
    pub message: String,
}

/// Who acted. Bodies carry it because a custom fold never sees the entry principal.
/// Serialized as `operator`, `engineer`, `agent:<n>`, `system:<name>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Actor {
    Operator,
    Engineer,
    Agent(AgentId),
    System(String),
}

impl Actor {
    pub fn of(p: &Principal) -> Actor {
        match p {
            Principal::Operator => Actor::Operator,
            Principal::Engineer => Actor::Engineer,
            Principal::Agent { id, .. } => Actor::Agent(*id),
            Principal::System { name } => Actor::System(name.clone()),
        }
    }
    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Actor::Agent(a) => Some(*a),
            _ => None,
        }
    }
    /// Operator or engineer: a user goal.
    pub fn is_user(&self) -> bool {
        matches!(self, Actor::Operator | Actor::Engineer)
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::Operator => f.write_str("operator"),
            Actor::Engineer => f.write_str("engineer"),
            Actor::Agent(a) => write!(f, "agent:{}", a.0),
            Actor::System(n) => write!(f, "system:{n}"),
        }
    }
}

fn parse_agent(s: &str) -> Option<AgentId> {
    s.strip_prefix("agent:").unwrap_or(s).parse::<u128>().ok().map(AgentId)
}

impl FromStr for Actor {
    type Err = String;
    fn from_str(s: &str) -> Result<Actor, String> {
        match s {
            "operator" => Ok(Actor::Operator),
            "engineer" => Ok(Actor::Engineer),
            _ => match s.strip_prefix("system:") {
                Some(n) => Ok(Actor::System(n.to_string())),
                None => parse_agent(s).map(Actor::Agent).ok_or_else(|| format!("unknown actor `{s}`")),
            },
        }
    }
}

impl Serialize for Actor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

mod agent_str {
    use super::*;
    pub fn serialize<S: Serializer>(a: &AgentId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("agent:{}", a.0))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<AgentId, D::Error> {
        let s = String::deserialize(d)?;
        parse_agent(&s).ok_or_else(|| D::Error::custom(format!("`{s}` is not an agent id")))
    }
    pub mod opt {
        use super::*;
        pub fn serialize<S: Serializer>(a: &Option<AgentId>, s: S) -> Result<S::Ok, S::Error> {
            match a {
                Some(a) => s.serialize_some(&format!("agent:{}", a.0)),
                None => s.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<AgentId>, D::Error> {
            match Option::<String>::deserialize(d)? {
                None => Ok(None),
                Some(s) => parse_agent(&s).map(Some).ok_or_else(|| D::Error::custom(format!("`{s}` is not an agent id"))),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub revision: u32,
    pub phase: Phase,
    pub title: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<BlockReason>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub rounds_started: u32,
    pub max_rounds: u32,
    /// Who created it: a user (operator/engineer) goal, an agent goal or a system goal.
    pub owner: Actor,
    /// The agent continuation rounds run for; the creator when an agent created it.
    #[serde(default, with = "agent_str::opt", skip_serializing_if = "Option::is_none")]
    pub driver: Option<AgentId>,
    /// Tombstone: cleared goals stay in the projection so their id is never reused.
    #[serde(default)]
    pub cleared: bool,
}

impl Goal {
    /// One-line status plus the objective, for model-facing results.
    pub fn summary(&self) -> String {
        let state = if self.cleared { "cleared".to_string() } else { self.phase.to_string() };
        let mut s = format!(
            "{} · {state} · round {}/{} · revision {} · owner {} — {}\nObjective: {}",
            self.id, self.rounds_started, self.max_rounds, self.revision, self.owner, self.title, self.objective
        );
        if let Some(b) = &self.blocked_reason {
            s.push_str(&format!("\nBlocked ({}): {}", b.code, b.message));
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create {
        #[serde(default)]
        title: String,
        objective: String,
        max_rounds: u32,
        #[serde(default)]
        paused: bool,
    },
    Edit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objective: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rounds: Option<u32>,
    },
    Pause,
    Resume,
    Complete,
    Block {
        reason: BlockReason,
    },
    Clear,
}

impl Op {
    pub fn tag(&self) -> &'static str {
        match self {
            Op::Create { .. } => "create",
            Op::Edit { .. } => "edit",
            Op::Pause => "pause",
            Op::Resume => "resume",
            Op::Complete => "complete",
            Op::Block { .. } => "block",
            Op::Clear => "clear",
        }
    }
}

// ───────────────────────────────── errors ─────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalError {
    UnknownGoal(String),
    NeedsId(&'static str),
    NoGoal,
    GoalExists(String),
    DuplicateId(String),
    Cleared(String),
    EmptyObjective,
    EmptyReason,
    BadRounds { max_rounds: u32, rounds_started: u32 },
    NothingToEdit(String),
    Illegal { id: String, op: &'static str, phase: Phase },
    RoundsExhausted { id: String, max_rounds: u32 },
    AnotherActive { active: String, title: String },
    NotYours { id: String, owner: Actor, op: &'static str },
    StaleRevision { id: String, have: u32, sent: u32 },
    BadRound { id: String, expected: u32, got: u32 },
    Tampered { id: String, op: &'static str },
    BadEvent(String),
}

impl fmt::Display for GoalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GoalError::*;
        match self {
            UnknownGoal(id) => write!(f, "there is no goal `{id}`; nothing was changed. get_goal shows the current goal"),
            NeedsId(op) => write!(f, "{op} needs the goal id; nothing was changed. get_goal shows the current goal"),
            NoGoal => write!(f, "there is no goal to update; nothing was changed. create_goal starts one"),
            GoalExists(id) => write!(f, "goal `{id}` already exists; nothing was changed"),
            DuplicateId(id) => write!(f, "goal id `{id}` was already used (possibly by a cleared goal); ids are never reused"),
            Cleared(id) => write!(f, "goal `{id}` was cleared; nothing was changed. create_goal starts a new one"),
            EmptyObjective => write!(f, "a goal needs a non-empty objective; nothing was changed. Say what done looks like"),
            EmptyReason => write!(f, "blocking a goal needs the concrete blocking condition as the reason; nothing was changed"),
            BadRounds { max_rounds, rounds_started } => write!(
                f,
                "max_rounds {max_rounds} is invalid: it must be at least 1 and not below the {rounds_started} rounds already started; \
                 nothing was changed"
            ),
            NothingToEdit(id) => {
                write!(f, "the edit would not change goal `{id}`; nothing was changed. Pass a new title, objective or max_rounds")
            }
            Illegal { id, op, phase } => {
                let allowed = match *op {
                    "pause" | "block" => "an active goal",
                    "resume" => "an active, paused or blocked goal",
                    "complete" => "a goal that is not already complete",
                    "edit" => "a goal that is not complete",
                    _ => "another phase",
                };
                write!(f, "goal `{id}` is {phase}; {op} applies only to {allowed}. Nothing was changed")
            }
            RoundsExhausted { id, max_rounds } => write!(
                f,
                "goal `{id}` has used all {max_rounds} rounds; nothing was changed. Raise max_rounds (update_goal action=edit) before resuming"
            ),
            AnotherActive { active, title } => write!(
                f,
                "goal `{active}` (\"{title}\") is the active goal and only one goal can be active; nothing was changed. Pause or \
                 complete `{active}` first, or update it instead"
            ),
            NotYours { id, owner, op } => write!(
                f,
                "goal `{id}` belongs to {owner}, and an agent may {op} only goals it created{}; nothing was changed. Ask the operator",
                if matches!(*op, "complete" | "block") { " or is driving" } else { "" }
            ),
            StaleRevision { id, have, sent } => {
                write!(f, "goal `{id}` is at revision {have}, not {sent}; nothing was changed. Call get_goal and use the current revision")
            }
            BadRound { id, expected, got } => write!(f, "goal `{id}`: round {got} is not the next admitted round ({expected})"),
            Tampered { id, op } => write!(
                f,
                "Goal/Changed {op} for `{id}` does not match the transition from the previous state (phase, revision, counters or \
                 timestamps)"
            ),
            BadEvent(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for GoalError {}

// ───────────────────────────────── transitions ─────────────────────────────────

fn derive_title(objective: &str) -> String {
    let first = objective.lines().next().unwrap_or("").trim();
    if first.chars().count() <= TITLE_CHARS {
        return first.to_string();
    }
    let cut: String = first.chars().take(TITLE_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Who may do what: users anything; the system pause/block; an agent complete/block what it
/// drives, and edit/pause/resume/clear only what it created.
fn authorize(g: &Goal, op: &Op, by: &Actor) -> Result<(), GoalError> {
    let ok = match by {
        Actor::Operator | Actor::Engineer => true,
        Actor::System(_) => matches!(op, Op::Pause | Op::Block { .. }),
        Actor::Agent(a) => {
            let owns = g.owner == Actor::Agent(*a);
            let drives = g.driver == Some(*a);
            match op {
                Op::Complete | Op::Block { .. } => owns || drives,
                _ => owns,
            }
        }
    };
    if ok { Ok(()) } else { Err(GoalError::NotYours { id: g.id.clone(), owner: g.owner.clone(), op: op.tag() }) }
}

/// The pure transition table for one goal. `at` is the op's time (clamped so `updated_at` never
/// goes backwards); `new_id` names a created goal.
///
/// pause: Active→Paused · block: Active→Blocked · resume: {Active,Paused,Blocked}→Active while
/// rounds < max · complete: any non-complete→Complete · edit never changes phase · clear leaves a
/// tombstone.
pub fn apply(goal: Option<&Goal>, op: &Op, by: &Actor, at: Timestamp, new_id: &str) -> Result<Goal, GoalError> {
    let g = match (goal, op) {
        (None, Op::Create { title, objective, max_rounds, paused }) => {
            let objective = objective.trim();
            if objective.is_empty() {
                return Err(GoalError::EmptyObjective);
            }
            if *max_rounds == 0 {
                return Err(GoalError::BadRounds { max_rounds: 0, rounds_started: 0 });
            }
            let title = if title.trim().is_empty() { derive_title(objective) } else { title.trim().to_string() };
            return Ok(Goal {
                id: new_id.to_string(),
                revision: 1,
                phase: if *paused { Phase::Paused } else { Phase::Active },
                title,
                objective: objective.to_string(),
                blocked_reason: None,
                created_at: at,
                updated_at: at,
                rounds_started: 0,
                max_rounds: *max_rounds,
                owner: by.clone(),
                driver: by.agent(),
                cleared: false,
            });
        }
        (Some(g), Op::Create { .. }) => return Err(GoalError::GoalExists(g.id.clone())),
        (None, _) => return Err(GoalError::NoGoal),
        (Some(g), _) => g,
    };
    if g.cleared {
        return Err(GoalError::Cleared(g.id.clone()));
    }
    authorize(g, op, by)?;
    let illegal = || GoalError::Illegal { id: g.id.clone(), op: op.tag(), phase: g.phase };
    let mut n = g.clone();
    n.revision += 1;
    n.updated_at = std::cmp::max(at, g.updated_at);
    match op {
        Op::Create { .. } => return Err(GoalError::GoalExists(g.id.clone())),
        Op::Edit { title, objective, max_rounds } => {
            if g.phase == Phase::Complete {
                return Err(illegal());
            }
            let mut changed = false;
            if let Some(t) = title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                changed |= t != n.title;
                n.title = t.to_string();
            }
            if let Some(o) = objective.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                changed |= o != n.objective;
                n.objective = o.to_string();
            }
            if let Some(m) = *max_rounds {
                if m == 0 || m < g.rounds_started {
                    return Err(GoalError::BadRounds { max_rounds: m, rounds_started: g.rounds_started });
                }
                changed |= m != n.max_rounds;
                n.max_rounds = m;
            }
            if !changed {
                return Err(GoalError::NothingToEdit(g.id.clone()));
            }
        }
        Op::Pause => {
            if g.phase != Phase::Active {
                return Err(illegal());
            }
            n.phase = Phase::Paused;
        }
        Op::Resume => {
            if g.phase == Phase::Complete {
                return Err(illegal());
            }
            if g.rounds_started >= g.max_rounds {
                return Err(GoalError::RoundsExhausted { id: g.id.clone(), max_rounds: g.max_rounds });
            }
            n.phase = Phase::Active;
            n.blocked_reason = None;
        }
        Op::Complete => {
            if g.phase == Phase::Complete {
                return Err(illegal());
            }
            n.phase = Phase::Complete;
            n.blocked_reason = None;
        }
        Op::Block { reason } => {
            if g.phase != Phase::Active {
                return Err(illegal());
            }
            if reason.message.trim().is_empty() || reason.code.trim().is_empty() {
                return Err(GoalError::EmptyReason);
            }
            n.phase = Phase::Blocked;
            n.blocked_reason = Some(BlockReason { code: reason.code.trim().to_string(), message: reason.message.trim().to_string() });
        }
        Op::Clear => n.cleared = true,
    }
    Ok(n)
}

// ───────────────────────────────── events ─────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
#[allow(clippy::large_enum_variant)] // a handful per session; the full snapshot is the contract
pub enum Event {
    /// The full snapshot after `op`; the fold recomputes and compares it.
    Changed { op: Op, goal: Goal, by: Actor },
    /// A continuation round was queued for `driver` with `source: Goal{id, revision, round}`.
    Round {
        id: String,
        revision: u32,
        round: u32,
        #[serde(with = "agent_str")]
        driver: AgentId,
    },
}

impl Event {
    pub const NAMES: &'static [&'static str] = &["Changed", "Round"];

    pub fn name(&self) -> &'static str {
        match self {
            Event::Changed { .. } => "Changed",
            Event::Round { .. } => "Round",
        }
    }
    pub fn body(&self) -> Result<Value, GoalError> {
        let mut v = serde_json::to_value(self).map_err(|e| GoalError::BadEvent(format!("Goal/{} did not serialize: {e}", self.name())))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Object(Default::default())))
    }
    pub fn parse(name: &str, body: &Value) -> Result<Event, GoalError> {
        if !Event::NAMES.contains(&name) {
            return Err(GoalError::BadEvent(format!("unknown goal event `Goal/{name}`")));
        }
        serde_json::from_value(serde_json::json!({ "name": name, "body": body }))
            .map_err(|e| GoalError::BadEvent(format!("malformed Goal/{name}: {e}")))
    }
    /// `{ "ns": "Goal", "name": .., "body": .. }` — the shape tools return under `value.events`.
    pub fn to_json(&self) -> Result<Value, GoalError> {
        Ok(serde_json::json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }
    pub fn to_entry(&self) -> Result<EntryKind, GoalError> {
        Ok(EntryKind::Custom { ns: NS.to_string(), name: self.name().to_string(), body: self.body()? })
    }
}

/// What the round driver decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundPlan {
    /// No goal is active.
    Idle,
    /// A goal is active but automatic continuation is disarmed (e.g. after resume).
    Disarmed { id: String },
    /// Queue round `round` with `source: Goal{id, revision, round}`; append `events` with it.
    Start { id: String, revision: u32, round: u32, prompt: String, events: Vec<Event> },
    /// The round budget is spent: the goal blocks with `round_limit`.
    Blocked { id: String, events: Vec<Event> },
}

// ───────────────────────────────── projection ─────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goals {
    /// Creation order; cleared goals stay as tombstones.
    #[serde(default)]
    goals: Vec<Goal>,
    #[serde(default)]
    created: u64,
}

impl Goals {
    pub fn all(&self) -> &[Goal] {
        &self.goals
    }
    pub fn get(&self, id: &str) -> Option<&Goal> {
        self.goals.iter().find(|g| g.id == id)
    }
    /// The earliest-created Active goal (at most one exists).
    pub fn current(&self) -> Option<&Goal> {
        self.goals.iter().find(|g| g.phase == Phase::Active && !g.cleared)
    }
    /// The goal updates should target: the Active one, else the most recently updated open goal.
    pub fn focus(&self) -> Option<&Goal> {
        self.current()
            .or_else(|| self.goals.iter().filter(|g| !g.cleared && g.phase != Phase::Complete).max_by_key(|g| (g.updated_at, g.revision)))
    }
    pub fn next_id(&self) -> String {
        let mut n = self.created + 1;
        loop {
            let id = format!("goal-{n}");
            if self.get(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }

    /// Events for `op` on `target` (ignored for create). A user activating a goal pauses an Active
    /// agent goal first (user priority); otherwise a second Active goal is refused.
    pub fn plan(&self, target: Option<&str>, op: Op, by: &Actor, at: Timestamp) -> Result<Vec<Event>, GoalError> {
        let (prev, id) = match &op {
            Op::Create { .. } => (None, self.next_id()),
            other => {
                let id = target.ok_or(GoalError::NeedsId(other.tag()))?;
                (Some(self.get(id).ok_or_else(|| GoalError::UnknownGoal(id.to_string()))?), id.to_string())
            }
        };
        let goal = apply(prev, &op, by, at, &id)?;
        let mut events = Vec::new();
        if goal.phase == Phase::Active
            && !goal.cleared
            && let Some(other) = self.current().filter(|c| c.id != goal.id)
        {
            if !(by.is_user() && !other.owner.is_user()) {
                return Err(GoalError::AnotherActive { active: other.id.clone(), title: other.title.clone() });
            }
            let system = Actor::System("goals".into());
            let paused = apply(Some(other), &Op::Pause, &system, at, &other.id)?;
            events.push(Event::Changed { op: Op::Pause, goal: paused, by: system });
        }
        events.push(Event::Changed { op, goal, by: by.clone() });
        self.stage(&events)?;
        Ok(events)
    }

    pub fn stage(&self, events: &[Event]) -> Result<Goals, GoalError> {
        let mut g = self.clone();
        for e in events {
            g.apply(e)?;
        }
        Ok(g)
    }

    /// One fold step: recompute the transition and require the logged snapshot to match it.
    pub fn apply(&mut self, e: &Event) -> Result<(), GoalError> {
        match e {
            Event::Changed { op, goal, by } => {
                let expected = match op {
                    Op::Create { .. } => {
                        if self.get(&goal.id).is_some() {
                            return Err(GoalError::DuplicateId(goal.id.clone()));
                        }
                        apply(None, op, by, goal.created_at, &goal.id)?
                    }
                    _ => {
                        let prev = self.get(&goal.id).ok_or_else(|| GoalError::UnknownGoal(goal.id.clone()))?;
                        apply(Some(prev), op, by, goal.updated_at, &goal.id)?
                    }
                };
                if &expected != goal {
                    return Err(GoalError::Tampered { id: goal.id.clone(), op: op.tag() });
                }
                if goal.phase == Phase::Active
                    && !goal.cleared
                    && let Some(other) = self.current().filter(|c| c.id != goal.id)
                {
                    return Err(GoalError::AnotherActive { active: other.id.clone(), title: other.title.clone() });
                }
                match self.goals.iter_mut().find(|g| g.id == goal.id) {
                    Some(slot) => *slot = goal.clone(),
                    None => {
                        self.goals.push(goal.clone());
                        self.created += 1;
                    }
                }
            }
            Event::Round { id, revision, round, driver } => {
                let g = self.goals.iter_mut().find(|g| &g.id == id).ok_or_else(|| GoalError::UnknownGoal(id.clone()))?;
                if g.cleared {
                    return Err(GoalError::Cleared(id.clone()));
                }
                if g.phase != Phase::Active {
                    return Err(GoalError::Illegal { id: id.clone(), op: "round", phase: g.phase });
                }
                if *revision != g.revision {
                    return Err(GoalError::StaleRevision { id: id.clone(), have: g.revision, sent: *revision });
                }
                if *round != g.rounds_started + 1 || *round > g.max_rounds {
                    return Err(GoalError::BadRound { id: id.clone(), expected: g.rounds_started + 1, got: *round });
                }
                g.rounds_started = *round;
                g.driver = Some(*driver);
            }
        }
        Ok(())
    }

    /// Round driver: while the current goal is active and armed, queue the next round; at
    /// `max_rounds` block it with `round_limit`.
    pub fn next_round(&self, driver: AgentId, armed: bool, at: Timestamp) -> Result<RoundPlan, GoalError> {
        let Some(g) = self.current() else { return Ok(RoundPlan::Idle) };
        if !armed {
            return Ok(RoundPlan::Disarmed { id: g.id.clone() });
        }
        if g.rounds_started < g.max_rounds {
            let round = g.rounds_started + 1;
            let events = vec![Event::Round { id: g.id.clone(), revision: g.revision, round, driver }];
            self.stage(&events)?;
            let prompt = format!(
                "Goal round {round}/{max} for {id} (revision {rev}).\nObjective: {obj}\nKeep working toward the objective. When it is \
                 actually achieved, call update_goal with id {id} and action complete. If the same concrete condition blocks you \
                 across rounds, call update_goal with action block and name that condition in reason.",
                max = g.max_rounds,
                id = g.id,
                rev = g.revision,
                obj = g.objective
            );
            return Ok(RoundPlan::Start { id: g.id.clone(), revision: g.revision, round, prompt, events });
        }
        let system = Actor::System("goal-rounds".into());
        let op = Op::Block {
            reason: BlockReason {
                code: ROUND_LIMIT.into(),
                message: format!("used all {} goal rounds without completing; raise max_rounds and resume to continue", g.max_rounds),
            },
        };
        let events = self.plan(Some(&g.id), op, &system, at)?;
        Ok(RoundPlan::Blocked { id: g.id.clone(), events })
    }
}

/// The `CustomFold` for `Goal/*`: decode the previous projection, validate and apply one event.
pub fn fold_goals(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut goals = project(prev)?;
    let e = Event::parse(name, body).map_err(|e| e.to_string())?;
    goals.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&goals).map_err(|e| format!("the goal projection did not serialize: {e}"))
}

/// Decodes `State.custom["Goal"]` (absent = no goals).
pub fn project(folded: Option<&Value>) -> Result<Goals, String> {
    match folded {
        None => Ok(Goals::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the goal projection is unreadable: {e}")),
    }
}

#[cfg(test)]
mod tests;
