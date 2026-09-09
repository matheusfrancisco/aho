//! `aho gc [--dry-run]`: what aho left on disk and in the process table that no session needs.
//!
//! * 04: "quotas and an owner-session on every resource-holding capability; `aho gc --dry-run`
//!   lists orphans by owner and age; `aho gc` reclaims".
//! * 11: "a tree is reachable while any log branch references it; `aho gc` prunes the rest".
//! * 12: "`aho gc` reaps children (viewers, jobs, sandboxes) orphaned by a dead identity …
//!   an identity must be missing on two consecutive scans before anything is marked gone";
//!   "crash-honest receipts".
//!
//! [`scan`] reads every session log under the session directory, on every branch, and lists:
//! * [`OrphanKind::Snapshot`] — checkpoint trees in this project's shadow repositories that no
//!   `Workspace/Checkpoint` references ([`snapshots`](crate::snapshots));
//! * [`OrphanKind::Worktree`] — `.aho/worktrees/<name>` whose sessions' logs are gone or closed
//!   longer than the retention;
//! * [`OrphanKind::Spill`] — spill files no `Tool/Result.spill_ref` names;
//! * [`OrphanKind::Child`] — children with a `Child/Spawned` receipt (`aho_proc::spawned`, written
//!   by the session's operations table) whose owner identity `(pid, start_time)` from
//!   `Session/Meta` was missing on this scan and the previous one (the state file carries the
//!   previous scan across runs).
//! * [`OrphanKind::Sandbox`] / [`OrphanKind::Machine`] — a namespace sandbox or network whose
//!   session log is gone, and VM machines whose session is gone or whose lease ended
//!   ([`sandboxes`](crate::sandboxes)).
//!
//! Nothing a live session references is listed: referenced trees and spills are kept whoever owns
//! them, the spill directory of a live owner is skipped, whatever is younger than the grace period
//! is kept, a worktree with uncommitted changes is kept, and an unreadable log stops every
//! judgement it could affect. [`reclaim`] scans again and removes only the report's items that the
//! fresh scan still lists.

#![forbid(unsafe_code)]

mod children;
mod git;
mod logs;
mod render;
mod sandboxes;
mod snapshots;
mod spill;
mod state;
mod worktrees;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use aho_log::ProcessIdentity;
use aho_types::Timestamp;

pub use children::{Liveness, child_liveness, liveness};
pub use render::{render_outcome, render_report};
pub use sandboxes::default_vm_state;

/// Name of the two-scan state file [`ScanInputs::new`] keeps in the session directory.
pub const STATE_FILE: &str = "gc-scan.json";
/// A closed session's worktree is kept this long after its last entry (`gc.worktree_retention_days`).
pub const DEFAULT_RETENTION_MS: u64 = 7 * 24 * 3_600_000;
/// Anything younger than this is kept: its reference may still be on its way into a log (`gc.grace_minutes`).
pub const DEFAULT_GRACE_MS: u64 = 3_600_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanInputs {
    /// The project (workspace root) whose `.aho/worktrees/*` and shadow repositories are judged.
    pub project: PathBuf,
    /// `session.dir`: session logs (`*.jsonl`, child logs in subdirectories) and `<id>.spill/`.
    pub session_dir: PathBuf,
    /// `workspace.shadow_dir` (default `~/.aho/shadow`); logs may name more.
    pub shadow_dir: PathBuf,
    /// Where the previous scan's missing owners are kept; `None` makes every scan a first scan.
    pub state_file: Option<PathBuf>,
    pub now: Timestamp,
    pub retention_ms: u64,
    pub grace_ms: u64,
    /// `vm.dir/state`: machines of this project's sessions are judged there; `None` skips VMs.
    pub vm_state: Option<PathBuf>,
}

impl ScanInputs {
    /// Defaults: the state file in the session directory, a week of worktree retention, an hour of grace.
    pub fn new(project: impl Into<PathBuf>, session_dir: impl Into<PathBuf>, shadow_dir: impl Into<PathBuf>, now: Timestamp) -> ScanInputs {
        let session_dir = session_dir.into();
        ScanInputs {
            project: project.into(),
            state_file: Some(session_dir.join(STATE_FILE)),
            session_dir,
            shadow_dir: shadow_dir.into(),
            now,
            retention_ms: DEFAULT_RETENTION_MS,
            grace_ms: DEFAULT_GRACE_MS,
            vm_state: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OrphanKind {
    Snapshot,
    Worktree,
    Spill,
    Child,
    Sandbox,
    Machine,
}

impl OrphanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OrphanKind::Snapshot => "snapshot",
            OrphanKind::Worktree => "worktree",
            OrphanKind::Spill => "spill",
            OrphanKind::Child => "child",
            OrphanKind::Sandbox => "sandbox",
            OrphanKind::Machine => "machine",
        }
    }
}

