//! The projection and its transition rules. `Graph::apply` is the only mutator; the log fold and
//! every engine method go through it, so a transition the engine can produce is exactly one the
//! fold accepts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aho_types::AgentId;
use serde::{Deserialize, Serialize};

use crate::error::{GateError, TaskError};
use crate::event::{Event, NodeSpec};
use crate::invariant::InvariantSeen;
use crate::model::{
    Actor, Confidence, Control, FailReason, GateKind, HandoffArtifact, Kind, Node, Origin, ROOT_GATE, Salvage, Settings, Status, TaskId,
    agent_label, agent_serde, gate_id,
};

/// Computed, never stored: what a node looks like to a scheduler or a reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Derived {
    Ready,
    Blocked(Vec<TaskId>),
    Running(Option<AgentId>),
    Stale(Option<AgentId>),
    Completed,
    Failed(Option<FailReason>),
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    nodes: BTreeMap<TaskId, Node>,
    #[serde(default, with = "agent_serde::set")]
    members: BTreeSet<AgentId>,
    #[serde(default)]
    next_seq: u64,
}

impl Graph {
    pub fn new(settings: Settings) -> Graph {
        Graph { settings, ..Graph::default() }
    }

    // ───────────────────────────── reads ─────────────────────────────

    pub fn settings(&self) -> &Settings {
        &self.settings
    }
    pub fn node(&self, id: &TaskId) -> Option<&Node> {
        self.nodes.get(id)
    }
    pub fn get(&self, id: &TaskId) -> Result<&Node, TaskError> {
        self.nodes.get(id).ok_or_else(|| TaskError::UnknownTask(id.clone()))
    }
    fn get_mut(&mut self, id: &TaskId) -> Result<&mut Node, TaskError> {
        self.nodes.get_mut(id).ok_or_else(|| TaskError::UnknownTask(id.clone()))
    }
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    /// Nodes in insertion order.
    pub fn nodes(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.nodes.values().collect();
        v.sort_by_key(|n| n.seq);
        v
    }
    pub fn members(&self) -> &BTreeSet<AgentId> {
        &self.members
    }

    fn sorted(&self, ids: impl IntoIterator<Item = TaskId>) -> Vec<TaskId> {
        let mut v: Vec<TaskId> = ids.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        v.sort_by_key(|id| self.nodes.get(id).map(|n| n.seq).unwrap_or(u64::MAX));
        v
    }

    /// Dependencies not yet completed.
    pub fn open_deps(&self, n: &Node) -> Vec<TaskId> {
        n.depends_on.iter().filter(|d| self.nodes.get(*d).map(|x| x.status != Status::Completed).unwrap_or(true)).cloned().collect()
    }

    pub fn derived(&self, id: &TaskId) -> Option<Derived> {
        let n = self.nodes.get(id)?;
        Some(match n.status {
            Status::Queued => {
                let open = self.open_deps(n);
                if open.is_empty() { Derived::Ready } else { Derived::Blocked(open) }
            }
            Status::Running => Derived::Running(n.owner),
            Status::RunningStale => Derived::Stale(n.owner),
            Status::Completed => Derived::Completed,
            Status::Failed => Derived::Failed(n.failure),
            Status::Stopped => Derived::Stopped,
        })
    }

    /// Queued nodes whose dependencies are all done, in insertion order. Derived from deps; never stored.
    pub fn ready(&self) -> Vec<TaskId> {
        self.nodes().into_iter().filter(|n| n.status == Status::Queued && self.open_deps(n).is_empty()).map(|n| n.id.clone()).collect()
    }

    pub fn dependents(&self, id: &TaskId) -> Vec<TaskId> {
        self.nodes().into_iter().filter(|n| n.depends_on.contains(id)).map(|n| n.id.clone()).collect()
    }

