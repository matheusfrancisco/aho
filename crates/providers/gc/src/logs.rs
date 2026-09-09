//! What every session log under the session directory references, on every branch.
//!
//! A log is `<session.dir>/<id>.jsonl` (the CLI names ids in hex, the daemon in decimal) or a
//! child log `<session.dir>/<parent>/<child>.jsonl`; its label is the path below the directory
//! without `.jsonl`. `read_from(None)` is every entry of every branch (a fork only moves the head),
//! so a tree or spill named on an abandoned branch is still referenced (11).

use std::path::{Path, PathBuf};

use aho_log::{EntryKind, JsonlLog, LogStore, ProcessIdentity, State};
use aho_types::{Hash, Timestamp};

/// A `Child/Spawned` receipt with the owner identity in force where it was appended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub child: ProcessIdentity,
    pub owner: Option<ProcessIdentity>,
    pub kind: String,
    pub label: String,
}

pub(crate) struct LogFacts {
    pub label: String,
    /// The last `Session/Meta.process`: the process that owns the session now.
    pub identity: Option<ProcessIdentity>,
    pub trees: Vec<Hash>,
    pub spills: Vec<Hash>,
    pub receipts: Vec<Receipt>,
    /// The fold of the current branch; `None` when it does not fold (then nothing it could own is judged).
    pub state: Option<State>,
    /// Newest entry time.
    pub last_at: Timestamp,
}

impl LogFacts {
    pub(crate) fn config(&self, key: &str) -> Option<String> {
        let v = &self.state.as_ref()?.config.get(key)?.value;
        v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }

    /// `workspace.worktree` when the session ran with `workspace.layout = worktree`.
    pub(crate) fn worktree(&self) -> Option<String> {
        (self.config("workspace.layout").as_deref() == Some("worktree")).then(|| self.config("workspace.worktree")).flatten()
    }
}

#[derive(Default)]
pub(crate) struct Sessions {
    pub logs: Vec<LogFacts>,
    /// Logs that could not be read, with why: anything they may reference is unknown.
    pub unreadable: Vec<(PathBuf, String)>,
}

impl Sessions {
    pub(crate) fn by_label(&self, label: &str) -> Option<&LogFacts> {
        self.logs.iter().find(|l| l.label == label)
    }
}

/// Every `*.jsonl` below `dir`. A missing directory holds no sessions; an unlistable one is an error.
#[allow(clippy::disallowed_methods, reason = "authority: gc-session-directory")]
pub(crate) fn read_all(dir: &Path) -> Result<Sessions, String> {
    let mut out = Sessions::default();
    if !dir.exists() {
        return Ok(out);
    }
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    files.sort();
    for path in files {
        match read_one(dir, &path) {
            Ok(f) => out.logs.push(f),
            Err(e) => out.unreadable.push((path, e)),
        }
    }
    Ok(out)
}

#[allow(clippy::disallowed_methods, reason = "authority: gc-session-walk")]
fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("could not list {} ({e}); nothing was scanned, and nothing is reclaimed", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not list {} ({e}); nothing was scanned, and nothing is reclaimed", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| format!("could not stat {} ({e})", path.display()))?;
        if kind.is_dir() {
            if path.extension().is_some_and(|x| x == "spill") {
                continue;
            }
            walk(&path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn label(dir: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(dir).unwrap_or(path).with_extension("");
    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

fn read_one(dir: &Path, path: &Path) -> Result<LogFacts, String> {
    let log = JsonlLog::open(path).map_err(|e| e.to_string())?;
    let mut f = LogFacts {
        label: label(dir, path),

        identity: None,
        trees: Vec::new(),
        spills: Vec::new(),
        receipts: Vec::new(),
        state: None,
        last_at: Timestamp(0),
    };
    for e in log.read_from(None) {
        f.last_at = Timestamp(f.last_at.0.max(e.at.0));
        match &e.kind {
            EntryKind::WorkspaceCheckpoint { tree, .. } => f.trees.push(*tree),
            EntryKind::ToolResult { spill_ref: Some(h), .. } => f.spills.push(*h),
            EntryKind::SessionMeta { process: Some(p), .. } => f.identity = Some(*p),
            _ => {
                if let Some(r) = aho_proc::child_receipt(&e) {
                    f.receipts.push(Receipt { child: r.child, owner: f.identity, kind: r.kind, label: r.label });
                }
            }
        }
    }
    f.state = match log.head() {
        None => Some(State::default()),
        Some(h) => log.read_branch(h).ok().and_then(|entries| aho_log::fold(&entries, &Default::default()).ok()),
    };
    Ok(f)
}
