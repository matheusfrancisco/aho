//! `workflow/*` entries and their fold (the ralph loop state machine).
//!
//! * `Started{workflow}`: the whole loop at iteration 0; one `running` loop at a time.
//! * `Iteration{id, n, promise_found, at}`: by the driver after loop-fed turn `n` ended;
//!   `n` is exactly the next one and never past `max_iterations`.
//! * `Finished{id, outcome, by, at, reason}`: `completed` only after an iteration found the
//!   promise, `exhausted` only when the budget is spent without it, `failed` when a fed turn
//!   aborted (driver), `stopped` by `ralph_stop` or the operator. Agents stop only agent loops.

use std::fmt;

use aho_control::TurnEnd;
use aho_goals::Actor;
use aho_log::EntryKind;
use aho_types::{Timestamp, Value};
use serde::{Deserialize, Serialize};

pub const NS: &str = "workflow";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    Completed,
    Exhausted,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Exhausted,
    Stopped,
    Failed,
}

impl Outcome {
    fn phase(self) -> Phase {
        match self {
            Outcome::Completed => Phase::Completed,
            Outcome::Exhausted => Phase::Exhausted,
            Outcome::Stopped => Phase::Stopped,
            Outcome::Failed => Phase::Failed,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Phase::Running => "running",
            Phase::Completed => "completed",
            Phase::Exhausted => "exhausted",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub prompt: String,
    pub promise: String,
    pub max_iterations: u32,
    /// Loop-fed turns that have ended.
    pub iterations: u32,
    /// Whether the last iteration's final text carried the promise.
    pub promise_found: bool,
    pub phase: Phase,
    pub started_by: Actor,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub ended_by: Option<Actor>,
    pub reason: Option<String>,
}

impl Workflow {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} is {} after {} of {} iterations; promise <promise>{}</promise>; started by {}.",
            self.id, self.phase, self.iterations, self.max_iterations, self.promise, self.started_by
        );
        if let Some(r) = &self.reason {
            s.push_str(&format!(" Reason: {r}."));
        }
        s.push_str(&format!("\nPrompt: {}", self.prompt));
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
pub enum Event {
    Started {
        workflow: Workflow,
    },
    Iteration {
        id: String,
        n: u32,
        promise_found: bool,
        at: Timestamp,
    },
    Finished {
        id: String,
        outcome: Outcome,
        by: Actor,
        at: Timestamp,
        #[serde(default)]
        reason: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowError {
    Empty(&'static str),
    TooMany { asked: u64, max: u32 },
    AnotherRunning { id: String },
    NoneRunning,
    Unknown(String),
    NotRunning { id: String, phase: Phase },
    NotYours { id: String, owner: Actor },
    BadEvent(String),
    Illegal { id: String, what: String },
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowError::Empty(what) => write!(f, "`{what}` is empty, so no loop was started; give the task and the completion phrase"),
            WorkflowError::TooMany { asked, max } => write!(
                f,
                "{asked} iterations is more than the {max} a loop may run here, so no loop was started; ask for fewer (the operator sets `workflows.max_iterations`)"
            ),
            WorkflowError::AnotherRunning { id } => {
                write!(f, "{id} is still running, so no second loop was started; finish it or stop it with ralph_stop first")
            }
            WorkflowError::NoneRunning => f.write_str("no ralph loop is running; nothing changed"),
            WorkflowError::Unknown(id) => write!(f, "there is no ralph loop `{id}`; ralph_status shows the latest"),
            WorkflowError::NotRunning { id, phase } => write!(f, "{id} is already {phase}; nothing changed"),
            WorkflowError::NotYours { id, owner } => {
                write!(f, "{id} was started by {owner}; only they (or the operator) can stop it, so it keeps running")
            }
            WorkflowError::BadEvent(e) => f.write_str(e),
            WorkflowError::Illegal { id, what } => write!(f, "{id}: {what}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl Event {
    pub const NAMES: &'static [&'static str] = &["Started", "Iteration", "Finished"];

    pub fn name(&self) -> &'static str {
        match self {
            Event::Started { .. } => "Started",
            Event::Iteration { .. } => "Iteration",
            Event::Finished { .. } => "Finished",
        }
    }
    pub fn body(&self) -> Result<Value, WorkflowError> {
        let mut v =
            serde_json::to_value(self).map_err(|e| WorkflowError::BadEvent(format!("workflow/{} did not serialize: {e}", self.name())))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Object(Default::default())))
    }
    pub fn parse(name: &str, body: &Value) -> Result<Event, WorkflowError> {
        if !Event::NAMES.contains(&name) {
            return Err(WorkflowError::BadEvent(format!("unknown workflow event `workflow/{name}`")));
        }
        serde_json::from_value(serde_json::json!({ "name": name, "body": body }))
            .map_err(|e| WorkflowError::BadEvent(format!("malformed workflow/{name}: {e}")))
    }
    pub fn to_json(&self) -> Result<Value, WorkflowError> {
        Ok(serde_json::json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }
    pub fn to_entry(&self) -> Result<EntryKind, WorkflowError> {
        Ok(EntryKind::Custom { ns: NS.to_string(), name: self.name().to_string(), body: self.body()? })
    }
}

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `text` contains `<promise>…</promise>` whose content equals `promise` (whitespace
/// runs collapsed, ends trimmed; case-sensitive).
pub fn promise_in(text: &str, promise: &str) -> bool {
    let want = normalized(promise);
    if want.is_empty() {
        return false;
    }
    let mut rest = text;
    while let Some(open) = rest.find("<promise>") {
        let after = &rest[open + "<promise>".len()..];
        let Some(close) = after.find("</promise>") else { return false };
        if normalized(&after[..close]) == want {
            return true;
        }
        rest = &after[close + "</promise>".len()..];
    }
    false
}

/// The loop driver's next action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopPlan {
    /// No loop is running.
    Idle,
    /// Feed iteration `n`: prompt `text`, inbox key `workflow:<id>:<n>`.
    Feed { id: String, n: u32, key: String, text: String },
    /// Close the running loop with `event`.
    Finish { event: Event },
}

/// The fed prompt: the task verbatim, then the loop's own line.
pub fn feed_text(w: &Workflow, n: u32) -> String {
    format!(
        "{prompt}\n\n[Ralph loop {id}: iteration {n} of {max}. This same prompt comes back after every turn until you finish. Once the \
         task is complete and verified, end your reply with <promise>{promise}</promise>, and only then: writing it early does \
         not make the work done. ralph_stop ends the loop without finishing.]",
        prompt = w.prompt,
        id = w.id,
        max = w.max_iterations,
        promise = w.promise
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflows {
    #[serde(default)]
    list: Vec<Workflow>,
    #[serde(default)]
    started: u64,
}

const DRIVER_ACTOR: &str = "workflow";

impl Workflows {
    pub fn all(&self) -> &[Workflow] {
        &self.list
    }
    pub fn get(&self, id: &str) -> Option<&Workflow> {
        self.list.iter().find(|w| w.id == id)
    }
    /// The running loop (at most one).
    pub fn current(&self) -> Option<&Workflow> {
        self.list.iter().find(|w| w.phase == Phase::Running)
    }
    pub fn next_id(&self) -> String {
        let mut n = self.started + 1;
        loop {
            let id = format!("ralph-{n}");
            if self.get(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }

    pub fn start(&self, prompt: &str, promise: &str, max_iterations: u32, by: &Actor, at: Timestamp) -> Result<Event, WorkflowError> {
        let workflow = Workflow {
            id: self.next_id(),
            prompt: prompt.trim().to_string(),
            promise: normalized(promise),
            max_iterations,
            iterations: 0,
            promise_found: false,
            phase: Phase::Running,
            started_by: by.clone(),
            started_at: at,
            updated_at: at,
            ended_by: None,
            reason: None,
        };
        let e = Event::Started { workflow };
        self.stage(std::slice::from_ref(&e))?;
        Ok(e)
    }

    /// `ralph_stop` / operator stop of `id` (default: the running loop).
    pub fn stop(&self, id: Option<&str>, by: &Actor, at: Timestamp, reason: Option<String>) -> Result<Event, WorkflowError> {
        let id = match id {
            Some(id) => id.to_string(),
            None => self.current().map(|w| w.id.clone()).ok_or(WorkflowError::NoneRunning)?,
        };
        let e = Event::Finished { id, outcome: Outcome::Stopped, by: by.clone(), at, reason };
        self.stage(std::slice::from_ref(&e))?;
        Ok(e)
    }

    pub fn stage(&self, events: &[Event]) -> Result<Workflows, WorkflowError> {
        let mut w = self.clone();
        for e in events {
            w.apply(e)?;
        }
        Ok(w)
    }

    /// One fold step; illegal transitions fail replay.
    pub fn apply(&mut self, e: &Event) -> Result<(), WorkflowError> {
        match e {
            Event::Started { workflow: w } => {
                if self.get(&w.id).is_some() {
                    return Err(WorkflowError::BadEvent(format!("loop id {} is used twice", w.id)));
                }
                if let Some(other) = self.current() {
                    return Err(WorkflowError::AnotherRunning { id: other.id.clone() });
                }
                if w.prompt.trim().is_empty() {
                    return Err(WorkflowError::Empty("prompt"));
                }
                if normalized(&w.promise).is_empty() {
                    return Err(WorkflowError::Empty("completion_promise"));
                }
                let fresh = w.phase == Phase::Running
                    && w.iterations == 0
                    && !w.promise_found
                    && w.max_iterations >= 1
                    && w.updated_at == w.started_at
                    && w.ended_by.is_none()
                    && w.reason.is_none();
                if !fresh {
                    return Err(WorkflowError::Illegal { id: w.id.clone(), what: "a loop starts running at iteration 0".into() });
                }
                self.list.push(w.clone());
                self.started += 1;
            }
            Event::Iteration { id, n, promise_found, at } => {
                let w = self.running_mut(id)?;
                if *n != w.iterations + 1 || *n > w.max_iterations {
                    return Err(WorkflowError::Illegal {
                        id: id.clone(),
                        what: format!("iteration {n} does not follow {} of {}", w.iterations, w.max_iterations),
                    });
                }
                w.iterations = *n;
                w.promise_found = *promise_found;
                w.updated_at = w.updated_at.max(*at);
            }
            Event::Finished { id, outcome, by, at, reason } => {
                let w = self.running_mut(id)?;
                let driver = *by == Actor::System(DRIVER_ACTOR.into());
                let legal = match outcome {
                    Outcome::Completed => driver && w.promise_found,
                    Outcome::Exhausted => driver && !w.promise_found && w.iterations == w.max_iterations,
                    Outcome::Failed => driver && reason.as_deref().is_some_and(|r| !r.trim().is_empty()),
                    Outcome::Stopped => by.agent().is_none() || w.started_by.agent().is_some(),
                };
                if !legal {
                    if *outcome == Outcome::Stopped {
                        return Err(WorkflowError::NotYours { id: id.clone(), owner: w.started_by.clone() });
                    }
                    return Err(WorkflowError::Illegal { id: id.clone(), what: format!("it cannot end as {:?} here", outcome) });
                }
                w.phase = outcome.phase();
                w.ended_by = Some(by.clone());
                w.reason = reason.clone();
                w.updated_at = w.updated_at.max(*at);
            }
        }
        Ok(())
    }

    fn running_mut(&mut self, id: &str) -> Result<&mut Workflow, WorkflowError> {
        let w = self.list.iter_mut().find(|w| w.id == id).ok_or_else(|| WorkflowError::Unknown(id.to_string()))?;
        if w.phase != Phase::Running {
            return Err(WorkflowError::NotRunning { id: id.to_string(), phase: w.phase });
        }
        Ok(w)
    }

    /// The round planner: after a turn ended (any turn), what the running loop needs next.
    pub fn next(&self, at: Timestamp) -> Result<LoopPlan, WorkflowError> {
        let Some(w) = self.current() else { return Ok(LoopPlan::Idle) };
        let by = Actor::System(DRIVER_ACTOR.into());
        let finish = |outcome| Event::Finished { id: w.id.clone(), outcome, by: by.clone(), at, reason: None };
        let plan = if w.promise_found {
            LoopPlan::Finish { event: finish(Outcome::Completed) }
        } else if w.iterations >= w.max_iterations {
            LoopPlan::Finish { event: finish(Outcome::Exhausted) }
        } else {
            let n = w.iterations + 1;
            LoopPlan::Feed { id: w.id.clone(), n, key: format!("workflow:{}:{n}", w.id), text: feed_text(w, n) }
        };
        if let LoopPlan::Finish { event } = &plan {
            self.stage(std::slice::from_ref(event))?;
        }
        Ok(plan)
    }

    /// Records loop-fed turn `n` of `id`: whether its final text carried the promise. A turn that
    /// aborted without it fails the loop instead of burning its budget on a broken model.
    pub fn after_turn(&self, id: &str, n: u32, end: &TurnEnd, at: Timestamp) -> Result<Vec<Event>, WorkflowError> {
        let w = self.get(id).ok_or_else(|| WorkflowError::Unknown(id.to_string()))?;
        let found = promise_in(&end.final_text, &w.promise);
        let mut events = vec![Event::Iteration { id: id.to_string(), n, promise_found: found, at }];
        if let (Some(why), false) = (&end.failed, found) {
            events.push(Event::Finished {
                id: id.to_string(),
                outcome: Outcome::Failed,
                by: Actor::System(DRIVER_ACTOR.into()),
                at,
                reason: Some(format!("iteration {n} ended with an error: {why}")),
            });
        }
        self.stage(&events)?;
        Ok(events)
    }
}

/// The `CustomFold` for `workflow/*`.
pub fn fold_workflows(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut w = project(prev)?;
    let e = Event::parse(name, body).map_err(|e| e.to_string())?;
    w.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&w).map_err(|e| format!("the workflow projection did not serialize: {e}"))
}

/// Decodes `State.custom["workflow"]` (absent = no loops).
pub fn project(folded: Option<&Value>) -> Result<Workflows, String> {
    match folded {
        None => Ok(Workflows::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the workflow projection is unreadable: {e}")),
    }
}

/// Folds only the `workflow/*` entries of one branch.
pub fn load(entries: &[aho_log::Entry]) -> Result<Workflows, String> {
    let mut w = Workflows::default();
    for e in entries {
        if let EntryKind::Custom { ns, name, body } = &e.kind
            && ns == NS
        {
            let ev = Event::parse(name, body).map_err(|x| format!("entry {}: {x}", e.id.0))?;
            w.apply(&ev).map_err(|x| format!("entry {}: {x}", e.id.0))?;
        }
    }
    Ok(w)
}
