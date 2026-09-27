//! `Task/*` events as the records `formal/Tasks.tla`'s `TraceStep(e)` reads; each session log is a
//! behavior. `aho_formal::trace_for("Tasks", …)` adds the entry fields (`id`, `parent`, `by`, `at`)
//! and sends every `Task/*` entry through [`tla_record`].
//!
//! Conventions the spec relies on: `task` is the node the event names; agents are `AgentId + 1`
//! (the spec's 0 is "nobody", and a session's own model is agent 0); whoever is not an agent is 0;
//! an absent parent is `""`; lists stay lists (tuples in TLA+). A seeded task's parent is `under`:
//! `parent` is the entry's own parent, which `Log.tla` reads from every record.

use aho_types::{AgentId, Value};
use serde_json::json;

use crate::event::{Event, NodeSpec};
use crate::model::{Actor, FailReason, Origin, TaskId};

fn ids(v: &[TaskId]) -> Value {
    json!(v.iter().map(TaskId::as_str).collect::<Vec<_>>())
}

/// `AgentId + 1`. An id past `u64` stays out of range, so writing the trace as TLA+ refuses it
/// (TLC's integers are 32-bit) instead of folding two agents into one.
fn agent(a: AgentId) -> Value {
    Value::from(u64::try_from(a.0).ok().and_then(|n| n.checked_add(1)).unwrap_or(u64::MAX))
}

fn origin(o: Origin) -> &'static str {
    match o {
        Origin::Seed => "seed",
        Origin::Expand => "expand",
        Origin::Gap => "gap",
        Origin::Gate => "gate",
    }
}

fn node_spec(n: &NodeSpec) -> Value {
    json!({
        "id": n.id.as_str(), "kind": n.kind.as_str(), "deps": ids(&n.depends_on), "origin": origin(n.origin),
        "parent": n.parent.as_ref().map(TaskId::as_str).unwrap_or(""),
    })
}

/// `(kind, agent)`: an agent's number, 0 for everyone else.
fn actor(a: &Actor) -> (&'static str, Value) {
    match a {
        Actor::Operator => ("operator", json!(0)),
        Actor::Engineer => ("engineer", json!(0)),
        Actor::Agent(n) => ("agent", agent(*n)),
        Actor::System(_) => ("system", json!(0)),
    }
}

/// The `TraceStep` record of one event: `action` is the event name, plus the fields its action
/// reads. Content, artifact text, checkpoint and salvage payloads are not modelled and not written.
pub fn tla_record(e: &Event) -> Value {
    match e {
        Event::Configured { settings, .. } => json!({
            "action": "Configured", "maxMembers": settings.max_members, "maxLive": settings.max_live_workers,
            "maxReclaims": settings.max_reclaims,
        }),
        Event::Seeded { id, kind, depends_on, origin, parent, .. } => json!({
            "action": "Seeded", "task": id.as_str(), "kind": kind.as_str(), "deps": ids(depends_on), "origin": self::origin(*origin),
            "under": parent.as_ref().map(TaskId::as_str).unwrap_or(""),
        }),
        Event::Rescoped { id, add, .. } => json!({"action": "Rescoped", "task": id.as_str(), "add": ids(add)}),
        Event::Expanded { parent, children, joins, by } => json!({
            "action": "Expanded", "task": parent.as_str(), "children": children.iter().map(node_spec).collect::<Vec<_>>(),
            "joins": ids(joins), "agent": agent(*by),
        }),
        Event::Started { id, by, scope } => json!({"action": "Started", "task": id.as_str(), "agent": agent(*by), "scope": ids(scope)}),
        Event::Heartbeat { id, by, .. } => json!({"action": "Heartbeat", "task": id.as_str(), "agent": agent(*by)}),
        Event::Checkpoint { id, by, .. } => json!({"action": "Checkpoint", "task": id.as_str(), "agent": agent(*by)}),
        Event::Completed { id, by, artifact, invariants } => json!({
            "action": "Completed", "task": id.as_str(), "agent": agent(*by), "conf": artifact.confidence.as_str(),
            "covers": ids(&artifact.covers),
            "invs": invariants.iter().map(|i| json!({"id": i.id, "status": i.status.as_str()})).collect::<Vec<_>>(),
        }),
        Event::Injected { from_gate, new_node, addresses, by } => json!({
            "action": "Injected", "task": from_gate.as_str(), "new": node_spec(new_node), "addresses": ids(addresses), "agent": agent(*by),
        }),
        Event::Released { id, by } => json!({"action": "Released", "task": id.as_str(), "agent": agent(*by)}),
        Event::Requeued { id, by } => json!({"action": "Requeued", "task": id.as_str(), "agent": agent(*by)}),
        Event::Failed { id, reason } => json!({
            "action": "Failed", "task": id.as_str(),
            "reason": match reason { FailReason::NoArtifact => "no_artifact", FailReason::ReclaimCap => "reclaim_cap" },
        }),
        Event::Reclaimed { id, n, from } => json!({"action": "Reclaimed", "task": id.as_str(), "n": n, "agent": agent(*from)}),
        Event::Control { id, op, by, to, salvage } => json!({
            "action": "Control", "task": id.as_str(), "op": op.as_str(), "by": actor(by).0, "to": to.map(agent).unwrap_or(json!(0)),
            "salvage": match salvage { None => "none", Some(s) if s.is_empty() => "empty", Some(_) => "given" },
        }),
        Event::Reopened { id, consumer, gates, by, .. } => json!({
            "action": "Reopened", "task": id.as_str(), "consumer": consumer.as_str(), "gates": ids(gates), "agent": agent(*by),
        }),
        Event::Stopped { id, by } => {
            let (kind, who) = actor(by);
            json!({"action": "Stopped", "task": id.as_str(), "by": kind, "agent": who})
        }
        Event::Resumed {} => json!({"action": "Resumed"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HandoffArtifact;
    use crate::invariant::{InvariantSeen, InvariantStatus};

    #[test]
    fn agents_shift_by_one_so_the_session_model_is_not_nobody() {
        let a = HandoffArtifact::parse(&json!({"findings": "ok", "confidence": "high", "what_i_did_not_check": ["perf"], "covers": ["b"]}))
            .unwrap();
        let seen = vec![InvariantSeen { id: "inv.a".into(), status: InvariantStatus::Pass }];
        let r = tla_record(&Event::Completed { id: TaskId::new("a/verify"), by: AgentId(0), artifact: a, invariants: seen });
        assert_eq!(r["agent"], json!(1), "the spec's owner 0 means nobody holds the task");
        assert_eq!(r["invs"], json!([{"id": "inv.a", "status": "pass"}]));
        assert_eq!(agent(AgentId(u128::MAX)), json!(u64::MAX), "never folded onto a real agent");
    }
}
