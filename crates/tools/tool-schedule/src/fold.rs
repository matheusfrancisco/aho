//! `schedule/*` entries and their fold. Every entry carries what the fold needs to recompute it
//! (a custom fold never sees the entry's time or principal): `Created` the whole schedule,
//! `Fired` the occurrence it covers and the instant it was decided. The fold recomputes
//! `next_due` and the skipped count from the spec; a mismatch fails replay loudly.
//!
//! * `Created{schedule}`: by a tool (`value.events`) or an operator command.
//! * `Fired{id, n, due, at, skipped}`: by the driver, appended *after* the fired turn ended; the
//!   turn's prompt is keyed `schedule:<id>:<n>`, so a restart between the two re-sends a key the
//!   session already admitted and nothing runs twice.
//! * `Cancelled{id, by, at}`: agents cancel what agents created; the operator anything.

use std::fmt;

use aho_goals::Actor;
use aho_log::EntryKind;
use aho_types::{Timestamp, Value};
use serde::{Deserialize, Serialize};

use crate::spec::{Spec, SpecError, format_utc};

pub const NS: &str = "schedule";
/// A fire counts at most this many missed occurrences (bounds the fold's recount).
pub const SKIP_CAP: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    /// A one-shot fired, or a spec with no further occurrence.
    Done,
    Cancelled,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Active => "active",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub label: String,
    pub prompt: String,
    pub spec: Spec,
    pub created_by: Actor,
    pub created_at: Timestamp,
    /// The next occurrence; `None` once done or cancelled.
    pub next_due: Option<Timestamp>,
    pub fires: u64,
    /// Occurrences folded into later fires (missed while the session was down or busy).
    pub skipped: u64,
    pub last_fired_at: Option<Timestamp>,
    pub status: Status,
    pub cancelled_by: Option<Actor>,
}

