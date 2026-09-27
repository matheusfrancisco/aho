//! The evidence a verify gate's `task_complete` hands the engine: a Verify gate cannot pass while
//! a covering invariant is `Fail` or `Stale`.
//!
//! The invariant records the running system keeps, and where this reads them:
//! * `Invariant/Registered { id, covers }`: appended by tool-formal on the engineer's approval;
//!   `covers` are paths, globs, log kinds or entry ranges. Only entries hold them.
//! * `Invariant/Checked { id, verdict, code_hash }`: the only way a verdict enters the log; the
//!   fold keeps the latest per id (`State::invariants`), and `aho_inspect::invariants` judges it
//!   against the current workspace checkpoint: a check of other code is `Stale`, never `Pass`.
//!   tool-formal binds each verdict to the workspace tree it checked, so the rows are
//!   `aho_inspect::current_invariants`: Stale once a later checkpoint has other code; before the
//!   first checkpoint the recorded verdict stands.
//!
//! A completing verify gate folds the session log (`aho.log`) as it stands at that
//! moment, so a file written earlier in the same step already makes a check stale. Without the
//! log (a composition outside a session) the rows come from the state the context hook last saw
//! and no registration is known.
//!
//! An invariant covers the gate when one of its `covers` matches a file the gate's scope handed
//! over (`Graph::gate_files`: exact path, a directory above it, or a glob). When the scope names
//! no files, or an invariant was never registered, nothing shows it does not cover the gate, so it
//! counts.

use std::collections::BTreeMap;

use aho_inspect::InvariantRow;
use aho_log::{CustomFolds, Entry, EntryKind, State, Verdict, fold};
use aho_tasks::{InvariantSeen, InvariantStatus};

#[derive(Clone, Debug, Default)]
pub(crate) struct Evidence {
    rows: Vec<InvariantRow>,
    /// Every registration's `covers`, per id; `None` when no registration can be known.
    covers: Option<BTreeMap<String, Vec<String>>>,
}

impl Evidence {
    /// Rows from the folded state only (the context hook's view); coverage unknown.
    pub(crate) fn from_state(state: &State) -> Evidence {
        Evidence { rows: aho_inspect::current_invariants(state), covers: None }
    }

    /// Rows and registrations from the branch as it stands.
    pub(crate) fn from_entries(entries: &[Entry]) -> Result<Evidence, String> {
        let state = fold(entries, &CustomFolds::default()).map_err(|e| e.to_string())?;
        let mut covers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for e in entries {
            if let EntryKind::InvariantRegistered { id, covers: c, .. } = &e.kind {
                covers.entry(id.clone()).or_default().extend(c.iter().cloned());
            }
        }
        Ok(Evidence { rows: aho_inspect::current_invariants(&state), covers: Some(covers) })
    }

    /// Every invariant covering `files`, with its status now, sorted by id.
    pub(crate) fn covering(&self, files: &[String]) -> Vec<InvariantSeen> {
        self.rows
            .iter()
            .filter(|r| files.is_empty() || self.patterns(&r.id).is_none_or(|ps| ps.iter().any(|p| files.iter().any(|f| matches(p, f)))))
            .map(|r| InvariantSeen { id: r.id.clone(), status: status(&r.verdict) })
            .collect()
    }

    fn patterns(&self, id: &str) -> Option<&Vec<String>> {
        self.covers.as_ref().and_then(|c| c.get(id))
    }
}

fn status(v: &Verdict) -> InvariantStatus {
    match v {
        Verdict::Pass => InvariantStatus::Pass,
        Verdict::Fail { .. } => InvariantStatus::Fail,
        Verdict::Unknown { .. } => InvariantStatus::Unknown,
        Verdict::Stale => InvariantStatus::Stale,
        Verdict::Vacuous => InvariantStatus::Vacuous,
    }
}

fn clean(s: &str) -> &str {
    s.trim().trim_start_matches("./").trim_end_matches('/')
}

/// A `covers` entry against one workspace-relative file: the file itself, a directory above it,
/// or a glob (`*` within one path segment, `**` across them, `?`, `[..]`, `{..}`) matching it.
fn matches(pattern: &str, file: &str) -> bool {
    let (p, f) = (clean(pattern), clean(file));
    if p.is_empty() {
        return false;
    }
    if f == p || f.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')) {
        return true;
    }
    p.contains(['*', '?', '[', '{'])
        && globset::GlobBuilder::new(p).literal_separator(true).build().is_ok_and(|g| g.compile_matcher().is_match(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_match_files_directories_and_globs_but_not_log_kinds() {
        assert!(matches("src/parser.rs", "./src/parser.rs"));
        assert!(matches("src/", "src/parser.rs") && matches("src", "src/deep/x.rs"));
        assert!(!matches("src/par", "src/parser.rs"), "a directory ends at a separator");
        assert!(matches("src/**/*.rs", "src/deep/x.rs") && matches("src/*.rs", "src/lib.rs") && !matches("src/*.rs", "src/deep/x.rs"));
        assert!(!matches("Task/*", "src/parser.rs") && !matches("", "src/parser.rs"));
    }
}
