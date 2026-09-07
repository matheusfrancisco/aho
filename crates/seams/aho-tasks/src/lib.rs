//! `12` — the swarm task graph as a pure engine over log events.
//!
//! * Nodes are `Task/*` log kinds (`EntryKind::Custom{ns: "Task", name, body}`); the projection is
//!   [`Graph`], folded by [`fold_tasks`] — the `CustomFold` a capability registers with
//!   `Registrar::entry_kind(NS, fold_tasks)`. Every transition is validated in the fold; a bad
//!   event fails replay loudly.
//! * Engine methods ([`Graph::seed`], [`Graph::expand`], [`Graph::dispatch`], [`Graph::complete`],
//!   [`Graph::inject_from_gate`], [`Graph::control`], …) never mutate: they return the events to
//!   append, staged on a clone through the same [`Graph::apply`] the fold uses.
//! * `Blocked` is computed from dependencies ([`Derived`]), never stored.
//! * The dependency edge carries a typed [`HandoffArtifact`]; thin artifacts are rejected, code and
//!   synthesize nodes need a [`Contract`] in deep mode, and a dependent's input is hydrated under
//!   "# Inputs from completed dependencies" ([`Graph::assemble_input`]).
//! * Gates are auto-inserted (Critique over explore children, Verify over code children, a root gate
//!   over all top-level work) and structurally cannot rubber-stamp ([`GateError`]); a verify gate
//!   cannot pass while an invariant covering its files is Fail or Stale ([`InvariantSeen`]).
//! * [`sim`] drives the whole engine with scripted workers and no model.
//! * [`tla_record`] maps each event to the record `formal/Tasks.tla`'s `TraceStep` reads.

#![forbid(unsafe_code)]

mod engine;
mod error;
mod event;
mod graph;
mod invariant;
mod model;
mod render;
mod salvage;
pub mod sim;
mod tla;

pub use engine::{ChildSpec, Dispatch, Growth, Injection, SeedSpec};
pub use error::{GateError, TaskError};
pub use event::{Event, NodeSpec};
pub use graph::{Derived, Graph};
pub use invariant::{InvariantSeen, InvariantStatus};
pub use model::{
    Actor, Confidence, Contract, Control, FailReason, GateKind, HandoffArtifact, Kind, MAX_ID_LEN, Node, Origin, Output, RECENT_CAP,
    ROOT_GATE, Salvage, Settings, Status, TaskId, agent_label, agent_serde, gate_id, parse_agent,
};
pub use render::{Hydrate, REFERENCE_FINDINGS_CHARS};
pub use tla::tla_record;

use aho_types::Value;

/// Log namespace: entries are `Task/<Event::name()>`.
pub const NS: &str = "Task";

/// The `CustomFold` for `Task/*` (`aho_log::CustomFold`): decode the previous projection, validate
/// and apply one event, encode it back. An illegal transition is an `Err`, which fails the log fold.
pub fn fold_tasks(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut g = project(prev)?;
    let e = Event::parse(name, body).map_err(|e| e.to_string())?;
    g.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&g).map_err(|e| format!("the task graph did not serialize: {e}"))
}

/// Decodes `State.custom["Task"]` (absent = empty graph).
pub fn project(folded: Option<&Value>) -> Result<Graph, String> {
    match folded {
        None => Ok(Graph::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the task graph projection is unreadable: {e}")),
    }
}