impl Schedule {
    pub fn summary(&self) -> String {
        let label = if self.label.is_empty() { String::new() } else { format!(" \"{}\"", self.label) };
        let when = match (self.status, self.next_due) {
            (Status::Active, Some(t)) => format!("next {}", format_utc(t)),
            (s, _) => s.to_string(),
        };
        let skipped = if self.skipped > 0 { format!(", {} missed", self.skipped) } else { String::new() };
        format!("{}{label}: {}; {when}; fired {}×{skipped}; by {}", self.id, self.spec.describe(), self.fires, self.created_by)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
pub enum Event {
    Created { schedule: Schedule },
    Fired { id: String, n: u64, due: Timestamp, at: Timestamp, skipped: u64 },
    Cancelled { id: String, by: Actor, at: Timestamp },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    EmptyPrompt,
    NoWhen,
    TwoWhens,
    Spec(SpecError),
    InPast { at: Timestamp, now: Timestamp },
    NeverFires(String),
    TooFrequent { gap_ms: u64, min_ms: u64 },
    TooMany { max: usize },
    Unknown(String),
    NotYours { id: String, owner: Actor },
    NotActive { id: String, status: Status },
    BadEvent(String),
    Tampered { id: String, what: &'static str },
}

impl From<SpecError> for ScheduleError {
    fn from(e: SpecError) -> Self {
        ScheduleError::Spec(e)
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::spec::format_duration as d;
        match self {
            ScheduleError::EmptyPrompt => {
                f.write_str("the prompt is empty, so nothing was scheduled; say what the session should do when it fires")
            }
            ScheduleError::NoWhen => {
                f.write_str("no time was given, so nothing was scheduled; pass exactly one of `at`, `every` or `cron`")
            }
            ScheduleError::TwoWhens => {
                f.write_str("more than one of `at`, `every`, `cron` was given, so nothing was scheduled; pass exactly one")
            }
            ScheduleError::Spec(e) => write!(f, "{e}; nothing was scheduled"),
            ScheduleError::InPast { at, now } => write!(
                f,
                "{} has already passed (it is {} now, UTC), so nothing was scheduled; pick a later time",
                format_utc(*at),
                format_utc(*now)
            ),
            ScheduleError::NeverFires(what) => write!(f, "{what} never matches a real date, so nothing was scheduled"),
            ScheduleError::TooFrequent { gap_ms, min_ms } => write!(
                f,
                "that fires every {} but schedules may fire at most every {} here, so nothing was scheduled; use a longer interval (the operator sets `schedules.min_interval_s`)",
                d(*gap_ms),
                d(*min_ms)
            ),
            ScheduleError::TooMany { max } => write!(
                f,
                "this session already has {max} active schedules, the most it allows, so nothing was scheduled; cancel one first (the operator sets `schedules.max_active`)"
            ),
            ScheduleError::Unknown(id) => write!(f, "there is no schedule `{id}`; schedule_list shows the ids"),
            ScheduleError::NotYours { id, owner } => {
                write!(f, "{id} was created by {owner}; only they (or the operator) can cancel it, so it keeps running")
            }
            ScheduleError::NotActive { id, status } => write!(f, "{id} is already {status}; nothing changed"),
            ScheduleError::BadEvent(e) => write!(f, "{e}"),
            ScheduleError::Tampered { id, what } => write!(f, "the logged {what} for {id} does not match what its spec computes"),
        }
    }
}

impl std::error::Error for ScheduleError {}

impl Event {
    pub const NAMES: &'static [&'static str] = &["Created", "Fired", "Cancelled"];

    pub fn name(&self) -> &'static str {
        match self {
            Event::Created { .. } => "Created",
            Event::Fired { .. } => "Fired",
            Event::Cancelled { .. } => "Cancelled",
        }
    }
    pub fn body(&self) -> Result<Value, ScheduleError> {
        let mut v =
            serde_json::to_value(self).map_err(|e| ScheduleError::BadEvent(format!("schedule/{} did not serialize: {e}", self.name())))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Object(Default::default())))
    }
    pub fn parse(name: &str, body: &Value) -> Result<Event, ScheduleError> {
        if !Event::NAMES.contains(&name) {
            return Err(ScheduleError::BadEvent(format!("unknown schedule event `schedule/{name}`")));
        }
        serde_json::from_value(serde_json::json!({ "name": name, "body": body }))
            .map_err(|e| ScheduleError::BadEvent(format!("malformed schedule/{name}: {e}")))
    }
    /// `{ "ns": "schedule", "name", "body" }`: the shape tools return under `value.events`.
    pub fn to_json(&self) -> Result<Value, ScheduleError> {
        Ok(serde_json::json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }
    pub fn to_entry(&self) -> Result<EntryKind, ScheduleError> {
        Ok(EntryKind::Custom { ns: NS.to_string(), name: self.name().to_string(), body: self.body()? })
    }
}

/// Limits the tools apply at creation (config, not replay rules: the fold does not re-check them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_active: usize,
    pub min_interval_ms: u64,
}

/// One occurrence the driver should fire now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub id: String,
    pub n: u64,
    pub due: Timestamp,
    pub skipped: u64,
    /// Inbox key of the fired prompt: `schedule:<id>:<n>`.
    pub key: String,
    /// The prompt the session receives: a provenance line, then the scheduled prompt.
    pub text: String,
    /// The record to append after the turn ended.
    pub event: Event,
}

fn fresh(id: &str, prompt: &str, label: &str, spec: &Spec, by: &Actor, at: Timestamp) -> Result<Schedule, ScheduleError> {
    let next_due = spec.next_after(at)?;
    Ok(Schedule {
        id: id.to_string(),
        label: label.to_string(),
        prompt: prompt.to_string(),
        spec: spec.clone(),
        created_by: by.clone(),
        created_at: at,
        next_due,
        fires: 0,
        skipped: 0,
        last_fired_at: None,
        status: if next_due.is_some() { Status::Active } else { Status::Done },
        cancelled_by: None,
    })
}

fn skipped_between(spec: &Spec, due: Timestamp, at: Timestamp) -> Result<u64, ScheduleError> {
    Ok(spec.count_between(due, at, SKIP_CAP + 1)?.saturating_sub(1).min(SKIP_CAP))
}

