//! Deterministic simulator: scripted worker closures drive seed/expand/complete/inject
//! through the real engine without a model. Every event goes through the same JSON body and
//! `fold_tasks` path the session log uses, so a run is also a replay test.

use aho_types::{AgentId, Value};

use crate::engine::{ChildSpec, Dispatch, Injection, SeedSpec};
use crate::error::TaskError;
use crate::event::Event;
use crate::fold_tasks;
use crate::graph::Graph;
use crate::invariant::InvariantSeen;
use crate::model::{Actor, GateKind, HandoffArtifact, Kind, Node, Settings, TaskId};
use crate::render::Hydrate;

/// The view a scripted worker gets for one turn.
pub struct Turn<'a> {
    pub graph: &'a Graph,
    pub node: &'a Node,
    /// The hydrated input (`assemble_input`, by reference).
    pub input: String,
    /// 1 for the first turn this worker spends on this node.
    pub attempt: u32,
}

/// One scripted move. A refused move (thin artifact, uncovered gate, …) ends the worker's turn
/// without an artifact, so artifact-or-nothing applies.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // simulator scripts are tiny; boxing would only obscure them
pub enum Action {
    Complete(HandoffArtifact),
    Expand(Vec<ChildSpec>),
    Inject(Vec<Injection>),
    Reopen {
        producer: TaskId,
        reason: String,
    },
    /// Progress without finishing; the worker keeps the node and acts again next pass.
    Checkpoint {
        summary: String,
        recent: Vec<String>,
    },
    /// The turn ends with the node still running.
    EndTurn,
    /// The worker process dies holding the node.
    Die,
}

