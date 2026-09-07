//! Engine operations. Each builds the events for one request and stages them on a clone through
//! `Graph::apply`; the events are returned only if every one of them is a legal transition.
//! Nothing here mutates `self`: the caller appends the events and folds them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aho_types::AgentId;

use crate::error::TaskError;
use crate::event::{Event, NodeSpec};
use crate::graph::Graph;
use crate::invariant::InvariantSeen;
use crate::model::{
    Actor, Control, FailReason, GateKind, HandoffArtifact, Kind, Origin, ROOT_GATE, Salvage, Settings, Status, TaskId, gate_id,
};

/// A caller-seeded task. `parent` names an already-expanded task whose scope this fills (origin Gap).
#[derive(Clone, Debug, PartialEq)]
pub struct SeedSpec {
    pub id: TaskId,
    pub content: String,
    pub kind: Kind,
    pub depends_on: Vec<TaskId>,
    pub parent: Option<TaskId>,
}

/// A child of an expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildSpec {
    pub id: TaskId,
    pub content: String,
    pub kind: Kind,
    pub depends_on: Vec<TaskId>,
}

/// Work a gate injects instead of passing; `addresses` names the in-scope siblings it follows up.
#[derive(Clone, Debug, PartialEq)]
pub struct Injection {
    pub id: TaskId,
    pub content: String,
    pub kind: Kind,
    pub depends_on: Vec<TaskId>,
    pub addresses: Vec<TaskId>,
}

/// What `dispatch` decided for one worker.
#[derive(Clone, Debug, PartialEq)]
pub enum Dispatch {
    /// A node started (possibly after releasing a stale gate the worker held).
    Started {
        id: TaskId,
        events: Vec<Event>,
    },
    /// The worker's stale gate went back to the queue and nothing else is ready for it.
    Released {
        id: TaskId,
        events: Vec<Event>,
    },
    /// The worker already holds this node.
    Holding(TaskId),
    /// Nothing ready for this worker: work is running, blocked on unfinished dependencies, or
    /// ready but reassigned to another agent (`reserved`).
    Idle {
        running: usize,
        blocked: usize,
        reserved: usize,
    },
    AtCapacity(String),
    /// No node is queued or running.
    Finished,
}

/// Seeded vs grown, by origin (12: an under-explored plan is visible).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Growth {
    pub seed: usize,
    pub expand: usize,
    pub gap: usize,
    /// Work injected by gates.
    pub gate: usize,
    /// Gates the graph inserted.
    pub gates: usize,
}

impl Growth {
    pub fn grown(&self) -> usize {
        self.expand + self.gap + self.gate
    }
}

impl std::fmt::Display for Growth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "seeded {} · grown {} (expand {}, gap {}, from gates {}) · {} gates",
            self.seed,
            self.grown(),
            self.expand,
            self.gap,
            self.gate,
            self.gates
        )
    }
}

fn gate_brief(kind: GateKind, parent: Option<&TaskId>) -> String {
    match (kind, parent) {
        (GateKind::Root, _) => "Root gate: review the whole plan once every top-level task is done. Name every task in scope in \
            `covers`; a LOW-confidence task needs injected follow-up work before this gate can pass."
            .into(),
        (GateKind::Critique, Some(p)) => format!(
            "Critique the findings produced under `{p}` before they are synthesized. Name every task in scope in `covers`; turn gaps \
             and LOW-confidence results into injected work."
        ),
        (GateKind::Verify, Some(p)) => format!(
            "Verify the code produced under `{p}` before it is synthesized: run each producer's contract acceptance checks. Name every \
             task in scope in `covers`; turn failures and LOW-confidence results into injected work."
        ),
        (_, None) => "Gate".into(),
    }
}