/// An agent cancels what agents created; the operator, the engineer and the system anything.
fn may_cancel(by: &Actor, owner: &Actor) -> bool {
    by.agent().is_none() || owner.agent().is_some()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedules {
    #[serde(default)]
    list: Vec<Schedule>,
    #[serde(default)]
    created: u64,
}

impl Schedules {
    pub fn all(&self) -> &[Schedule] {
        &self.list
    }
    pub fn get(&self, id: &str) -> Option<&Schedule> {
        self.list.iter().find(|s| s.id == id)
    }
    pub fn active(&self) -> impl Iterator<Item = &Schedule> {
        self.list.iter().filter(|s| s.status == Status::Active)
    }
    pub fn next_id(&self) -> String {
        let mut n = self.created + 1;
        loop {
            let id = format!("sched-{n}");
            if self.get(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }

    /// The `Created` event for a new schedule; `at` is its creation time (the anchor of `every`).
    pub fn create(&self, prompt: &str, label: &str, spec: Spec, by: &Actor, at: Timestamp, limits: Limits) -> Result<Event, ScheduleError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(ScheduleError::EmptyPrompt);
        }
        if self.active().count() >= limits.max_active {
            return Err(ScheduleError::TooMany { max: limits.max_active });
        }
        let schedule = fresh(&self.next_id(), prompt, label.trim(), &spec, by, at)?;
        if schedule.next_due.is_none() {
            return Err(match spec {
                Spec::At { at: when } => ScheduleError::InPast { at: when, now: at },
                other => ScheduleError::NeverFires(other.describe()),
            });
        }
        if let Some(gap) = spec.min_gap(at, 8)?
            && gap < limits.min_interval_ms
        {
            return Err(ScheduleError::TooFrequent { gap_ms: gap, min_ms: limits.min_interval_ms });
        }
        let e = Event::Created { schedule };
        self.stage(std::slice::from_ref(&e))?;
        Ok(e)
    }

    pub fn cancel(&self, id: &str, by: &Actor, at: Timestamp) -> Result<Event, ScheduleError> {
        let e = Event::Cancelled { id: id.to_string(), by: by.clone(), at };
        self.stage(std::slice::from_ref(&e))?;
        Ok(e)
    }

    pub fn stage(&self, events: &[Event]) -> Result<Schedules, ScheduleError> {
        let mut s = self.clone();
        for e in events {
            s.apply(e)?;
        }
        Ok(s)
    }

    /// One fold step: recompute and require the logged values to match.
    pub fn apply(&mut self, e: &Event) -> Result<(), ScheduleError> {
        match e {
            Event::Created { schedule } => {
                if self.get(&schedule.id).is_some() {
                    return Err(ScheduleError::BadEvent(format!("schedule id {} is used twice", schedule.id)));
                }
                let expected =
                    fresh(&schedule.id, &schedule.prompt, &schedule.label, &schedule.spec, &schedule.created_by, schedule.created_at)?;
                if &expected != schedule || schedule.status != Status::Active {
                    return Err(ScheduleError::Tampered { id: schedule.id.clone(), what: "creation" });
                }
                self.list.push(schedule.clone());
                self.created += 1;
            }
            Event::Fired { id, n, due, at, skipped } => {
                let s = self.list.iter_mut().find(|s| &s.id == id).ok_or_else(|| ScheduleError::Unknown(id.clone()))?;
                if s.status != Status::Active {
                    return Err(ScheduleError::NotActive { id: id.clone(), status: s.status });
                }
                if *n != s.fires + 1 || Some(*due) != s.next_due || at < due {
                    return Err(ScheduleError::Tampered { id: id.clone(), what: "occurrence" });
                }
                if *skipped != skipped_between(&s.spec, *due, *at)? {
                    return Err(ScheduleError::Tampered { id: id.clone(), what: "skipped count" });
                }
                s.fires = *n;
                s.skipped += skipped;
                s.last_fired_at = Some(*at);
                s.next_due = s.spec.next_after(*at)?;
                if s.next_due.is_none() {
                    s.status = Status::Done;
                }
            }
            Event::Cancelled { id, by, .. } => {
                let s = self.list.iter_mut().find(|s| &s.id == id).ok_or_else(|| ScheduleError::Unknown(id.clone()))?;
                if s.status != Status::Active {
                    return Err(ScheduleError::NotActive { id: id.clone(), status: s.status });
                }
                if !may_cancel(by, &s.created_by) {
                    return Err(ScheduleError::NotYours { id: id.clone(), owner: s.created_by.clone() });
                }
                s.status = Status::Cancelled;
                s.cancelled_by = Some(by.clone());
                s.next_due = None;
            }
        }
        Ok(())
    }

    /// The earliest active occurrence due at `now` (ties by id), with everything the driver needs.
    /// Each schedule fires once however many occurrences it missed; `skipped` counts the rest.
    pub fn next_due(&self, now: Timestamp) -> Result<Option<Due>, ScheduleError> {
        let Some(s) =
            self.active().filter(|s| s.next_due.is_some_and(|d| d <= now)).min_by(|a, b| (a.next_due, &a.id).cmp(&(b.next_due, &b.id)))
        else {
            return Ok(None);
        };
        let Some(due) = s.next_due else { return Ok(None) };
        let n = s.fires + 1;
        let skipped = skipped_between(&s.spec, due, now)?;
        let event = Event::Fired { id: s.id.clone(), n, due, at: now, skipped };
        self.stage(std::slice::from_ref(&event))?;
        Ok(Some(Due {
            id: s.id.clone(),
            n,
            due,
            skipped,
            key: format!("schedule:{}:{n}", s.id),
            text: fire_text(s, n, due, now, skipped),
            event,
        }))
    }

    /// The driver's next look: the earliest `next_due` of an active schedule.
    pub fn next_wake(&self) -> Option<Timestamp> {
        self.active().filter_map(|s| s.next_due).min()
    }
}

/// The fired prompt: one provenance line (who scheduled it, which occurrence, how late, what was
/// folded in), then the prompt verbatim.
pub fn fire_text(s: &Schedule, n: u64, due: Timestamp, now: Timestamp, skipped: u64) -> String {
    let label = if s.label.is_empty() { String::new() } else { format!(" \"{}\"", s.label) };
    let mut head = format!(
        "[Scheduled prompt {}{label}: {}, occurrence {n}, due {}; created by {} at {}.",
        s.id,
        s.spec.describe(),
        format_utc(due),
        s.created_by,
        format_utc(s.created_at)
    );
    if now.0.saturating_sub(due.0) >= crate::spec::MINUTE {
        head.push_str(&format!(" It fired late, at {}, because the session was down or busy.", format_utc(now)));
    }
    if skipped > 0 {
        let more = if skipped >= SKIP_CAP { format!("{SKIP_CAP} or more") } else { skipped.to_string() };
        head.push_str(&format!(" {more} later occurrence(s) up to now were missed; they are covered by this one and will not be sent."));
    }
    head.push_str("]\n\n");
    head.push_str(&s.prompt);
    head
}

/// The `CustomFold` for `schedule/*`.
pub fn fold_schedules(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut s = project(prev)?;
    let e = Event::parse(name, body).map_err(|e| e.to_string())?;
    s.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&s).map_err(|e| format!("the schedule projection did not serialize: {e}"))
}

/// Decodes `State.custom["schedule"]` (absent = no schedules).
pub fn project(folded: Option<&Value>) -> Result<Schedules, String> {
    match folded {
        None => Ok(Schedules::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the schedule projection is unreadable: {e}")),
    }
}

/// Folds only the `schedule/*` entries of one branch (what the driver needs, without the core fold).
pub fn load(entries: &[aho_log::Entry]) -> Result<Schedules, String> {
    let mut s = Schedules::default();
    for e in entries {
        if let EntryKind::Custom { ns, name, body } = &e.kind
            && ns == NS
        {
            let ev = Event::parse(name, body).map_err(|x| format!("entry {}: {x}", e.id.0))?;
            s.apply(&ev).map_err(|x| format!("entry {}: {x}", e.id.0))?;
        }
    }
    Ok(s)
}