/// What reclaiming an orphan acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An unreferenced root tree; the loose objects only orphans reach go with it.
    Snapshot {
        repo: PathBuf,
        tree: String,
    },
    /// `git -C <base> worktree remove <path>`.
    Worktree {
        base: PathBuf,
        path: PathBuf,
    },
    Spill {
        path: PathBuf,
    },
    /// SIGTERM, then SIGKILL, to the child's process group.
    Child {
        child: ProcessIdentity,
        owner: ProcessIdentity,
    },
    /// A namespace sandbox: its keeper is stopped and the directory removed.
    Namespace {
        dir: PathBuf,
    },
    /// A stopped network's state directory.
    Network {
        dir: PathBuf,
    },
    /// A VM machine: QEMU quit over its owned QMP socket, then its directory and disks removed.
    Machine {
        state: PathBuf,
        owner: String,
        machine: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    /// The session log that owned it (its label below the session directory), `-` when no log names it.
    pub owner: String,
    pub kind: OrphanKind,
    pub age_ms: Option<u64>,
    /// Bytes on disk it frees; `None` for processes.
    pub size: Option<u64>,
    /// What the table shows in the PATH column.
    pub path: String,
    pub target: Target,
}

impl Orphan {
    /// Identity across two scans of the same inputs.
    fn key(&self) -> String {
        match &self.target {
            Target::Snapshot { repo, tree } => format!("snapshot {} {tree}", repo.display()),
            Target::Worktree { path, .. } => format!("worktree {}", path.display()),
            Target::Spill { path } => format!("spill {}", path.display()),
            Target::Child { child, .. } => format!("child {} {}", child.pid, child.start_time.0),
            Target::Namespace { dir } => format!("namespace {}", dir.display()),
            Target::Network { dir } => format!("network {}", dir.display()),
            Target::Machine { owner, machine, .. } => format!("machine {owner} {machine}"),
        }
    }
}

/// A candidate that was not listed, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub kind: OrphanKind,
    pub path: String,
    pub why: String,
}

/// An owner missing for the first time: its running children are judged on the next scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub owner: String,
    pub identity: ProcessIdentity,
    pub children: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcReport {
    pub inputs: ScanInputs,
    pub items: Vec<Orphan>,
    pub kept: Vec<Kept>,
    pub pending: Vec<Pending>,
    pub notes: Vec<String>,
    /// Owner identities gone on this scan: what the next scan confirms against.
    pub missing: Vec<ProcessIdentity>,
    /// Set when the session directory could not be listed: then nothing is known, and nothing is listed.
    pub blind: Option<String>,
    referenced: HashSet<String>,
}

