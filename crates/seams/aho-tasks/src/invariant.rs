//! A Verify gate cannot pass while a covering invariant is `Fail` or `Stale`.
//!
//! The graph does not read invariants. Whoever completes a verify gate (tool-tasks) decides which
//! registered invariants cover the files the gate's scope handed over ([`Graph::gate_files`]) and
//! what each one's status is at that moment (`aho_inspect::invariants`: the latest
//! `Invariant/Checked` per id, `Stale` once the code moved past it), and the `Task/Completed` event
//! carries that list. The fold refuses a pass that names a failing or stale one, on append and on
//! replay; `formal/Tasks.tla` checks each named status against the log's own checks.

use serde::{Deserialize, Serialize};

use crate::graph::Graph;
use crate::model::TaskId;

/// An invariant's status as the operator sees it now (`aho invariants`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantStatus {
    Pass,
    Fail,
    Unknown,
    Stale,
    Vacuous,
}

impl InvariantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InvariantStatus::Pass => "pass",
            InvariantStatus::Fail => "fail",
            InvariantStatus::Unknown => "unknown",
            InvariantStatus::Stale => "stale",
            InvariantStatus::Vacuous => "vacuous",
        }
    }

    /// `Fail` and `Stale` keep a verify gate from passing.
    pub fn blocks_verify(self) -> bool {
        matches!(self, InvariantStatus::Fail | InvariantStatus::Stale)
    }
}

/// One invariant covering a verify gate, with the status it had when the gate completed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantSeen {
    pub id: String,
    pub status: InvariantStatus,
}

impl Graph {
    /// The files a gate's scope handed over: every `contract.outputs[].path` of the completed tasks
    /// it reviews, sorted, without duplicates. Empty when none declared a contract.
    pub fn gate_files(&self, gate: &TaskId) -> Vec<String> {
        let Some(g) = self.node(gate) else { return vec![] };
        let mut files: Vec<String> = g
            .depends_on
            .iter()
            .filter_map(|d| self.node(d))
            .filter_map(|n| n.artifact.as_ref())
            .filter_map(|a| a.contract.as_ref())
            .flat_map(|c| c.outputs.iter().map(|o| o.path.clone()))
            .collect();
        files.sort();
        files.dedup();
        files
    }
}