pub type Script = Box<dyn FnMut(&Turn<'_>) -> Action>;

struct SimWorker {
    id: AgentId,
    script: Script,
    dead: bool,
    attempts: Vec<(TaskId, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing is queued or running.
    Finished {
        passes: usize,
    },
    /// A full pass changed nothing and work remains (blocked on failed/stopped work, or no live worker).
    Stuck {
        passes: usize,
    },
    PassLimit,
}

pub struct Sim {
    graph: Graph,
    log: Vec<(String, Value)>,
    workers: Vec<SimWorker>,
    rejected: Vec<(AgentId, TaskId, TaskError)>,
    invariants: Vec<InvariantSeen>,
}

impl Sim {
    pub fn new(settings: Settings) -> Result<Sim, TaskError> {
        let mut s = Sim { graph: Graph::default(), log: vec![], workers: vec![], rejected: vec![], invariants: vec![] };
        let ev = s.graph.configure(&Actor::System("sim".into()), &settings)?;
        s.commit(ev)?;
        Ok(s)
    }

    /// Sets the invariants each verify gate completed from now on reports as covering its files.
    pub fn set_invariants(&mut self, seen: Vec<InvariantSeen>) {
        self.invariants = seen;
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }
    /// `(name, body)` of every `Task/*` event, in order.
    pub fn log(&self) -> &[(String, Value)] {
        &self.log
    }
    pub fn names(&self) -> Vec<&str> {
        self.log.iter().map(|(n, _)| n.as_str()).collect()
    }
    /// Moves the engine refused: `(worker, node, why)`.
    pub fn rejected(&self) -> &[(AgentId, TaskId, TaskError)] {
        &self.rejected
    }

    pub fn add_worker(&mut self, id: AgentId, script: Script) {
        self.workers.push(SimWorker { id, script, dead: false, attempts: vec![] });
    }

    pub fn seed(&mut self, by: Actor, specs: Vec<SeedSpec>) -> Result<(), TaskError> {
        let ev = self.graph.seed(&by, specs)?;
        self.commit(ev)
    }

    /// Operator-side moves (control, stop) go through here.
    pub fn apply(&mut self, events: Vec<Event>) -> Result<(), TaskError> {
        self.commit(events)
    }

    fn commit(&mut self, events: Vec<Event>) -> Result<(), TaskError> {
        for e in events {
            let body = e.body()?;
            // round-trip through the log representation before applying
            let decoded = Event::parse(e.name(), &body)?;
            self.graph.apply(&decoded)?;
            self.log.push((e.name().to_string(), body));
        }
        Ok(())
    }

    /// Re-folds the recorded log through `fold_tasks` from nothing.
    pub fn replay(&self) -> Result<Graph, String> {
        let mut state: Option<Value> = None;
        for (name, body) in &self.log {
            state = Some(fold_tasks(state.as_ref(), name, body)?);
        }
        crate::project(state.as_ref())
    }

    /// One round-robin pass over live workers. Returns whether anything changed.
    pub fn step(&mut self) -> Result<bool, TaskError> {
        let mut progressed = false;
        for i in 0..self.workers.len() {
            if self.workers[i].dead {
                continue;
            }
            let wid = self.workers[i].id;
            let id = match self.graph.dispatch(wid)? {
                Dispatch::Started { id, events } => {
                    self.commit(events)?;
                    id
                }
                Dispatch::Holding(id) => id,
                Dispatch::Released { events, .. } => {
                    self.commit(events)?;
                    progressed = true;
                    continue;
                }
                Dispatch::Idle { .. } | Dispatch::AtCapacity(_) | Dispatch::Finished => continue,
            };
            let node = self.graph.get(&id)?.clone();
            let input = self.graph.assemble_input(&id, Hydrate::Reference)?;
            let attempt = {
                let w = &mut self.workers[i];
                match w.attempts.iter_mut().find(|(t, _)| *t == id) {
                    Some((_, n)) => {
                        *n += 1;
                        *n
                    }
                    None => {
                        w.attempts.push((id.clone(), 1));
                        1
                    }
                }
            };
            let action = {
                let Sim { graph, workers, .. } = &mut *self;
                (workers[i].script)(&Turn { graph, node: &node, input, attempt })
            };
            progressed = true;
            let result = match action {
                Action::Complete(a) => {
                    let seen = if node.kind == Kind::Gate(GateKind::Verify) { self.invariants.clone() } else { vec![] };
                    self.graph.complete(wid, &id, a, seen)
                }
                Action::Expand(ch) => self.graph.expand(wid, &id, ch),
                Action::Inject(work) => self.graph.inject_from_gate(wid, &id, work),
                Action::Reopen { producer, reason } => self.graph.reopen(wid, &id, &producer, &reason),
                Action::Checkpoint { summary, recent } => self.graph.checkpoint(wid, &id, &summary, recent),
                Action::EndTurn => Ok(vec![]),
                Action::Die => {
                    self.workers[i].dead = true;
                    let ev = self.graph.reclaim_dead(wid)?;
                    self.commit(ev)?;
                    continue;
                }
            };
            let keeps_node = matches!(&result, Ok(ev) if ev.iter().any(|e| matches!(e, Event::Checkpoint { .. })));
            match result {
                Ok(ev) => self.commit(ev)?,
                Err(e) => self.rejected.push((wid, id.clone(), e)),
            }
            if !keeps_node {
                // the turn is over: anything still running without an artifact is requeued once, then failed
                let ev = self.graph.turn_ended(wid)?;
                self.commit(ev)?;
            }
        }
        Ok(progressed)
    }

    pub fn run(&mut self, max_passes: usize) -> Result<Outcome, TaskError> {
        for pass in 1..=max_passes {
            if self.graph.nodes().iter().all(|n| n.status.is_terminal()) {
                return Ok(Outcome::Finished { passes: pass - 1 });
            }
            if !self.step()? {
                return Ok(Outcome::Stuck { passes: pass });
            }
        }
        if self.graph.nodes().iter().all(|n| n.status.is_terminal()) {
            return Ok(Outcome::Finished { passes: max_passes });
        }
        Ok(Outcome::PassLimit)
    }
}
