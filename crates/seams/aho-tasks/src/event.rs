//! `Task/*` log events. Each is one `EntryKind::Custom{ns: "Task", name, body}`; the variant name is
//! the entry name and the variant's fields are the body.

use aho_log::EntryKind;
use aho_types::{AgentId, Value};
use serde::{Deserialize, Serialize};

use crate::invariant::InvariantSeen;
use crate::model::{Actor, Control, FailReason, HandoffArtifact, Kind, Origin, Salvage, Settings, TaskId, agent_serde};
use crate::{NS, TaskError};

/// A node as it enters the graph (seed, gap, auto gate, expansion child, gate injection).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: TaskId,
    #[serde(default)]
    pub content: String,
    pub kind: Kind,
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TaskId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
pub enum Event {
    /// Swarm knobs in force from here on.
    Configured {
        settings: Settings,
        by: Actor,
    },
    /// Idempotent by (id, content); same id + different content fails the fold.
    Seeded {
        id: TaskId,
        #[serde(default)]
        content: String,
        kind: Kind,
        #[serde(default)]
        depends_on: Vec<TaskId>,
        origin: Origin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<TaskId>,
        by: Actor,
    },
    /// A gate (or a synthesis join taking a new gap gate) gains scope. A completed gate re-opens.
    Rescoped {
        id: TaskId,
        add: Vec<TaskId>,
        by: Actor,
    },
    /// Only the owner, only while Running & unexpanded; children + auto gates arrive together.
    Expanded {
        parent: TaskId,
        children: Vec<NodeSpec>,
        joins: Vec<TaskId>,
        #[serde(with = "agent_serde")]
        by: AgentId,
    },
    Started {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
        /// Gates: the scope snapshot the verdict will be checked against.
        #[serde(default)]
        scope: Vec<TaskId>,
    },
    Heartbeat {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    Checkpoint {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
        summary: String,
        #[serde(default)]
        recent: Vec<String>,
    },
    Completed {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
        artifact: HandoffArtifact,
        /// Verify gates: every invariant covering the gate's files, with its status as the gate
        /// passed. None may be Fail or Stale.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invariants: Vec<InvariantSeen>,
    },
    /// A gate turning a miss into work.
    Injected {
        from_gate: TaskId,
        new_node: NodeSpec,
        #[serde(default)]
        addresses: Vec<TaskId>,
        #[serde(with = "agent_serde")]
        by: AgentId,
    },
    /// A gate whose scope changed after it started goes back to the queue.
    Released {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
    },
    /// Artifact-or-nothing, first miss: back to the queue once.
    Requeued {
        id: TaskId,
        #[serde(with = "agent_serde")]
        by: AgentId,
    },
    Failed {
        id: TaskId,
        reason: FailReason,
    },
    /// Dead assignee; `n` counts reclaims of this node, capped by `Settings::max_reclaims`.
    Reclaimed {
        id: TaskId,
        n: u32,
        #[serde(with = "agent_serde")]
        from: AgentId,
    },
    Control {
        id: TaskId,
        op: Control,
        by: Actor,
        #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
        to: Option<AgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salvage: Option<Salvage>,
    },
    /// A dependent's acceptance check failed: the producer (and gates that passed over it) re-open.
    Reopened {
        id: TaskId,
        consumer: TaskId,
        reason: String,
        #[serde(default)]
        gates: Vec<TaskId>,
        #[serde(with = "agent_serde")]
        by: AgentId,
    },
    Stopped {
        id: TaskId,
        by: Actor,
    },
    /// The session resumed: every Running node becomes RunningStale until its worker shows life.
    Resumed {},
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Configured { .. } => "Configured",
            Event::Seeded { .. } => "Seeded",
            Event::Rescoped { .. } => "Rescoped",
            Event::Expanded { .. } => "Expanded",
            Event::Started { .. } => "Started",
            Event::Heartbeat { .. } => "Heartbeat",
            Event::Checkpoint { .. } => "Checkpoint",
            Event::Completed { .. } => "Completed",
            Event::Injected { .. } => "Injected",
            Event::Released { .. } => "Released",
            Event::Requeued { .. } => "Requeued",
            Event::Failed { .. } => "Failed",
            Event::Reclaimed { .. } => "Reclaimed",
            Event::Control { .. } => "Control",
            Event::Reopened { .. } => "Reopened",
            Event::Stopped { .. } => "Stopped",
            Event::Resumed {} => "Resumed",
        }
    }

    /// Every entry name this crate writes (`Task/<name>`).
    pub const NAMES: &'static [&'static str] = &[
        "Configured",
        "Seeded",
        "Rescoped",
        "Expanded",
        "Started",
        "Heartbeat",
        "Checkpoint",
        "Completed",
        "Injected",
        "Released",
        "Requeued",
        "Failed",
        "Reclaimed",
        "Control",
        "Reopened",
        "Stopped",
        "Resumed",
    ];

    pub fn body(&self) -> Result<Value, TaskError> {
        let mut v = serde_json::to_value(self).map_err(|e| TaskError::BadEvent(format!("Task/{} did not serialize: {e}", self.name())))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Object(Default::default())))
    }

    /// Decodes one `Task/<name>` body. Unknown names and malformed bodies are errors.
    pub fn parse(name: &str, body: &Value) -> Result<Event, TaskError> {
        if !Event::NAMES.contains(&name) {
            return Err(TaskError::BadEvent(format!("unknown task event `Task/{name}`")));
        }
        serde_json::from_value(serde_json::json!({ "name": name, "body": body }))
            .map_err(|e| TaskError::BadEvent(format!("malformed Task/{name}: {e}")))
    }

    /// `{ "ns": "Task", "name": .., "body": .. }` — the shape tools return under `value.events`.
    pub fn to_json(&self) -> Result<Value, TaskError> {
        Ok(serde_json::json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }

    pub fn to_entry(&self) -> Result<EntryKind, TaskError> {
        Ok(EntryKind::Custom { ns: NS.to_string(), name: self.name().to_string(), body: self.body()? })
    }
}