    /// Does `from` transitively depend on `target`?
    pub fn reaches(&self, from: &TaskId, target: &TaskId) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from.clone()];
        while let Some(x) = stack.pop() {
            if &x == target {
                return true;
            }
            if !seen.insert(x.clone()) {
                continue;
            }
            if let Some(n) = self.nodes.get(&x) {
                stack.extend(n.depends_on.iter().cloned());
            }
        }
        false
    }

    /// Kahn over the whole graph; the nodes left over are on a cycle.
    pub fn check_acyclic(&self) -> Result<(), TaskError> {
        let mut indeg: BTreeMap<&TaskId, usize> = self.nodes.keys().map(|k| (k, 0)).collect();
        let mut out: BTreeMap<&TaskId, Vec<&TaskId>> = BTreeMap::new();
        for n in self.nodes.values() {
            for d in &n.depends_on {
                if self.nodes.contains_key(d) {
                    *indeg.entry(&n.id).or_default() += 1;
                    out.entry(d).or_default().push(&n.id);
                }
            }
        }
        let mut q: VecDeque<&TaskId> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
        let mut done = 0usize;
        while let Some(k) = q.pop_front() {
            done += 1;
            for m in out.get(k).into_iter().flatten() {
                if let Some(d) = indeg.get_mut(m) {
                    *d -= 1;
                    if *d == 0 {
                        q.push_back(m);
                    }
                }
            }
        }
        if done == self.nodes.len() {
            return Ok(());
        }
        Err(TaskError::Cycle(self.sorted(indeg.into_iter().filter(|(_, d)| *d > 0).map(|(k, _)| k.clone()))))
    }

    /// Agents holding a running node.
    pub fn live_workers(&self) -> BTreeSet<AgentId> {
        self.nodes.values().filter(|n| n.status.is_running()).filter_map(|n| n.owner).collect()
    }

    pub fn held_by(&self, a: AgentId) -> Option<&Node> {
        self.nodes().into_iter().find(|n| n.status.is_running() && n.owner == Some(a))
    }

    pub fn all_terminal(&self) -> bool {
        self.nodes.values().all(|n| n.status.is_terminal())
    }

    pub fn has_running(&self) -> bool {
        self.nodes.values().any(|n| n.status == Status::Running)
    }

    /// What a node consumes: direct deps plus, through each gate dep, the gate's scope.
    pub fn inputs(&self, id: &TaskId) -> Vec<TaskId> {
        let Some(n) = self.nodes.get(id) else { return vec![] };
        let mut out = Vec::new();
        for d in &n.depends_on {
            out.push(d.clone());
            if let Some(g) = self.nodes.get(d)
                && g.kind.is_gate()
            {
                out.extend(g.depends_on.iter().cloned());
            }
        }
        self.sorted(out)
    }

    /// Current scope of a gate differs from the snapshot taken when it started.
    pub fn gate_is_stale(&self, g: &Node) -> bool {
        g.kind.is_gate()
            && g.status.is_running()
            && g.depends_on.iter().collect::<BTreeSet<_>>() != g.dispatched_scope.iter().collect::<BTreeSet<_>>()
    }

    /// `agent` may stop `n` if it holds it or planned (expanded) one of its ancestors.
    pub fn in_subtree(&self, n: &Node, agent: AgentId) -> bool {
        if n.owner == Some(agent) || n.planner == Some(agent) {
            return true;
        }
        let mut cur = n.parent.clone();
        let mut hops = 0;
        while let Some(p) = cur {
            let Some(pn) = self.nodes.get(&p) else { break };
            if pn.planner == Some(agent) || pn.owner == Some(agent) {
                return true;
            }
            cur = pn.parent.clone();
            hops += 1;
            if hops > self.nodes.len() {
                break;
            }
        }
        false
    }

    fn require_holder(&self, id: &TaskId, by: AgentId) -> Result<&Node, TaskError> {
        let n = self.get(id)?;
        if !n.status.is_running() {
            return Err(TaskError::NotRunning { task: id.clone(), status: n.status });
        }
        if n.owner != Some(by) {
            return Err(TaskError::NotOwner { task: id.clone(), owner: n.owner, by });
        }
        Ok(n)
    }

    /// Thin/contract rules for a completion of `n`.
    pub fn check_artifact(&self, n: &Node, a: &HandoffArtifact) -> Result<(), TaskError> {
        if let Some(why) = a.thin_reason() {
            return Err(TaskError::ThinArtifact(why));
        }
        if a.contract.is_none() && self.settings.deep && n.kind.needs_contract() {
            return Err(TaskError::ContractRequired(n.id.clone()));
        }
        Ok(())
    }

    /// A passing gate must name every done node in scope, cannot pass over Low siblings, is stale
    /// if its scope grew after it started, and — a verify gate — cannot pass while one of the
    /// `invariants` covering its files is Fail or Stale.
    pub fn validate_gate(&self, gate: &TaskId, artifact: &HandoffArtifact, invariants: &[InvariantSeen]) -> Result<(), GateError> {
        let g = match self.nodes.get(gate) {
            Some(g) if g.kind.is_gate() => g,
            _ => return Err(GateError::NotAGate(gate.clone())),
        };
        let dispatched: BTreeSet<&TaskId> = g.dispatched_scope.iter().collect();
        let entered: Vec<TaskId> = g.depends_on.iter().filter(|d| !dispatched.contains(d)).cloned().collect();
        if !entered.is_empty() {
            return Err(GateError::StaleGateScope { entered: self.sorted(entered) });
        }
        let done: Vec<&Node> = g.depends_on.iter().filter_map(|d| self.nodes.get(d)).filter(|n| n.status == Status::Completed).collect();
        let covers: BTreeSet<&TaskId> = artifact.covers.iter().collect();
        let uncovered: Vec<TaskId> = done.iter().filter(|n| !covers.contains(&n.id)).map(|n| n.id.clone()).collect();
        if !uncovered.is_empty() {
            return Err(GateError::UncoveredSiblings(self.sorted(uncovered)));
        }
        let low: Vec<TaskId> = done
            .iter()
            .filter(|n| n.confidence() == Some(Confidence::Low))
            .filter(|n| !self.addressed(gate, &n.id))
            .map(|n| n.id.clone())
            .collect();
        if !low.is_empty() {
            return Err(GateError::UnaddressedLowConfidence(self.sorted(low)));
        }
        let failing: Vec<InvariantSeen> = invariants.iter().filter(|i| i.status.blocks_verify()).cloned().collect();
        if g.kind == Kind::Gate(GateKind::Verify) && !failing.is_empty() {
            return Err(GateError::InvariantNotPassing(failing));
        }
        Ok(())
    }

    /// A LOW sibling is addressed when work this gate injected for it completed at medium or better.
    fn addressed(&self, gate: &TaskId, sibling: &TaskId) -> bool {
        self.nodes.values().any(|n| {
            n.injected_by.as_ref() == Some(gate)
                && n.addresses.contains(sibling)
                && n.status == Status::Completed
                && n.confidence().map(|c| c >= Confidence::Medium).unwrap_or(false)
        })
    }

    /// Clone, apply, return the staged graph. Nothing is committed on error.
    pub fn stage(&self, events: &[Event]) -> Result<Graph, TaskError> {
        let mut g = self.clone();
        for e in events {
            g.apply(e)?;
        }
        Ok(g)
    }

    // ───────────────────────────── transitions ─────────────────────────────

    /// One validated transition. The fold (`fold_tasks`) and the engine share this.
    pub fn apply(&mut self, e: &Event) -> Result<(), TaskError> {
        match e {
            Event::Configured { settings, .. } => {
                settings.check()?;
                self.settings = settings.clone();
            }
            Event::Seeded { id, content, kind, depends_on, origin, parent, .. } => {
                let spec = NodeSpec {
                    id: id.clone(),
                    content: content.clone(),
                    kind: *kind,
                    depends_on: depends_on.clone(),
                    origin: *origin,
                    parent: parent.clone(),
                };
                self.apply_seeded(&spec)?;
            }
            Event::Rescoped { id, add, .. } => self.apply_rescoped(id, add)?,
            Event::Expanded { parent, children, joins, by } => self.apply_expanded(parent, children, joins, *by)?,
            Event::Started { id, by, scope } => self.apply_started(id, *by, scope)?,
            Event::Heartbeat { id, by, tool } => {
                self.require_holder(id, *by)?;
                let n = self.get_mut(id)?;
                n.status = Status::Running;
                if let Some(t) = tool {
                    n.push_recent(t.clone());
                }
            }
            Event::Checkpoint { id, by, summary, recent } => {
                self.require_holder(id, *by)?;
                if summary.trim().is_empty() {
                    return Err(TaskError::BadEvent("a checkpoint needs a summary of the progress so far".into()));
                }
                let n = self.get_mut(id)?;
                n.status = Status::Running;
                n.checkpoint = Some(summary.clone());
                for r in recent {
                    n.push_recent(r.clone());
                }
            }
            Event::Completed { id, by, artifact, invariants } => self.apply_completed(id, *by, artifact, invariants)?,
            Event::Injected { from_gate, new_node, addresses, by } => self.apply_injected(from_gate, new_node, addresses, *by)?,
            Event::Released { id, by } => {
                let g = self.require_holder(id, *by)?;
                if !self.gate_is_stale(g) {
                    return Err(TaskError::BadEvent(format!(
                        "`{id}` cannot be released: only a gate whose scope changed after it started goes back to the queue"
                    )));
                }
                let g = self.get_mut(id)?;
                release(g, Status::Queued);
            }
            Event::Requeued { id, by } => {
                let n = self.require_holder(id, *by)?;
                if n.no_artifact_requeues >= 1 {
                    return Err(TaskError::BadEvent(format!(
                        "`{id}` was already requeued once without an artifact; the next miss fails it"
                    )));
                }
                let n = self.get_mut(id)?;
                release(n, Status::Queued);
                n.no_artifact_requeues += 1;
            }
            Event::Failed { id, reason } => {
                let max = self.settings.max_reclaims;
                let n = self.get_mut(id)?;
                if !n.status.is_running() {
                    return Err(TaskError::NotRunning { task: id.clone(), status: n.status });
                }
                let earned = match reason {
                    FailReason::NoArtifact => n.no_artifact_requeues >= 1,
                    FailReason::ReclaimCap => n.reclaims >= max,
                };
                if !earned {
                    return Err(TaskError::BadEvent(format!("`{id}` has not reached the {} limit", reason.as_str())));
                }
                release(n, Status::Failed);
                n.failure = Some(*reason);
            }
            Event::Reclaimed { id, n: count, from } => {
                let max = self.settings.max_reclaims;
                let n = self.get_mut(id)?;
                if !n.status.is_running() {
                    return Err(TaskError::NotRunning { task: id.clone(), status: n.status });
                }
                if n.owner != Some(*from) {
                    return Err(TaskError::NotOwner { task: id.clone(), owner: n.owner, by: *from });
                }
                if *count != n.reclaims + 1 || *count > max {
                    return Err(TaskError::BadEvent(format!(
                        "reclaim #{count} of `{id}` is out of order or over the cap of {max} (seen {})",
                        n.reclaims
                    )));
                }
                release(n, Status::Queued);
                n.reclaims = *count;
            }
            Event::Control { id, op, by, to, salvage } => self.apply_control(id, *op, by, *to, salvage.as_ref())?,
            Event::Reopened { id, consumer, reason, gates, by } => self.apply_reopened(id, consumer, reason, gates, *by)?,
            Event::Stopped { id, by } => {
                let n = self.get(id)?;
                if n.status.is_terminal() {
                    return Err(TaskError::BadEvent(format!("`{id}` is already {}; there is nothing to stop", n.status)));
                }
                let allowed = match by {
                    Actor::Operator | Actor::Engineer => true,
                    Actor::Agent(a) => self.in_subtree(n, *a),
                    Actor::System(_) => false,
                };
                if !allowed {
                    return Err(TaskError::NotInSubtree { task: id.clone(), by: by.clone() });
                }
                let n = self.get_mut(id)?;
                release(n, Status::Stopped);
            }
            Event::Resumed {} => {
                for n in self.nodes.values_mut() {
                    if n.status == Status::Running {
                        n.status = Status::RunningStale;
                    }
                }
            }
        }
        Ok(())
    }

    fn insert(&mut self, spec: &NodeSpec) -> &mut Node {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.nodes.entry(spec.id.clone()).or_insert_with(|| Node::new(spec, seq))
    }

    fn check_spec_shape(&self, spec: &NodeSpec) -> Result<(), TaskError> {
        if spec.kind.is_gate() {
            if spec.depends_on.is_empty() {
                return Err(TaskError::BadEvent(format!("gate `{}` has an empty scope", spec.id)));
            }
        } else {
            spec.id.check_user()?;
            if spec.content.trim().is_empty() {
                return Err(TaskError::EmptyContent(spec.id.clone()));
            }
        }
        let mut seen = BTreeSet::new();
        for d in &spec.depends_on {
            if d == &spec.id || !seen.insert(d) {
                return Err(TaskError::BadEvent(format!("`{}` lists `{d}` as a dependency twice or on itself", spec.id)));
            }
        }
        Ok(())
    }

    fn require_open_parent(&self, parent: &TaskId) -> Result<&Node, TaskError> {
        let p = self.get(parent)?;
        if !p.expanded || p.status != Status::Queued {
            return Err(TaskError::ParentNotOpen { parent: parent.clone(), status: p.status, expanded: p.expanded });
        }
        Ok(p)
    }

    fn apply_seeded(&mut self, spec: &NodeSpec) -> Result<(), TaskError> {
        if let Some(n) = self.nodes.get(&spec.id) {
            // idempotent by (id, content)
            if n.content == spec.content {
                return Ok(());
            }
            return Err(TaskError::DuplicateNode(spec.id.clone()));
        }
        self.check_spec_shape(spec)?;
        match spec.origin {
            Origin::Seed => {
                if spec.parent.is_some() || spec.kind.is_gate() {
                    return Err(TaskError::BadEvent(format!(
                        "seeded `{}` must be top-level work; gates are inserted by the graph",
                        spec.id
                    )));
                }
            }
            Origin::Gap => {
                let Some(parent) = &spec.parent else {
                    return Err(TaskError::BadEvent(format!("gap task `{}` names no parent", spec.id)));
                };
                if spec.kind.is_gate() {
                    return Err(TaskError::InvalidKind(spec.kind.as_str().into()));
                }
                self.require_open_parent(parent)?;
            }
            Origin::Gate => match (spec.kind.gate(), &spec.parent) {
                (Some(GateKind::Root), None) if spec.id.as_str() == ROOT_GATE => {}
                (Some(gk @ (GateKind::Critique | GateKind::Verify)), Some(parent)) if spec.id == gate_id(parent, gk) => {
                    self.require_open_parent(parent)?;
                }
                _ => return Err(TaskError::BadEvent(format!("`{}` is not a well-formed gate", spec.id))),
            },
            Origin::Expand => {
                return Err(TaskError::BadEvent(format!("`{}`: expansion children arrive in Task/Expanded", spec.id)));
            }
        }
        for d in &spec.depends_on {
            if !self.nodes.contains_key(d) {
                return Err(TaskError::UnknownDependency { task: spec.id.clone(), dep: d.clone() });
            }
        }
        self.insert(spec);
        Ok(())
    }

    fn apply_rescoped(&mut self, id: &TaskId, add: &[TaskId]) -> Result<(), TaskError> {
        let n = self.get(id)?;
        if add.is_empty() {
            return Err(TaskError::BadEvent(format!("rescoping `{id}` adds nothing")));
        }
        let gate = n.kind.is_gate();
        if !gate && !(n.expanded && n.status == Status::Queued) {
            return Err(TaskError::ParentNotOpen { parent: id.clone(), status: n.status, expanded: n.expanded });
        }
        for a in add {
            if !self.nodes.contains_key(a) {
                return Err(TaskError::UnknownDependency { task: id.clone(), dep: a.clone() });
            }
            if a == id || n.depends_on.contains(a) {
                return Err(TaskError::BadEvent(format!("`{a}` is already in the scope of `{id}`")));
            }
            if self.reaches(a, id) {
                return Err(TaskError::Cycle(vec![id.clone(), a.clone()]));
            }
        }
        let reopen = gate && n.status == Status::Completed;
        if reopen {
            for d in self.dependents(id) {
                if let Some(dn) = self.nodes.get(&d)
                    && dn.status != Status::Queued
                {
                    return Err(TaskError::GateFedCompleted { gate: id.clone(), dependent: d });
                }
            }
        }
        let n = self.get_mut(id)?;
        n.depends_on.extend(add.iter().cloned());
        if reopen {
            n.status = Status::Queued;
            n.artifact = None;
            n.reopened += 1;
            n.dispatched_scope.clear();
        }
        Ok(())
    }

    fn apply_expanded(&mut self, parent: &TaskId, children: &[NodeSpec], joins: &[TaskId], by: AgentId) -> Result<(), TaskError> {
        let p = self.get(parent)?;
        if p.kind.is_gate() {
            return Err(TaskError::GateCannotExpand(parent.clone()));
        }
        if p.expanded {
            return Err(TaskError::AlreadyExpanded(parent.clone()));
        }
        if p.status != Status::Running {
            return Err(TaskError::NotRunning { task: parent.clone(), status: p.status });
        }
        if p.owner != Some(by) {
            return Err(TaskError::NotOwner { task: parent.clone(), owner: p.owner, by });
        }
        let work: Vec<&NodeSpec> = children.iter().filter(|c| !c.kind.is_gate()).collect();
        let gates: Vec<&NodeSpec> = children.iter().filter(|c| c.kind.is_gate()).collect();
        if work.is_empty() {
            return Err(TaskError::EmptyExpansion(parent.clone()));
        }

        // staged: insert everything, then check deps, gate coverage, acyclicity
        let mut staged = self.clone();
        for c in children {
            if c.parent.as_ref() != Some(parent) {
                return Err(TaskError::BadEvent(format!("child `{}` does not name `{parent}` as its parent", c.id)));
            }
            if staged.nodes.contains_key(&c.id) {
                return Err(TaskError::DuplicateNode(c.id.clone()));
            }
            staged.check_spec_shape(c)?;
            match c.kind.gate() {
                Some(gk @ (GateKind::Critique | GateKind::Verify)) if c.origin == Origin::Gate && c.id == gate_id(parent, gk) => {}
                Some(_) => return Err(TaskError::BadEvent(format!("`{}` is not a well-formed gate for `{parent}`", c.id))),
                None if c.origin == Origin::Expand => {}
                None => return Err(TaskError::BadEvent(format!("child `{}` must have origin expand", c.id))),
            }
            staged.insert(c);
        }
        for c in children {
            for d in &c.depends_on {
                if d == parent {
                    return Err(TaskError::ChildDependsOnParent { child: c.id.clone(), parent: parent.clone() });
                }
                if !staged.nodes.contains_key(d) {
                    return Err(TaskError::UnknownDependency { task: c.id.clone(), dep: d.clone() });
                }
            }
        }
        let gate_ids: BTreeSet<&TaskId> = gates.iter().map(|g| &g.id).collect();
        if gate_ids != joins.iter().collect::<BTreeSet<_>>() {
            return Err(TaskError::BadEvent(format!("the synthesis join of `{parent}` must wait on exactly its gates")));
        }
        let work_kind: BTreeMap<&TaskId, Kind> = work.iter().map(|w| (&w.id, w.kind)).collect();
        for g in &gates {
            for d in &g.depends_on {
                match (work_kind.get(d), g.kind.gate()) {
                    (Some(k), Some(gk)) if k.gate_for() == gk => {}
                    _ => return Err(TaskError::BadEvent(format!("gate `{}` may only cover `{parent}`'s children of its kind", g.id))),
                }
            }
        }
        for w in &work {
            let covering = gates.iter().filter(|g| g.depends_on.contains(&w.id)).count();
            if covering != 1 {
                return Err(TaskError::UngatedChild(w.id.clone()));
            }
        }
        let p = staged.get_mut(parent)?;
        p.expanded = true;
        p.planner = Some(by);
        p.depends_on.extend(joins.iter().cloned());
        release(p, Status::Queued);
        staged.check_acyclic()?;
        *self = staged;
        Ok(())
    }

    fn apply_started(&mut self, id: &TaskId, by: AgentId, scope: &[TaskId]) -> Result<(), TaskError> {
        let n = self.get(id)?;
        if n.status != Status::Queued {
            return Err(TaskError::NotQueued { task: id.clone(), status: n.status });
        }
        let open = self.open_deps(n);
        if !open.is_empty() {
            return Err(TaskError::Blocked { task: id.clone(), on: open });
        }
        if let Some(p) = n.pinned
            && p != by
        {
            return Err(TaskError::PinnedTo { task: id.clone(), agent: p });
        }
        if let Some(h) = self.held_by(by) {
            return Err(TaskError::AlreadyHolding { agent: by, task: h.id.clone() });
        }
        let live = self.live_workers();
        if !live.contains(&by) && live.len() as u64 >= self.settings.max_live_workers as u64 {
            return Err(TaskError::AtCapacity(format!(
                "{} workers are already running, the live-worker budget (swarm.max_live_workers)",
                live.len()
            )));
        }
        if !self.members.contains(&by) && self.members.len() as u64 >= self.settings.max_members as u64 {
            return Err(TaskError::AtCapacity(format!(
                "the swarm already has {} members, its hard cap (swarm.max_members), and {} is not one of them",
                self.members.len(),
                agent_label(by)
            )));
        }
        let expected: BTreeSet<&TaskId> = if n.kind.is_gate() { n.depends_on.iter().collect() } else { BTreeSet::new() };
        if scope.iter().collect::<BTreeSet<_>>() != expected {
            return Err(TaskError::BadEvent(format!("the scope recorded for `{id}` does not match its dependencies")));
        }
        let n = self.get_mut(id)?;
        n.status = Status::Running;
        n.owner = Some(by);
        n.pinned = None;
        n.dispatched_scope = scope.to_vec();
        self.members.insert(by);
        Ok(())
    }

    fn apply_completed(
        &mut self,
        id: &TaskId,
        by: AgentId,
        artifact: &HandoffArtifact,
        invariants: &[InvariantSeen],
    ) -> Result<(), TaskError> {
        let n = self.require_holder(id, by)?;
        self.check_artifact(n, artifact)?;
        if !invariants.is_empty() && n.kind != Kind::Gate(GateKind::Verify) {
            return Err(TaskError::BadEvent(format!("`{id}` is not a verify gate; only a verify gate's pass names invariants")));
        }
        // a gate's verdict is checked first: work that entered its scope after dispatch is
        // `StaleGateScope`, not merely an open dependency
        if n.kind.is_gate() {
            self.validate_gate(id, artifact, invariants).map_err(|err| TaskError::Gate { gate: id.clone(), err })?;
        }
        let open = self.open_deps(n);
        if !open.is_empty() {
            return Err(TaskError::Blocked { task: id.clone(), on: open });
        }
        let n = self.get_mut(id)?;
        release(n, Status::Completed);
        n.artifact = Some(artifact.clone());
        n.feedback = None;
        n.salvage = None;
        Ok(())
    }

    fn apply_injected(&mut self, gate: &TaskId, spec: &NodeSpec, addresses: &[TaskId], by: AgentId) -> Result<(), TaskError> {
        let g = self.require_holder(gate, by)?;
        if !g.kind.is_gate() {
            return Err(TaskError::NotAGate(gate.clone()));
        }
        if spec.origin != Origin::Gate || spec.kind.is_gate() || spec.parent != g.parent {
            return Err(TaskError::BadEvent(format!("injected `{}` must be gate-origin work under the gate's parent", spec.id)));
        }
        if self.nodes.contains_key(&spec.id) {
            return Err(TaskError::DuplicateNode(spec.id.clone()));
        }
        self.check_spec_shape(spec)?;
        for d in &spec.depends_on {
            if !self.nodes.contains_key(d) {
                return Err(TaskError::UnknownDependency { task: spec.id.clone(), dep: d.clone() });
            }
            if d == gate || self.reaches(d, gate) {
                return Err(TaskError::Cycle(vec![gate.clone(), spec.id.clone(), d.clone()]));
            }
        }
        if let Some(a) = addresses.iter().find(|a| !g.depends_on.contains(a)) {
            return Err(TaskError::BadEvent(format!("`{a}` is not in the scope of gate `{gate}`, so injected work cannot address it")));
        }
        let n = self.insert(spec);
        n.injected_by = Some(gate.clone());
        n.addresses = addresses.to_vec();
        let g = self.get_mut(gate)?;
        g.depends_on.push(spec.id.clone());
        Ok(())
    }

    fn apply_control(
        &mut self,
        id: &TaskId,
        op: Control,
        by: &Actor,
        to: Option<AgentId>,
        salvage: Option<&Salvage>,
    ) -> Result<(), TaskError> {
        if *by != Actor::Operator {
            return Err(TaskError::OperatorOnly(op));
        }
        let n = self.get(id)?;
        if !op.safe_from().contains(&n.status) {
            return Err(TaskError::UnsafeControl { task: id.clone(), op, status: n.status });
        }
        match op {
            Control::Reassign if to.is_none() => {
                return Err(TaskError::BadEvent("reassign needs the agent to hand the task to (`to`)".into()));
            }
            Control::Salvage => {
                if salvage.map(Salvage::is_empty).unwrap_or(true) {
                    return Err(TaskError::NothingToSalvage(id.clone()));
                }
            }
            _ if salvage.is_some() => {
                return Err(TaskError::BadEvent(format!("{} does not carry a salvage snapshot", op.as_str())));
            }
            _ => {}
        }
        let n = self.get_mut(id)?;
        release(n, Status::Queued);
        n.pinned = to;
        n.failure = None;
        n.reclaims = 0;
        n.no_artifact_requeues = 0;
        match op {
            Control::Retry => n.artifact = None,
            Control::Reassign => {}
            Control::Replace => {
                n.checkpoint = None;
                n.recent.clear();
                n.salvage = None;
            }
            Control::Salvage => n.salvage = salvage.cloned(),
        }
        Ok(())
    }

    fn apply_reopened(
        &mut self,
        producer: &TaskId,
        consumer: &TaskId,
        reason: &str,
        gates: &[TaskId],
        by: AgentId,
    ) -> Result<(), TaskError> {
        let c = self.require_holder(consumer, by)?;
        if c.kind.is_gate() {
            return Err(TaskError::BadEvent(format!("gate `{consumer}` turns misses into work with `inject`, not by re-opening")));
        }
        if !self.inputs(consumer).contains(producer) {
            return Err(TaskError::NotAnInput { consumer: consumer.clone(), producer: producer.clone() });
        }
        if reason.trim().is_empty() {
            return Err(TaskError::BadEvent("re-opening a producer needs the failed acceptance check as the reason".into()));
        }
        let p = self.get(producer)?;
        if p.kind.is_gate() || p.status != Status::Completed {
            return Err(TaskError::BadEvent(format!("`{producer}` is {} {}; only completed work re-opens", p.status, p.kind)));
        }
        let expected: Vec<TaskId> = self
            .nodes()
            .into_iter()
            .filter(|g| g.kind.is_gate() && g.depends_on.contains(producer))
            .filter(|g| g.status == Status::Completed || g.status.is_running())
            .map(|g| g.id.clone())
            .collect();
        if gates.iter().collect::<BTreeSet<_>>() != expected.iter().collect::<BTreeSet<_>>() {
            return Err(TaskError::BadEvent(format!("re-opening `{producer}` must also re-open exactly the gates over it: {expected:?}")));
        }
        for g in &expected {
            for d in self.dependents(g) {
                let ok = &d == consumer || self.nodes.get(&d).map(|x| x.status == Status::Queued).unwrap_or(true);
                if !ok {
                    return Err(TaskError::GateFedCompleted { gate: g.clone(), dependent: d });
                }
            }
        }
        for g in &expected {
            let gn = self.get_mut(g)?;
            release(gn, Status::Queued);
            gn.artifact = None;
            gn.reopened += 1;
        }
        let pn = self.get_mut(producer)?;
        pn.status = Status::Queued;
        pn.artifact = None;
        pn.reopened += 1;
        pn.pinned = None;
        pn.feedback = Some(format!("`{consumer}` re-opened this task: {}", reason.trim()));
        let cn = self.get_mut(consumer)?;
        release(cn, Status::Queued);
        Ok(())
    }
}

/// Clears the holder (remembering it) and the dispatch snapshot, then sets `status`.
fn release(n: &mut Node, status: Status) {
    if let Some(o) = n.owner.take() {
        n.last_owner = Some(o);
    }
    n.dispatched_scope.clear();
    n.status = status;
}