impl Graph {
    /// `Task/Configured` when the settings differ from the ones in force.
    pub fn configure(&self, by: &Actor, settings: &Settings) -> Result<Vec<Event>, TaskError> {
        if self.settings() == settings {
            return Ok(vec![]);
        }
        let e = Event::Configured { settings: settings.clone(), by: by.clone() };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// Idempotent by (id, content): re-sending a seeded task is a no-op, the same id with other content
    /// is `DuplicateNode`. New top-level tasks join the root gate (created on first seed, re-opened
    /// when a reseed widens it). A task with `parent` fills a gap in that expansion and joins its gate.
    pub fn seed(&self, by: &Actor, specs: Vec<SeedSpec>) -> Result<Vec<Event>, TaskError> {
        let mut fresh: Vec<SeedSpec> = Vec::new();
        for s in specs {
            if s.kind.is_gate() {
                return Err(TaskError::InvalidKind(s.kind.as_str().into()));
            }
            s.id.check_user()?;
            if s.content.trim().is_empty() {
                return Err(TaskError::EmptyContent(s.id.clone()));
            }
            let prior =
                self.node(&s.id).map(|n| n.content.as_str()).or_else(|| fresh.iter().find(|f| f.id == s.id).map(|f| f.content.as_str()));
            match prior {
                Some(c) if c == s.content => continue,
                Some(_) => return Err(TaskError::DuplicateNode(s.id.clone())),
                None => fresh.push(s),
            }
        }
        if fresh.is_empty() {
            return Ok(vec![]);
        }
        let order = topo_batch(&fresh, |d| self.node(d).is_some())?;

        let mut events = Vec::new();
        for s in &order {
            events.push(Event::Seeded {
                id: s.id.clone(),
                content: s.content.clone(),
                kind: s.kind,
                depends_on: s.depends_on.clone(),
                origin: if s.parent.is_some() { Origin::Gap } else { Origin::Seed },
                parent: s.parent.clone(),
                by: by.clone(),
            });
        }

        let tops: Vec<TaskId> = order.iter().filter(|s| s.parent.is_none()).map(|s| s.id.clone()).collect();
        if !tops.is_empty() {
            let root = TaskId::new(ROOT_GATE);
            if self.node(&root).is_some() {
                events.push(Event::Rescoped { id: root, add: tops, by: by.clone() });
            } else {
                let mut scope: Vec<TaskId> = self
                    .nodes()
                    .into_iter()
                    .filter(|n| n.parent.is_none() && !n.kind.is_gate() && n.origin == Origin::Seed)
                    .map(|n| n.id.clone())
                    .collect();
                scope.extend(tops);
                events.push(Event::Seeded {
                    id: root,
                    content: gate_brief(GateKind::Root, None),
                    kind: Kind::Gate(GateKind::Root),
                    depends_on: scope,
                    origin: Origin::Gate,
                    parent: None,
                    by: by.clone(),
                });
            }
        }

        let mut gaps: BTreeMap<(TaskId, GateKind), Vec<TaskId>> = BTreeMap::new();
        for s in order.iter() {
            if let Some(p) = &s.parent {
                gaps.entry((p.clone(), s.kind.gate_for())).or_default().push(s.id.clone());
            }
        }
        for ((parent, gk), ids) in gaps {
            let gid = gate_id(&parent, gk);
            if self.node(&gid).is_some() {
                events.push(Event::Rescoped { id: gid, add: ids, by: by.clone() });
            } else {
                events.push(Event::Seeded {
                    id: gid.clone(),
                    content: gate_brief(gk, Some(&parent)),
                    kind: Kind::Gate(gk),
                    depends_on: ids,
                    origin: Origin::Gate,
                    parent: Some(parent.clone()),
                    by: by.clone(),
                });
                events.push(Event::Rescoped { id: parent, add: vec![gid], by: by.clone() });
            }
        }
        self.stage(&events)?;
        Ok(events)
    }

    /// Only the owner, only while Running & unexpanded. Explore/synthesize children get a Critique
    /// gate, code children a Verify gate, between them and the parent — which is re-queued as the
    /// synthesis join with affinity to the planner that expanded it.
    pub fn expand(&self, by: AgentId, parent: &TaskId, children: Vec<ChildSpec>) -> Result<Vec<Event>, TaskError> {
        let mut specs = Vec::new();
        for c in &children {
            if c.kind.is_gate() {
                return Err(TaskError::InvalidKind(c.kind.as_str().into()));
            }
            specs.push(NodeSpec {
                id: c.id.clone(),
                content: c.content.clone(),
                kind: c.kind,
                depends_on: c.depends_on.clone(),
                origin: Origin::Expand,
                parent: Some(parent.clone()),
            });
        }
        let mut joins = Vec::new();
        for gk in [GateKind::Critique, GateKind::Verify] {
            let scope: Vec<TaskId> = children.iter().filter(|c| c.kind.gate_for() == gk).map(|c| c.id.clone()).collect();
            if scope.is_empty() {
                continue;
            }
            let gid = gate_id(parent, gk);
            specs.push(NodeSpec {
                id: gid.clone(),
                content: gate_brief(gk, Some(parent)),
                kind: Kind::Gate(gk),
                depends_on: scope,
                origin: Origin::Gate,
                parent: Some(parent.clone()),
            });
            joins.push(gid);
        }
        let e = Event::Expanded { parent: parent.clone(), children: specs, joins, by };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// Picks the next ready node for `worker`: pinned-to-it first, then its own synthesis joins
    /// (planner affinity), then unclaimed work, then others' joins. A stale gate the worker holds is
    /// released (and restarted over the widened scope if that scope is done).
    pub fn dispatch(&self, worker: AgentId) -> Result<Dispatch, TaskError> {
        let mut g = self.clone();
        let mut events = Vec::new();
        let mut released = None;
        if let Some(h) = g.held_by(worker) {
            if !g.gate_is_stale(h) {
                return Ok(Dispatch::Holding(h.id.clone()));
            }
            let e = Event::Released { id: h.id.clone(), by: worker };
            released = Some(h.id.clone());
            g.apply(&e)?;
            events.push(e);
        }
        let mut ready: Vec<(u8, u64, TaskId)> = g
            .nodes()
            .into_iter()
            .filter(|n| n.status == Status::Queued && g.open_deps(n).is_empty())
            .filter(|n| n.pinned.map(|p| p == worker).unwrap_or(true))
            .map(|n| {
                let class = if n.pinned == Some(worker) {
                    0
                } else if n.planner == Some(worker) {
                    1
                } else if n.planner.is_none() {
                    2
                } else {
                    3
                };
                (class, n.seq, n.id.clone())
            })
            .collect();
        ready.sort();
        let Some((_, _, id)) = ready.into_iter().next() else {
            if let Some(id) = released {
                return Ok(Dispatch::Released { id, events });
            }
            if g.nodes().iter().all(|n| n.status.is_terminal()) {
                return Ok(Dispatch::Finished);
            }
            let running = g.nodes().iter().filter(|n| n.status.is_running()).count();
            let queued: Vec<_> = g.nodes().into_iter().filter(|n| n.status == Status::Queued).collect();
            let blocked = queued.iter().filter(|n| !g.open_deps(n).is_empty()).count();
            let reserved = queued.len() - blocked;
            return Ok(Dispatch::Idle { running, blocked, reserved });
        };
        let n = g.get(&id)?;
        let scope = if n.kind.is_gate() { n.depends_on.clone() } else { vec![] };
        let e = Event::Started { id: id.clone(), by: worker, scope };
        match g.apply(&e) {
            Ok(()) => {}
            Err(TaskError::AtCapacity(why)) => {
                return Ok(match released {
                    Some(id) => Dispatch::Released { id, events },
                    None => Dispatch::AtCapacity(why),
                });
            }
            Err(e) => return Err(e),
        }
        events.push(e);
        Ok(Dispatch::Started { id, events })
    }

    /// Hands over `artifact`. `invariants`: for a verify gate, every invariant covering its files
    /// ([`Graph::gate_files`]) with its status now; empty for any other task.
    pub fn complete(
        &self,
        by: AgentId,
        id: &TaskId,
        artifact: HandoffArtifact,
        invariants: Vec<InvariantSeen>,
    ) -> Result<Vec<Event>, TaskError> {
        let e = Event::Completed { id: id.clone(), by, artifact, invariants };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// A gate converts misses into work: one `Injected` per task, then the gate goes back to the
    /// queue until that work completes (its scope grew, so its old dispatch is stale).
    pub fn inject_from_gate(&self, by: AgentId, gate: &TaskId, work: Vec<Injection>) -> Result<Vec<Event>, TaskError> {
        let g = self.get(gate)?;
        if !g.kind.is_gate() {
            return Err(TaskError::NotAGate(gate.clone()));
        }
        if work.is_empty() {
            return Err(TaskError::BadEvent("inject needs at least one task".into()));
        }
        let parent = g.parent.clone();
        let mut events = Vec::new();
        for w in work {
            if w.kind.is_gate() {
                return Err(TaskError::InvalidKind(w.kind.as_str().into()));
            }
            events.push(Event::Injected {
                from_gate: gate.clone(),
                new_node: NodeSpec {
                    id: w.id,
                    content: w.content,
                    kind: w.kind,
                    depends_on: w.depends_on,
                    origin: Origin::Gate,
                    parent: parent.clone(),
                },
                addresses: w.addresses,
                by,
            });
        }
        events.push(Event::Released { id: gate.clone(), by });
        self.stage(&events)?;
        Ok(events)
    }

    /// Operator control with the safe-state table (`Control::safe_from`). `Salvage` without an
    /// explicit snapshot forwards the node's own checkpoint and recent tool summaries.
    pub fn control(
        &self,
        by: &Actor,
        id: &TaskId,
        op: Control,
        to: Option<AgentId>,
        salvage: Option<Salvage>,
    ) -> Result<Vec<Event>, TaskError> {
        let n = self.get(id)?;
        let salvage = match (op, salvage) {
            (Control::Salvage, Some(s)) => Some(s),
            (Control::Salvage, None) => {
                Some(Salvage { from: n.owner.or(n.last_owner), checkpoint: n.checkpoint.clone(), summaries: n.recent.clone() })
            }
            (_, s) => s,
        };
        let e = Event::Control { id: id.clone(), op, by: by.clone(), to, salvage };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// An agent stops only its own subtree; the operator stops anything.
    pub fn stop(&self, by: &Actor, id: &TaskId) -> Result<Vec<Event>, TaskError> {
        let e = Event::Stopped { id: id.clone(), by: by.clone() };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// A consumer whose acceptance check failed re-opens the producer (and every gate that passed
    /// over it); the consumer goes back to the queue until the producer delivers again.
    pub fn reopen(&self, by: AgentId, consumer: &TaskId, producer: &TaskId, reason: &str) -> Result<Vec<Event>, TaskError> {
        let gates: Vec<TaskId> = self
            .nodes()
            .into_iter()
            .filter(|g| g.kind.is_gate() && g.depends_on.contains(producer))
            .filter(|g| g.status == Status::Completed || g.status.is_running())
            .map(|g| g.id.clone())
            .collect();
        let e = Event::Reopened { id: producer.clone(), consumer: consumer.clone(), reason: reason.to_string(), gates, by };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    pub fn heartbeat(&self, by: AgentId, id: &TaskId, tool: Option<String>) -> Result<Vec<Event>, TaskError> {
        let e = Event::Heartbeat { id: id.clone(), by, tool };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    pub fn checkpoint(&self, by: AgentId, id: &TaskId, summary: &str, recent: Vec<String>) -> Result<Vec<Event>, TaskError> {
        let e = Event::Checkpoint { id: id.clone(), by, summary: summary.trim().to_string(), recent };
        self.stage(std::slice::from_ref(&e))?;
        Ok(vec![e])
    }

    /// Artifact-or-nothing: `worker`'s turn ended with its node still running. First miss requeues,
    /// the second fails `NoArtifact`.
    pub fn turn_ended(&self, worker: AgentId) -> Result<Vec<Event>, TaskError> {
        let mut events = Vec::new();
        for n in self.nodes().into_iter().filter(|n| n.status.is_running() && n.owner == Some(worker)) {
            events.push(if n.no_artifact_requeues == 0 {
                Event::Requeued { id: n.id.clone(), by: worker }
            } else {
                Event::Failed { id: n.id.clone(), reason: FailReason::NoArtifact }
            });
        }
        self.stage(&events)?;
        Ok(events)
    }

    /// Dead assignee: reclaim each node it held, up to `max_reclaims` times, then fail it.
    pub fn reclaim_dead(&self, dead: AgentId) -> Result<Vec<Event>, TaskError> {
        let max = self.settings().max_reclaims;
        let mut events = Vec::new();
        for n in self.nodes().into_iter().filter(|n| n.status.is_running() && n.owner == Some(dead)) {
            events.push(if n.reclaims < max {
                Event::Reclaimed { id: n.id.clone(), n: n.reclaims + 1, from: dead }
            } else {
                Event::Failed { id: n.id.clone(), reason: FailReason::ReclaimCap }
            });
        }
        self.stage(&events)?;
        Ok(events)
    }

    /// On resume: `Task/Started` without `Completed` is stale. One `Resumed` if anything is running.
    pub fn running_stale(&self) -> Vec<Event> {
        if self.has_running() { vec![Event::Resumed {}] } else { vec![] }
    }

    pub fn growth(&self) -> Growth {
        let mut g = Growth::default();
        for n in self.nodes() {
            if n.kind.is_gate() {
                g.gates += 1;
                continue;
            }
            match n.origin {
                Origin::Seed => g.seed += 1,
                Origin::Expand => g.expand += 1,
                Origin::Gap => g.gap += 1,
                Origin::Gate => g.gate += 1,
            }
        }
        g
    }
}

/// Orders a seed batch so every `Seeded` names only existing dependencies; reports unknown deps
/// and cycles inside the batch.
fn topo_batch(batch: &[SeedSpec], exists: impl Fn(&TaskId) -> bool) -> Result<Vec<SeedSpec>, TaskError> {
    let ids: BTreeSet<&TaskId> = batch.iter().map(|s| &s.id).collect();
    let mut indeg: BTreeMap<&TaskId, usize> = BTreeMap::new();
    let mut out: BTreeMap<&TaskId, Vec<&TaskId>> = BTreeMap::new();
    for s in batch {
        indeg.entry(&s.id).or_default();
        for d in &s.depends_on {
            if ids.contains(d) {
                *indeg.entry(&s.id).or_default() += 1;
                out.entry(d).or_default().push(&s.id);
            } else if !exists(d) {
                return Err(TaskError::UnknownDependency { task: s.id.clone(), dep: d.clone() });
            }
        }
    }
    let mut q: VecDeque<&TaskId> = batch.iter().map(|s| &s.id).filter(|id| indeg.get(id) == Some(&0)).collect();
    let mut order = Vec::new();
    while let Some(id) = q.pop_front() {
        if let Some(s) = batch.iter().find(|s| &s.id == id) {
            order.push(s.clone());
        }
        for m in out.get(id).into_iter().flatten() {
            if let Some(d) = indeg.get_mut(m) {
                *d -= 1;
                if *d == 0 {
                    q.push_back(m);
                }
            }
        }
    }
    if order.len() < batch.len() {
        let stuck: Vec<TaskId> = batch.iter().filter(|s| indeg.get(&s.id).map(|d| *d > 0).unwrap_or(false)).map(|s| s.id.clone()).collect();
        return Err(TaskError::Cycle(stuck));
    }
    Ok(order)
}
