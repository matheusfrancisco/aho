//! Every operator screen is a projection. Nothing here inspects a live struct: each
//! function takes a `Plan`, a folded `State`, a `Loadout`, or log entries, and returns data that
//! CLI, TUI, web, and the agent's `aho_why` tool render identically (all rows are `Serialize`).

#![forbid(unsafe_code)]

mod catalog;
mod explain;
mod query;
mod session;
mod tree;

use aho_log::CapState;
use aho_types::*;

pub use catalog::catalog_config;
pub use explain::{ConfigSource, Explanation, Finding, HistoryRow, Shadowed, explain, render_explain};
pub use query::{parse_session_query, render_session_query};
pub use session::{
    CatchUp, InvariantRow, LeaseRow, OpenCall, Orphan, OrphanKind, OrphanScan, SessionStatus, Status, catch_up, current_invariants,
    invariants, orphans, render_catch_up, render_invariants, render_orphans, render_status, session_status, status,
};
pub use tree::{Card, Missing, Setting, TreeNode, Why, govern, inventory, inventory_in, render_inventory, render_tree, tree, why, why_in};

/// `●` Active `○` Unlocked `◌` Locked `✕` Broken `⊘` Disabled, the same in every surface.
pub fn glyph(s: &CapState) -> char {
    match s {
        CapState::Active => '●',
        CapState::Unlocked => '○',
        CapState::Locked { .. } => '◌',
        CapState::Broken { .. } => '✕',
        CapState::Disabled { .. } => '⊘',
    }
}

pub fn state_name(s: &CapState) -> &'static str {
    match s {
        CapState::Active => "Active",
        CapState::Unlocked => "Unlocked",
        CapState::Locked { .. } => "Locked",
        CapState::Broken { .. } => "Broken",
        CapState::Disabled { .. } => "Disabled",
    }
}

/// `ReadOnly · WorkspaceWrite`.
pub fn render_classes(c: ConsequenceSet) -> String {
    c.iter().map(|x| x.name()).collect::<Vec<_>>().join(" · ")
}

/// The smallest named preset admitting `over` (mirrors the plan's own suggestion).
pub(crate) fn suggest_preset(over: ConsequenceSet) -> &'static str {
    if over.contains(Consequence::ControlsVM) || over.contains(Consequence::ControlsDesktop) || over.contains(Consequence::HostWrite) {
        "lab"
    } else if over.contains(Consequence::NetworkEgress) {
        "dev"
    } else {
        "workspace"
    }
}

/// The damage a class can do, for "loudest first" orderings.
pub(crate) fn blast(c: Consequence) -> u8 {
    match c {
        Consequence::ReadOnly => 0,
        Consequence::TalksToHuman => 1,
        Consequence::WorkspaceWrite => 2,
        Consequence::NetworkEgress | Consequence::SpawnsProcess => 3,
        Consequence::ControlsDesktop | Consequence::ControlsVM => 4,
        Consequence::HostWrite => 5,
    }
}

/// Values the resolver redacted render as `secret (digest abc1234)`, never as JSON noise.
pub(crate) fn show_value(v: &Value) -> String {
    match v {
        Value::Object(m) if m.len() == 1 => match m.get("digest").and_then(Value::as_str) {
            Some(d) => format!("secret (digest {})", d.chars().take(7).collect::<String>()),
            None => v.to_string(),
        },
        other => other.to_string(),
    }
}

/// `HH:MM` UTC of a millisecond timestamp.
pub(crate) fn clock(t: Timestamp) -> String {
    let secs = t.0 / 1000 % 86_400;
    format!("{:02}:{:02}", secs / 3600, secs / 60 % 60)
}

#[cfg(test)]
mod tests;
