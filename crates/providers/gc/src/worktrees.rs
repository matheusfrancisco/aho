//! Worktrees (`workspace.layout = worktree` works in `.aho/worktrees/<name>` on branch
//! `aho/<name>`, a real `git worktree`). A session names its worktree in its bootstrap
//! (`Config/Resolved` of `workspace.layout` and `workspace.worktree`). A worktree is an orphan
//! when every log naming it is closed longer than the retention, or when no log names it and it
//! has not changed for that long. Worktrees with uncommitted changes are never listed, and
//! reclaiming runs `git worktree remove` (which refuses a dirty or locked tree); the branch
//! `aho/<name>` and its commits stay.

use std::path::{Path, PathBuf};

use aho_inspect::{SessionStatus, session_status};

use crate::children::{Liveness, Prober};
use crate::logs::{LogFacts, Sessions};
use crate::{GcReport, Kept, Orphan, OrphanKind, ScanInputs, Target, age_of, git};

/// Whether the log's session is closed (not merely crashed or waiting): its current branch
/// folds to `SessionStatus::Closed`. A log that does not fold counts as open.
fn closed(log: &LogFacts, prober: &mut Prober) -> bool {
    let Some(state) = &log.state else { return false };
    let alive = match log.identity {
        Some(id) => prober.owner(id) != Liveness::Gone,
        None => false,
    };
    session_status(state, alive).0 == SessionStatus::Closed
}

#[allow(clippy::disallowed_methods, reason = "authority: gc-worktree-root")]
fn names_root(log: &LogFacts, base: &Path) -> bool {
    match log.config("workspace.root") {
        None => true,
        Some(r) => std::fs::canonicalize(&r).map(|r| r == base).unwrap_or(false),
    }
}

/// Bytes of every file below `dir` (symlinks are not followed).
#[allow(clippy::disallowed_methods, reason = "authority: gc-worktree-size")]
pub(crate) fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in entries.flatten() {
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        n += if m.is_dir() { size_of(&e.path()) } else { m.len() };
    }
    n
}

/// Paths `git status` reports as changed or untracked.
fn dirty(path: &Path) -> Result<usize, String> {
    let out = git::run(
        [std::ffi::OsStr::new("-C"), path.as_os_str(), "status".as_ref(), "--porcelain".as_ref(), "--untracked-files=all".as_ref()],
        &format!("read the status of worktree {}", path.display()),
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count())
}

#[allow(clippy::disallowed_methods, reason = "authority: gc-worktree-scan")]
pub(crate) fn judge(inputs: &ScanInputs, sessions: &Sessions, prober: &mut Prober, report: &mut GcReport) {
    let Ok(base) = std::fs::canonicalize(&inputs.project) else { return };
    let dir = base.join(".aho/worktrees");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join(".git").exists()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let shown = path.display().to_string();
        if !sessions.unreadable.is_empty() {
            report.kept.push(Kept {
                kind: OrphanKind::Worktree,
                path: shown,
                why: format!("{} session log(s) could not be read, and any of them may be using it", sessions.unreadable.len()),
            });
            continue;
        }
        let owners: Vec<&LogFacts> =
            sessions.logs.iter().filter(|l| l.worktree().as_deref() == Some(name.as_str()) && names_root(l, &base)).collect();
        let (owner, age) = if owners.is_empty() {
            ("-".to_string(), age_of(&path, inputs.now))
        } else {
            if owners.iter().any(|l| !closed(l, prober)) {
                continue;
            }
            let newest = owners.iter().max_by_key(|l| l.last_at.0).map(|l| l.label.clone()).unwrap_or_default();
            let last = owners.iter().map(|l| l.last_at.0).max().unwrap_or(0);
            (newest, Some(inputs.now.0.saturating_sub(last)))
        };
        let Some(age) = age else { continue };
        if age < inputs.retention_ms {
            if owners.is_empty() {
                report.kept.push(Kept {
                    kind: OrphanKind::Worktree,
                    path: shown,
                    why: "no session log names it, but it changed within the retention period".into(),
                });
            }
            continue;
        }
        match dirty(&path) {
            Ok(0) => {}
            Ok(n) => {
                report.kept.push(Kept {
                    kind: OrphanKind::Worktree,
                    path: shown,
                    why: format!("{n} uncommitted change(s); commit or discard them, then run aho gc again"),
                });
                continue;
            }
            Err(e) => {
                report.kept.push(Kept { kind: OrphanKind::Worktree, path: shown, why: e });
                continue;
            }
        }
        report.items.push(Orphan {
            owner,
            kind: OrphanKind::Worktree,
            age_ms: Some(age),
            size: Some(size_of(&path)),
            path: shown,
            target: Target::Worktree { base: base.clone(), path: path.clone() },
        });
    }
}

/// `git worktree remove <path>` from the main repository; git refuses a dirty or locked tree.
pub(crate) fn remove(base: &Path, path: &Path) -> Result<(), String> {
    git::run(
        [std::ffi::OsStr::new("-C"), base.as_os_str(), "worktree".as_ref(), "remove".as_ref(), path.as_os_str()],
        &format!("remove worktree {}", path.display()),
    )
    .map(|_| ())
}
