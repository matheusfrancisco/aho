//! Typed refusals. Every message says what happened, what the graph did (nothing — refusals never
//! half-apply), and how to change it (12, message voice).

use std::fmt;

use aho_types::AgentId;

use crate::invariant::{InvariantSeen, InvariantStatus};
use crate::model::{Actor, Control, Status, TaskId, agent_label};

/// Why a gate verdict was refused. A passing gate names every done task in scope, never passes
/// over a LOW-confidence sibling, is stale when its scope grew after it started, and — a verify
/// gate — never passes while an invariant covering its files is Fail or Stale (docs 09, 13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateError {
    UncoveredSiblings(Vec<TaskId>),
    UnaddressedLowConfidence(Vec<TaskId>),
    StaleGateScope { entered: Vec<TaskId> },
    NotAGate(TaskId),
    InvariantNotPassing(Vec<InvariantSeen>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskError {
    UnknownTask(TaskId),
    DuplicateNode(TaskId),
    InvalidId { id: String, why: &'static str },
    InvalidKind(String),
    EmptyContent(TaskId),
    UnknownDependency { task: TaskId, dep: TaskId },
    Cycle(Vec<TaskId>),
    NotOwner { task: TaskId, owner: Option<AgentId>, by: AgentId },
    NotRunning { task: TaskId, status: Status },
    NotQueued { task: TaskId, status: Status },
    Blocked { task: TaskId, on: Vec<TaskId> },
    AlreadyExpanded(TaskId),
    GateCannotExpand(TaskId),
    EmptyExpansion(TaskId),
    ChildDependsOnParent { child: TaskId, parent: TaskId },
    UngatedChild(TaskId),
    ParentNotOpen { parent: TaskId, status: Status, expanded: bool },
    PinnedTo { task: TaskId, agent: AgentId },
    AlreadyHolding { agent: AgentId, task: TaskId },
    AtCapacity(String),
    ThinArtifact(&'static str),
    BadArtifact(String),
    ContractRequired(TaskId),
    Gate { gate: TaskId, err: GateError },
    NotAGate(TaskId),
    UnsafeControl { task: TaskId, op: Control, status: Status },
    NothingToSalvage(TaskId),
    OperatorOnly(Control),
    NotAnInput { consumer: TaskId, producer: TaskId },
    GateFedCompleted { gate: TaskId, dependent: TaskId },
    NotInSubtree { task: TaskId, by: Actor },
    BadEvent(String),
}

fn list(ids: &[TaskId]) -> String {
    ids.iter().map(|i| format!("`{i}`")).collect::<Vec<_>>().join(", ")
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::UncoveredSiblings(ids) => write!(
                f,
                "the verdict does not name {} in `covers`; the gate stays running. Review each one and list every task in scope",
                list(ids)
            ),
            GateError::UnaddressedLowConfidence(ids) => write!(
                f,
                "{} came back LOW confidence and nothing addressed it; the gate stays running. Inject follow-up work that addresses it \
                 (task_complete with `inject`, naming it in `addresses`), then review again once that work completes",
                list(ids)
            ),
            GateError::StaleGateScope { entered } => write!(
                f,
                "{} entered the gate's scope after the review started; the verdict was not recorded. Call task_claim to restart the \
                 review over the widened scope",
                list(entered)
            ),
            GateError::NotAGate(id) => write!(f, "`{id}` is not a gate"),
            GateError::InvariantNotPassing(invs) => {
                let each: Vec<String> = invs
                    .iter()
                    .map(|i| match i.status {
                        InvariantStatus::Stale => format!("`{}` is Stale (its last check is of older code)", i.id),
                        InvariantStatus::Fail => format!("`{}` fails (`aho invariants show {}` renders the counterexample)", i.id, i.id),
                        other => format!("`{}` is {}", i.id, other.as_str()),
                    })
                    .collect();
                write!(
                    f,
                    "the verify gate cannot pass while an invariant covering its files is not passing: {}; the gate stays running. Fix \
                     what a failing check found, re-run each check against the current code (spec_check, spec_trace or lean_build with \
                     `invariant: <id>`), then complete the gate again",
                    each.join(", ")
                )
            }
        }
    }
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TaskError::*;
        match self {
            UnknownTask(id) => write!(f, "there is no task `{id}`; nothing was changed. task_status lists the graph"),
            DuplicateNode(id) => write!(
                f,
                "task `{id}` already exists with different content; nothing was changed. Pick a new id, or resend the exact same \
                 content to make the seed a no-op"
            ),
            InvalidId { id, why } => write!(f, "task id `{id}` is not allowed: {why}; nothing was changed. Choose another id"),
            InvalidKind(k) => write!(
                f,
                "`{k}` is not a work kind; nothing was changed. Use explore, code or synthesize (gates are inserted by the graph)"
            ),
            EmptyContent(id) => write!(f, "task `{id}` has no description; nothing was changed. Say what the worker should do"),
            UnknownDependency { task, dep } => write!(
                f,
                "task `{task}` depends on `{dep}`, which does not exist; nothing was changed. Seed `{dep}` first or drop the dependency"
            ),
            Cycle(ids) => write!(
                f,
                "these tasks would wait on each other in a loop: {}; nothing was changed. Remove one of the dependencies",
                list(ids)
            ),
            NotOwner { task, owner, by } => write!(
                f,
                "task `{task}` is held by {}, not {}; nothing was changed. Only the agent working a task may expand, checkpoint or \
                 complete it",
                owner.map(agent_label).unwrap_or_else(|| "nobody".into()),
                agent_label(*by)
            ),
            NotRunning { task, status } => {
                write!(f, "task `{task}` is {status}, not running; nothing was changed. Claim it with task_claim first")
            }
            NotQueued { task, status } => write!(f, "task `{task}` is {status}, so it cannot start; nothing was changed"),
            Blocked { task, on } => {
                write!(f, "task `{task}` is still waiting on {}; nothing was changed. It becomes ready when those complete", list(on))
            }
            AlreadyExpanded(id) => write!(
                f,
                "task `{id}` was already expanded once; nothing was changed. Add missing work with task_seed and `parent: \"{id}\"`"
            ),
            GateCannotExpand(id) => {
                write!(f, "`{id}` is a gate and cannot be expanded; nothing was changed. A gate turns misses into work with `inject`")
            }
            EmptyExpansion(id) => write!(f, "expanding `{id}` needs at least one child task; nothing was changed"),
            ChildDependsOnParent { child, parent } => write!(
                f,
                "child `{child}` depends on its own parent `{parent}`, which waits for its children; nothing was changed. Drop that \
                 dependency"
            ),
            UngatedChild(id) => write!(f, "child `{id}` is not covered by exactly one gate of the right kind; nothing was changed"),
            ParentNotOpen { parent, status, expanded } => {
                if *expanded {
                    write!(
                        f,
                        "task `{parent}` is {status}, so its scope can no longer grow; nothing was changed. Seed the missing work as a \
                         new top-level task instead"
                    )
                } else {
                    write!(
                        f,
                        "task `{parent}` has not been expanded, so it has no children to add to; nothing was changed. Seed a top-level \
                         task, or let its worker expand it"
                    )
                }
            }
            PinnedTo { task, agent } => {
                write!(f, "task `{task}` was reassigned to {} by the operator; nothing was changed", agent_label(*agent))
            }
            AlreadyHolding { agent, task } => write!(
                f,
                "{} already holds `{task}`; nothing was changed. Finish it with task_complete before claiming more",
                agent_label(*agent)
            ),
            AtCapacity(why) => write!(f, "{why}; nothing was started. Wait for a running worker to finish, or raise the cap"),
            ThinArtifact(field) => {
                let fix = match *field {
                    "findings" => "Say what you found in `findings`",
                    "what_i_did_not_check" => {
                        "List what you did not verify in `what_i_did_not_check` — every hand-off has blind spots; name them"
                    }
                    "confidence" => "Set `confidence` to low, medium or high",
                    "contract.outputs" => "Name every output in `contract.outputs` with its path and format",
                    "contract.acceptance" => "Give the dependent at least one acceptance check in `contract.acceptance`",
                    _ => "Fill it in",
                };
                write!(f, "the hand-off artifact is too thin (`{field}`); the task stays running. {fix}, then complete again")
            }
            BadArtifact(why) => write!(f, "the hand-off artifact could not be read: {why}; the task stays running"),
            ContractRequired(id) => write!(
                f,
                "task `{id}` hands over code in deep mode, so its artifact needs a `contract` (outputs with path and format, units, \
                 version, acceptance checks); the task stays running. Add the contract and complete again"
            ),
            Gate { gate, err } => write!(f, "gate `{gate}`: {err}"),
            NotAGate(id) => write!(f, "`{id}` is not a gate; only gates inject work. Use task_seed with `parent` to add work"),
            UnsafeControl { task, op, status } => {
                let allowed = op.safe_from().iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                let hint = if *status == Status::Running {
                    " Its worker is live: stop it first (task_control op=stop) or wait for it to go stale"
                } else if *status == Status::Completed {
                    " A completed task re-opens only when a dependent's acceptance check fails (task_control op=reopen)"
                } else {
                    ""
                };
                write!(f, "task `{task}` is {status}; {} applies only to {allowed}. Nothing was changed.{hint}", op.as_str())
            }
            NothingToSalvage(id) => {
                write!(f, "task `{id}` has no checkpoint or tool summaries to salvage; nothing was changed. Use replace for a fresh worker")
            }
            OperatorOnly(op) => write!(f, "{} is an operator decision; nothing was changed. It needs the operator's approval", op.as_str()),
            NotAnInput { consumer, producer } => write!(
                f,
                "`{producer}` is not an input of `{consumer}`; nothing was changed. Only a task that consumed an artifact may re-open it"
            ),
            GateFedCompleted { gate, dependent } => write!(
                f,
                "gate `{gate}` already fed `{dependent}`, which has started; nothing was changed. Seed follow-up work as a new task"
            ),
            NotInSubtree { task, by } => write!(
                f,
                "{by} may stop only tasks in its own subtree, and `{task}` is not one; nothing was changed. Ask the operator to stop it"
            ),
            BadEvent(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for TaskError {}
