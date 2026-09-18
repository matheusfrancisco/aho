//! `aho explain <key>`: winner, every shadowed layer, lint markers, and (from a log) history.

use aho_config::{Loadout, Origin, Scope, Sourced};
use aho_log::{Entry, EntryKind, State};
use aho_types::*;
use serde::Serialize;

use crate::{clock, show_value};

const BROADER_SCOPE_WINS: &str = "broader-scope-wins";

/// A resolver diagnostic about one key, as stored in the loadout or a `LoadoutDiagnostic` entry.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub lint: String,
    pub severity: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Shadowed {
    pub origin: String,
    pub scope: Scope,
    pub value: Value,
    /// This loser was more specific than the winner that beat it (`broader-scope-wins`).
    pub broader_scope_wins: bool,
}

/// One `ConfigResolved` entry for the key on this branch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HistoryRow {
    pub entry: EntryId,
    pub at: Timestamp,
    pub by: Principal,
    /// Written before `PlanSealed`, i.e. part of the session's bootstrap.
    pub bootstrap: bool,
    pub value: Value,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Explanation {
    pub key: String,
    pub value: Option<Value>,
    pub winner: Option<Origin>,
    pub shadowed: Vec<Shadowed>,
    pub findings: Vec<Finding>,
    pub history: Vec<HistoryRow>,
}

/// Anything `explain` can read a key from: a fresh `Loadout`, a folded `State`, or log entries.
pub trait ConfigSource {
    fn sourced(&self, key: &str) -> Option<Sourced<Value>>;
    /// `None` when the source does not keep resolver diagnostics (a folded `State`).
    fn findings(&self, key: &str) -> Option<Vec<Finding>>;
    fn history(&self, _key: &str) -> Vec<HistoryRow> {
        Vec::new()
    }
}

fn serde_name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

impl ConfigSource for Loadout {
    fn sourced(&self, key: &str) -> Option<Sourced<Value>> {
        self.config.get(key).cloned()
    }
    fn findings(&self, key: &str) -> Option<Vec<Finding>> {
        Some(
            self.diagnostics
                .iter()
                .filter(|d| d.key == key)
                .map(|d| Finding { lint: serde_name(&d.lint), severity: serde_name(&d.severity), detail: d.detail.clone() })
                .collect(),
        )
    }
}

impl ConfigSource for State {
    fn sourced(&self, key: &str) -> Option<Sourced<Value>> {
        self.config.get(key).cloned()
    }
    fn findings(&self, _key: &str) -> Option<Vec<Finding>> {
        None
    }
}

impl ConfigSource for [Entry] {
    fn sourced(&self, key: &str) -> Option<Sourced<Value>> {
        self.iter().rev().find_map(|e| match &e.kind {
            EntryKind::ConfigResolved { key: k, sourced } if k == key => Some(sourced.clone()),
            _ => None,
        })
    }
    fn findings(&self, key: &str) -> Option<Vec<Finding>> {
        Some(
            self.iter()
                .filter_map(|e| match &e.kind {
                    EntryKind::LoadoutDiagnostic { lint, severity, key: k, detail } if k == key => {
                        Some(Finding { lint: lint.clone(), severity: severity.clone(), detail: detail.clone() })
                    }
                    _ => None,
                })
                .collect(),
        )
    }
    fn history(&self, key: &str) -> Vec<HistoryRow> {
        let sealed_at = self.iter().find(|e| matches!(e.kind, EntryKind::PlanSealed { .. })).map(|e| e.id);
        self.iter()
            .filter_map(|e| match &e.kind {
                EntryKind::ConfigResolved { key: k, sourced } if k == key => Some(HistoryRow {
                    entry: e.id,
                    at: e.at,
                    by: e.by.clone(),
                    bootstrap: sealed_at.is_none_or(|s| e.id < s),
                    value: sourced.value.clone(),
                    origin: sourced.winner.render(),
                }),
                _ => None,
            })
            .collect()
    }
}

pub fn explain<S: ConfigSource + ?Sized>(src: &S, key: &str) -> Explanation {
    let sourced = src.sourced(key);
    let findings = src.findings(key);
    let shadowed = match &sourced {
        None => Vec::new(),
        Some(s) => s
            .shadowed
            .iter()
            .map(|(o, v)| {
                let broader_scope_wins = match &findings {
                    Some(fs) => {
                        let needle = format!("beats {} (", o.render());
                        fs.iter().any(|f| f.lint == BROADER_SCOPE_WINS && f.detail.contains(&needle))
                    }
                    // No diagnostics kept: apply the lint's own rule to the recorded origins.
                    None => s.winner.scope < o.scope,
                };
                Shadowed { origin: o.render(), scope: o.scope, value: v.clone(), broader_scope_wins }
            })
            .collect(),
    };
    Explanation {
        key: key.to_string(),
        value: sourced.as_ref().map(|s| s.value.clone()),
        winner: sourced.map(|s| s.winner),
        shadowed,
        findings: findings.unwrap_or_default(),
        history: src.history(key),
    }
}

fn principal(p: &Principal) -> String {
    match p {
        Principal::Operator => "operator".into(),
        Principal::Engineer => "engineer".into(),
        Principal::Agent { turn, .. } => format!("agent (turn {})", turn.0),
        Principal::System { name } => format!("system:{name}"),
    }
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s} ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

pub fn render_explain(e: &Explanation) -> String {
    let (Some(value), Some(winner)) = (&e.value, &e.winner) else {
        return format!("{} is not set by any layer\n", e.key);
    };
    let mut out = format!("{} = {}\n", e.key, show_value(value));
    out.push_str(&format!("  winner   {}scope: {}\n", pad(&winner.render(), 44), serde_name(&winner.scope)));
    for s in &e.shadowed {
        let marker = if s.broader_scope_wins { "   ⚠ broader-scope-wins" } else { "" };
        let lhs = format!("{} = {}", s.origin, show_value(&s.value));
        out.push_str(&format!("  shadowed {}scope: {}{marker}\n", pad(&lhs, 44), serde_name(&s.scope)));
    }
    for f in e.findings.iter().filter(|f| f.lint != BROADER_SCOPE_WINS) {
        out.push_str(&format!("  ⚠ {} ({}): {}\n", f.lint, f.severity, f.detail));
    }
    if !e.history.is_empty() {
        out.push_str("history (this session):\n");
        for h in &e.history {
            let how = if h.bootstrap { "(bootstrap)".to_string() } else { format!("by: {}  at {}", principal(&h.by), clock(h.at)) };
            out.push_str(&format!("  entry {:<6} ConfigResolved   {how}   = {}\n", h.entry.0, show_value(&h.value)));
        }
    }
    out
}