impl GcReport {
    fn empty(inputs: &ScanInputs) -> GcReport {
        GcReport {
            inputs: inputs.clone(),
            items: Vec::new(),
            kept: Vec::new(),
            pending: Vec::new(),
            notes: Vec::new(),
            missing: Vec::new(),
            blind: None,
            referenced: HashSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReclaimOpts {
    /// How long a child gets after SIGTERM before SIGKILL.
    pub term_grace_ms: u64,
}

impl Default for ReclaimOpts {
    fn default() -> ReclaimOpts {
        ReclaimOpts { term_grace_ms: 2_000 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcOutcome {
    pub removed: Vec<Orphan>,
    /// Items the fresh scan no longer lists (or that were already gone), with why.
    pub skipped: Vec<(Orphan, String)>,
    pub failed: Vec<(Orphan, String)>,
    pub freed: u64,
}

/// Milliseconds since `path` last changed, `None` when it cannot be read.
#[allow(clippy::disallowed_methods, reason = "authority: gc-artifact-age")]
pub(crate) fn age_of(path: &Path, now: Timestamp) -> Option<u64> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    let ms = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
    Some(now.0.saturating_sub(ms))
}

/// One scan (a dry run): reads the previous scan's missing owners from the state file and
/// records this scan's in it.
pub fn scan(inputs: &ScanInputs) -> GcReport {
    let (previous, note) = match &inputs.state_file {
        None => (Vec::new(), None),
        Some(p) => match state::load(p) {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e)),
        },
    };
    let mut report = scan_with(inputs, &previous);
    report.notes.extend(note);
    if let Some(p) = &inputs.state_file
        && report.blind.is_none()
        && let Err(e) = state::save(p, inputs.now, &report.missing)
    {
        report.notes.push(e);
    }
    report
}

fn scan_with(inputs: &ScanInputs, previous: &[ProcessIdentity]) -> GcReport {
    let mut report = GcReport::empty(inputs);
    let sessions = match logs::read_all(&inputs.session_dir) {
        Ok(s) => s,
        Err(e) => {
            report.blind = Some(e);
            return report;
        }
    };
    for (path, why) in &sessions.unreadable {
        report.notes.push(format!("could not read the session log {} ({why}); what it may reference is kept", path.display()));
    }
    report.referenced = sessions.logs.iter().flat_map(|l| l.trees.iter().map(|t| t.to_hex())).collect();
    let mut prober = children::Prober::default();
    children::judge(&sessions, previous, inputs.now, &mut prober, &mut report);
    spill::judge(inputs, &sessions, &mut prober, &mut report);
    worktrees::judge(inputs, &sessions, &mut prober, &mut report);
    let referenced = std::mem::take(&mut report.referenced);
    snapshots::judge(inputs, &sessions, &referenced, &mut report);
    sandboxes::judge(inputs, &sessions, &mut report);
    report.referenced = referenced;
    report.missing = prober.owners_gone();
    report.items.sort_by(|a, b| (a.kind, &a.owner, &a.path).cmp(&(b.kind, &b.owner, &b.path)));
    report
}

/// Removes the report's items that a fresh scan (confirming against the report's missing owners)
/// still lists. The fresh scan does not touch the state file.
#[allow(clippy::disallowed_methods, reason = "authority: gc-reclaim-spill")]
pub fn reclaim(report: &GcReport, opts: &ReclaimOpts) -> Result<GcOutcome, String> {
    if let Some(why) = &report.blind {
        return Err(format!(
            "the scan could not read the session logs ({why}), so nothing is known to be an orphan; nothing was reclaimed"
        ));
    }
    let fresh = scan_with(&report.inputs, &report.missing);
    if let Some(why) = &fresh.blind {
        return Err(format!("the session logs cannot be read again ({why}); nothing was reclaimed"));
    }
    let now: HashMap<String, &Orphan> = fresh.items.iter().map(|o| (o.key(), o)).collect();
    let mut out = GcOutcome::default();
    let mut trees: BTreeMap<PathBuf, Vec<&Orphan>> = BTreeMap::new();
    for item in &report.items {
        let Some(current) = now.get(&item.key()) else {
            out.skipped.push((item.clone(), "a fresh scan no longer lists it (a session references it again, or it is gone)".into()));
            continue;
        };
        let done = match &current.target {
            Target::Snapshot { repo, .. } => {
                trees.entry(repo.clone()).or_default().push(current);
                continue;
            }
            Target::Worktree { base, path } => worktrees::remove(base, path).map(|_| current.size.unwrap_or(0)),
            Target::Spill { path } => std::fs::remove_file(path)
                .map(|_| current.size.unwrap_or(0))
                .map_err(|e| format!("could not remove {} ({e}); it is still there", path.display())),
            Target::Child { child, .. } => children::reap(child, opts.term_grace_ms).map(|_| 0),
            t @ (Target::Namespace { .. } | Target::Network { .. } | Target::Machine { .. }) => {
                sandboxes::reclaim(t).map(|()| current.size.unwrap_or(0))
            }
        };
        match done {
            Ok(n) => {
                out.freed += n;
                out.removed.push((*current).clone());
            }
            Err(e) => out.failed.push(((*current).clone(), e)),
        }
    }
    for (repo, items) in trees {
        let roots: Vec<String> = items
            .iter()
            .filter_map(|o| match &o.target {
                Target::Snapshot { tree, .. } => Some(tree.clone()),
                _ => None,
            })
            .collect();
        match snapshots::delete(&repo, &roots, &fresh.referenced, report.inputs.now, report.inputs.grace_ms) {
            Ok((gone, kept, freed)) => {
                out.freed += freed;
                for o in items {
                    let Target::Snapshot { tree, .. } = &o.target else { continue };
                    if gone.contains(tree) {
                        out.removed.push(o.clone());
                    } else if let Some((_, why)) = kept.iter().find(|(t, _)| t == tree) {
                        out.skipped.push((o.clone(), why.clone()));
                    }
                }
            }
            Err(e) => out.failed.extend(items.into_iter().map(|o| (o.clone(), e.clone()))),
        }
    }
    Ok(out)
}

/// The `aho gc [--dry-run]` verb: scan, then (unless `dry_run`) reclaim; the rendered text.
pub fn run(inputs: &ScanInputs, dry_run: bool) -> Result<String, String> {
    let report = scan(inputs);
    if let Some(why) = &report.blind {
        return Err(format!("{why}. Check session.dir, then run aho gc again."));
    }
    let mut text = render_report(&report);
    if !dry_run && !report.items.is_empty() {
        let outcome = reclaim(&report, &ReclaimOpts::default())?;
        text.push('\n');
        text.push_str(&render_outcome(&outcome));
    }
    Ok(text)
}
