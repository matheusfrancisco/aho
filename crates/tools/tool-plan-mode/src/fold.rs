//! `plan/*` entries and their fold.
//!
//! * `Entered{by, at, reason, restore, allowed}`: `restore` is the policy before plan mode,
//!   `allowed` the narrowed one: `allowed ⊆ restore ∩ {ReadOnly, TalksToHuman}`.
//! * `Reviewed{at, plan, outcome, allowed}`: the operator's answer to one `exit_plan_mode`.
//!   Only `allowed_once` leaves plan mode, and only if the policy after it contains `restore`
//!   again; every other outcome keeps plan mode on.

use aho_goals::Actor;
use aho_log::{ApprovalOutcome, EntryKind};
use aho_types::{ConsequenceSet, Timestamp, Value};
use serde::{Deserialize, Serialize};

use crate::PLAN_CUT;

pub const NS: &str = "plan";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMode {
    pub active: bool,
    pub entered_by: Option<Actor>,
    pub entered_at: Option<Timestamp>,
    pub reason: Option<String>,
    /// The policy from before plan mode: what an approved plan restores.
    pub restore: ConsequenceSet,
    /// The policy while in plan mode (or after the last review).
    pub allowed: ConsequenceSet,
    pub times_entered: u32,
    /// Reviews in the current (or last) plan mode.
    pub reviews: u32,
    pub last_plan: Option<String>,
    pub last_outcome: Option<ApprovalOutcome>,
    pub left_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
pub enum Event {
    Entered { by: Actor, at: Timestamp, reason: Option<String>, restore: ConsequenceSet, allowed: ConsequenceSet },
    Reviewed { at: Timestamp, plan: String, outcome: ApprovalOutcome, allowed: ConsequenceSet },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    AlreadyOn,
    NotOn,
    EmptyPlan,
    NotNarrowed { restore: ConsequenceSet, allowed: ConsequenceSet },
    NotRestored { restore: ConsequenceSet, allowed: ConsequenceSet },
    BadEvent(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::AlreadyOn => f.write_str("plan mode is already on"),
            PlanError::NotOn => f.write_str("plan mode is not on"),
            PlanError::EmptyPlan => f.write_str("the plan is empty"),
            PlanError::NotNarrowed { restore, allowed } => {
                write!(f, "plan mode would allow {allowed}, which is not a read-only narrowing of {restore}")
            }
            PlanError::NotRestored { restore, allowed } => {
                write!(f, "an approved plan must restore {restore}, but the policy allows only {allowed}")
            }
            PlanError::BadEvent(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for PlanError {}

impl Event {
    pub const NAMES: &'static [&'static str] = &["Entered", "Reviewed"];

    pub fn name(&self) -> &'static str {
        match self {
            Event::Entered { .. } => "Entered",
            Event::Reviewed { .. } => "Reviewed",
        }
    }
    pub fn body(&self) -> Result<Value, PlanError> {
        let mut v = serde_json::to_value(self).map_err(|e| PlanError::BadEvent(format!("plan/{} did not serialize: {e}", self.name())))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Object(Default::default())))
    }
    pub fn parse(name: &str, body: &Value) -> Result<Event, PlanError> {
        if !Event::NAMES.contains(&name) {
            return Err(PlanError::BadEvent(format!("unknown plan event `plan/{name}`")));
        }
        serde_json::from_value(serde_json::json!({ "name": name, "body": body }))
            .map_err(|e| PlanError::BadEvent(format!("malformed plan/{name}: {e}")))
    }
    pub fn to_json(&self) -> Result<Value, PlanError> {
        Ok(serde_json::json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }
    pub fn to_entry(&self) -> Result<EntryKind, PlanError> {
        Ok(EntryKind::Custom { ns: NS.to_string(), name: self.name().to_string(), body: self.body()? })
    }
}

impl PlanMode {
    /// The way plan mode was last left, for the context line.
    pub fn left_how(&self) -> String {
        match (self.last_outcome, self.left_at) {
            (Some(ApprovalOutcome::AllowedOnce), Some(_)) => "the operator approved the plan".into(),
            _ => "it was left".into(),
        }
    }

    pub fn enter(
        &self,
        by: &Actor,
        at: Timestamp,
        reason: Option<String>,
        restore: ConsequenceSet,
        allowed: ConsequenceSet,
    ) -> Result<Event, PlanError> {
        let e = Event::Entered { by: by.clone(), at, reason, restore, allowed };
        self.stage(&e)?;
        Ok(e)
    }

    pub fn review(&self, at: Timestamp, plan: &str, outcome: ApprovalOutcome, allowed: ConsequenceSet) -> Result<Event, PlanError> {
        let e = Event::Reviewed { at, plan: plan.to_string(), outcome, allowed };
        self.stage(&e)?;
        Ok(e)
    }

    pub fn stage(&self, e: &Event) -> Result<PlanMode, PlanError> {
        let mut m = self.clone();
        m.apply(e)?;
        Ok(m)
    }

    pub fn apply(&mut self, e: &Event) -> Result<(), PlanError> {
        match e {
            Event::Entered { by, at, reason, restore, allowed } => {
                if self.active {
                    return Err(PlanError::AlreadyOn);
                }
                if !allowed.is_subset(&restore.intersect(PLAN_CUT)) {
                    return Err(PlanError::NotNarrowed { restore: *restore, allowed: *allowed });
                }
                *self = PlanMode {
                    active: true,
                    entered_by: Some(by.clone()),
                    entered_at: Some(*at),
                    reason: reason.clone(),
                    restore: *restore,
                    allowed: *allowed,
                    times_entered: self.times_entered + 1,
                    reviews: 0,
                    last_plan: None,
                    last_outcome: None,
                    left_at: None,
                };
            }
            Event::Reviewed { at, plan, outcome, allowed } => {
                if !self.active {
                    return Err(PlanError::NotOn);
                }
                if plan.trim().is_empty() {
                    return Err(PlanError::EmptyPlan);
                }
                if *outcome == ApprovalOutcome::AllowedOnce {
                    if !self.restore.is_subset(allowed) {
                        return Err(PlanError::NotRestored { restore: self.restore, allowed: *allowed });
                    }
                    self.active = false;
                    self.left_at = Some(*at);
                }
                self.allowed = *allowed;
                self.reviews += 1;
                self.last_plan = Some(plan.clone());
                self.last_outcome = Some(*outcome);
            }
        }
        Ok(())
    }
}

/// The `CustomFold` for `plan/*`.
pub fn fold_plan(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut m = project(prev)?;
    let e = Event::parse(name, body).map_err(|e| e.to_string())?;
    m.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&m).map_err(|e| format!("the plan-mode projection did not serialize: {e}"))
}

/// Decodes `State.custom["plan"]` (absent = never entered).
pub fn project(folded: Option<&Value>) -> Result<PlanMode, String> {
    match folded {
        None => Ok(PlanMode::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the plan-mode projection is unreadable: {e}")),
    }
}

/// Folds only the `plan/*` entries of one branch.
pub fn load(entries: &[aho_log::Entry]) -> Result<PlanMode, String> {
    let mut m = PlanMode::default();
    for e in entries {
        if let EntryKind::Custom { ns, name, body } = &e.kind
            && ns == NS
        {
            let ev = Event::parse(name, body).map_err(|x| format!("entry {}: {x}", e.id.0))?;
            m.apply(&ev).map_err(|x| format!("entry {}: {x}", e.id.0))?;
        }
    }
    Ok(m)
}
