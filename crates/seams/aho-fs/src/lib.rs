//! `02` — the Fs seam. Definition crate: trait + handle newtypes + `Snapshot`, plus the pure
//! helpers every provider shares (lexical path fence, content versions, unified-diff hunks).
//! No I/O here; providers are `world-host`, `world-ns`, `world-vm`, or yours (15).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;

use aho_cap::{Handle, HandleKind, SeamId};
use aho_types::*;

pub const SEAM: SeamId = SeamId("fs");
pub const SNAPSHOT_SEAM: SeamId = SeamId("snapshot");

/// Result cap for `glob` / `grep`; providers return at most this many, sorted.
pub const MAX_HITS: usize = 500;

/// Lines of context around each change in an `AppliedDiff` hunk (10).
pub const DIFF_CONTEXT: usize = 3;

// ───────────────────────────── versions ─────────────────────────────

/// Content version: the first 8 bytes of blake3(content). Changes exactly when the content does,
/// so the read ledger can refuse an edit based on a stale read (`FS_STALE_VERSION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileVersion(pub u64);

impl FileVersion {
    pub fn of(content: &[u8]) -> FileVersion {
        let h = Hash::of(content);
        let mut b = [0u8; 8];
        b.copy_from_slice(&h.0[..8]);
        FileVersion(u64::from_be_bytes(b))
    }
    /// 16 lowercase hex digits (JSON-safe; a u64 does not survive a JS number).
    pub fn to_hex(&self) -> String {
        format!("{:016x}", self.0)
    }
    pub fn from_hex(s: &str) -> Option<FileVersion> {
        if s.len() != 16 {
            return None;
        }
        u64::from_str_radix(s, 16).ok().map(FileVersion)
    }
}

impl fmt::Display for FileVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

// ───────────────────────────── results ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ReadResult {
    /// The requested lines verbatim (no line numbers; tools add those for the model).
    pub text: String,
    /// Version of the whole file, even for a partial read.
    pub version: FileVersion,
    pub total_lines: u32,
    /// True when `range` covered fewer lines than the file has.
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    pub is_dir: bool,
    pub bytes: u64,
    /// `Some` for files from `stat`; `None` for directories and for `list` entries.
    pub version: Option<FileVersion>,
}

/// One hunk per change with three lines of context (10). `created` for a new file.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedDiff {
    pub path: String,
    pub created: bool,
    pub hunks: Vec<String>,
    pub new_version: FileVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrepHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

// ───────────────────────────── errors ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum FsError {
    NotFound(String),
    NotObserved(String),
    StaleVersion {
        path: String,
        expected: FileVersion,
        actual: FileVersion,
    },
    OutsideWorkspace(String),
    DanglingSymlink(String),
    NotUnique {
        path: String,
        count: usize,
    },
    /// `edit` found no occurrence of the text to replace.
    NoMatch(String),
    /// The request itself is malformed (empty `old`, directory where a file is needed, …).
    Invalid(String),
    Denied(String),
    Io(String),
}

impl FsError {
    /// Stable machine code carried in `ExecOutcome::Err{code}`.
    pub fn code(&self) -> &'static str {
        match self {
            FsError::NotFound(_) => "FS_NOT_FOUND",
            FsError::NotObserved(_) => "FS_NOT_OBSERVED",
            FsError::StaleVersion { .. } => "FS_STALE_VERSION",
            FsError::OutsideWorkspace(_) => "FS_OUTSIDE_WORKSPACE",
            FsError::DanglingSymlink(_) => "FS_DANGLING_SYMLINK",
            FsError::NotUnique { .. } => "FS_NOT_UNIQUE",
            FsError::NoMatch(_) => "FS_NO_MATCH",
            FsError::Invalid(_) => "FS_INVALID",
            FsError::Denied(_) => "FS_DENIED",
            FsError::Io(_) => "FS_IO",
        }
    }
}

/// Message voice (12): what happened, what we did, how to change it.
impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound(p) => write!(f, "`{p}` does not exist in the workspace; nothing was done. Check the path with glob."),
            FsError::NotObserved(p) => {
                write!(f, "`{p}` has not been read in this session, so the change was refused. Read it first, then retry.")
            }
            FsError::StaleVersion { path, expected, actual } => write!(
                f,
                "`{path}` changed since it was last read (read version {expected}, now {actual}); nothing was written. Read it again and redo the change."
            ),
            FsError::OutsideWorkspace(p) => {
                write!(f, "`{p}` resolves outside the workspace, so it was refused. Use a path inside the workspace root.")
            }
            FsError::DanglingSymlink(p) => {
                write!(
                    f,
                    "`{p}` is a symlink whose target does not exist, so it was refused. Point it at a real file inside the workspace."
                )
            }
            FsError::NotUnique { path, count } => write!(
                f,
                "the text to replace occurs {count} times in `{path}`; nothing was changed. Include more surrounding lines to make it unique, or set replace_all."
            ),
            FsError::NoMatch(p) => write!(
                f,
                "the text to replace was not found in `{p}`; nothing was changed. Read the file again and copy the exact text, including whitespace."
            ),
            FsError::Invalid(m) | FsError::Denied(m) | FsError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for FsError {}

// ───────────────────────────── the seam ─────────────────────────────
/// An exclusively allocated scratch directory in the bound filesystem's world.
///
/// Providers fence the parent like any other write and own cleanup of the entire subtree.
/// Dropping the lease must initiate cleanup even when allocation's caller or a later operation
/// is cancelled. Call `close` to await cleanup and observe errors before returning a result.
pub trait ScratchDir: Send + Sync {
    /// Absolute path in the bound world, never an ambient host path substituted for that world.
    fn path(&self) -> &str;
    /// Idempotently remove the owned subtree. Cancellation must leave drop cleanup armed.
    fn close(&mut self) -> BoxFuture<'_, Result<(), FsError>>;
}

pub trait Fs: Send + Sync {
    /// The workspace root every relative path resolves against (absolute).
    fn root(&self) -> String;
    /// `range` = 1-based inclusive `(first_line, last_line)`.
    fn read(&self, path: &str, range: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>>;
    fn read_bytes(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>>;
    fn stat(&self, path: &str) -> BoxFuture<'_, Result<Stat, FsError>>;
    /// Atomic (temp file + rename) and version-checked: `Some(v)` refuses unless the file exists
    /// at version `v` (`StaleVersion` / `NotFound`); `None` writes unconditionally (a new file, or
    /// a caller that did its own read-before-write check). Creates parent dirs; never climbs out.
    fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>>;
    /// Replace `old` with `new`; `old` must occur exactly once unless `replace_all`.
    fn edit(&self, path: &str, old: &str, new: &str, replace_all: bool, expect: FileVersion)
    -> BoxFuture<'_, Result<AppliedDiff, FsError>>;
    /// Workspace-relative file paths matching `pattern` under `base`; sorted, ≤ `MAX_HITS`, `.gitignore` honored.
    fn glob(&self, pattern: &str, base: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>>;
    /// Regex over text files under `base` (or the one file `base` names); `include` = file glob.
    fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>>;
    fn list(&self, dir: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>>;
    /// Removes a file, a symlink, or a directory tree. Never the root itself.
    fn remove(&self, path: &str) -> BoxFuture<'_, Result<(), FsError>>;
    /// Create a directory and any missing ancestors within the same lexical and symlink fence
    /// as `write`. Succeeds when the directory already exists.
    fn create_dir(&self, _dir: &str) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async { Err(FsError::Denied("this filesystem does not support directory creation".into())) })
    }
    /// Exclusively create a scratch directory under an existing, fenced parent. `prefix` must
    /// be a single path component. The returned lease owns cleanup, including on cancellation.
    /// Providers without scratch support fail closed; callers must not stage on the host instead.
    fn scratch_dir(&self, _parent: &str, _prefix: &str) -> BoxFuture<'_, Result<Box<dyn ScratchDir>, FsError>> {
        Box::pin(async { Err(FsError::Denied("this filesystem does not support owned scratch directories".into())) })
    }
}

/// Read side of a world's fs. Handle kind `FsRead`.
#[derive(Clone)]
pub struct FsRead(pub Arc<dyn Fs>);
/// Write side of a world's fs. Handle kind `FsWrite` (ceiling: `WorkspaceWrite`).
#[derive(Clone)]
pub struct FsWrite(pub Arc<dyn Fs>);
/// Host-only variants exist so a daemon provider's manifest can be printed honestly (07).
#[derive(Clone)]
pub struct HostFsRead(pub Arc<dyn Fs>);
#[derive(Clone)]
pub struct HostFsWrite(pub Arc<dyn Fs>);

impl Handle for FsRead {
    const KIND: HandleKind = HandleKind::FsRead;
}
impl Handle for FsWrite {
    const KIND: HandleKind = HandleKind::FsWrite;
}
impl Handle for HostFsRead {
    const KIND: HandleKind = HandleKind::HostFsRead;
}
impl Handle for HostFsWrite {
    const KIND: HandleKind = HandleKind::HostFsWrite;
}

// ───────────────────────────── snapshots (11) ─────────────────────────────

/// Content-addressed workspace trees. Optional per world; absent → `checkpoint` is Locked.
pub trait Snapshot: Send + Sync {
    /// Record the current workspace (untracked included, ignored excluded) and name it.
    fn tree(&self) -> BoxFuture<'_, Result<Hash, FsError>>;
    /// Make the workspace equal `tree`, minimally; ignored paths are left alone.
    fn restore(&self, tree: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>>;
    /// What `restore(b)` would change starting from `a`.
    fn diff(&self, a: Hash, b: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreDelta {
    /// Written (created or modified), workspace-relative, sorted.
    pub changed: Vec<String>,
    /// Present before, absent in the target, sorted.
    pub deleted: Vec<String>,
}

#[derive(Clone)]
pub struct SnapshotHandle(pub Arc<dyn Snapshot>);
impl Handle for SnapshotHandle {
    const KIND: HandleKind = HandleKind::Snapshot;
}

// ───────────────────────────── lexical fence ─────────────────────────────

/// Lexically resolve `path` against the absolute `root` into a workspace-relative, `/`-separated
/// path (`"."` for the root itself). Refuses a `..` that climbs above the root and absolute paths
/// outside it. Never touches the filesystem: providers add the symlink checks on top.
pub fn lexical_rel(root: &str, path: &str) -> Result<String, FsError> {
    let outside = || FsError::OutsideWorkspace(path.to_string());
    let root_parts: Vec<&str> = root.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let mut stack: Vec<&str> = Vec::new();
    let absolute = path.starts_with('/');
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err(outside());
                }
            }
            c => stack.push(c),
        }
    }
    let rel: &[&str] = if absolute {
        if stack.len() < root_parts.len() || stack[..root_parts.len()] != root_parts[..] {
            return Err(outside());
        }
        &stack[root_parts.len()..]
    } else {
        &stack
    };
    if rel.is_empty() { Ok(".".to_string()) } else { Ok(rel.join("/")) }
}

// ───────────────────────────── diff ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Del,
    Ins,
}

/// Cells of the LCS table we are willing to fill; beyond this the changed middle is shown as one
/// replace hunk (still exact, just not minimal).
const LCS_BUDGET: usize = 1_000_000;

/// Unified-diff hunks (`@@ -a,b +c,d @@` + ` `/`-`/`+` lines) turning `old` into `new`.
pub fn unified_hunks(old: &str, new: &str, context: usize) -> Vec<String> {
    let a: Vec<&str> = split_lines(old);
    let b: Vec<&str> = split_lines(new);
    let ops = line_ops(&a, &b);
    if ops.iter().all(|(o, _, _)| *o == Op::Eq) {
        return Vec::new();
    }
    // (op, old_index, new_index) with indices of the line each op consumes.
    let changes: Vec<usize> = ops.iter().enumerate().filter(|(_, (o, _, _))| *o != Op::Eq).map(|(i, _)| i).collect();
    let mut groups: Vec<(usize, usize)> = Vec::new(); // inclusive op ranges of changes
    for &i in &changes {
        match groups.last_mut() {
            Some((_, end)) if i - *end <= 2 * context + 1 => *end = i,
            _ => groups.push((i, i)),
        }
    }
    let mut hunks = Vec::new();
    for (first, last) in groups {
        let start = first.saturating_sub(context);
        let end = (last + context).min(ops.len() - 1);
        let (mut old_start, mut new_start) = (ops[start].1, ops[start].2);
        let (mut old_len, mut new_len) = (0usize, 0usize);
        let mut body = Vec::new();
        for (op, ai, bi) in &ops[start..=end] {
            match op {
                Op::Eq => {
                    old_len += 1;
                    new_len += 1;
                    body.push(format!(" {}", a[*ai]));
                }
                Op::Del => {
                    old_len += 1;
                    body.push(format!("-{}", a[*ai]));
                }
                Op::Ins => {
                    new_len += 1;
                    body.push(format!("+{}", b[*bi]));
                }
            }
        }
        // unified-diff convention: an empty side starts at the line before (0 for an empty file)
        old_start = if old_len == 0 { old_start } else { old_start + 1 };
        new_start = if new_len == 0 { new_start } else { new_start + 1 };
        hunks.push(format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n{}", body.join("\n")));
    }
    hunks
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.split('\n').collect()
}

/// Line ops as `(op, old_pos, new_pos)`: the old/new line index the op sits at. Eq/Del consume
/// `a[old_pos]`, Ins consumes `b[new_pos]`; hunk headers read both positions from the first op.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<(Op, usize, usize)> {
    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<(Op, usize, usize)> = (0..prefix).map(|i| (Op::Eq, i, i)).collect();
    let mut mid: Vec<Op> = Vec::new();
    if am.len().saturating_mul(bm.len()) <= LCS_BUDGET {
        // classic LCS table on the changed middle
        let (n, m) = (am.len(), bm.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] =
                    if am[i] == bm[j] { t[(i + 1) * (m + 1) + j + 1] + 1 } else { t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                mid.push(Op::Eq);
                i += 1;
                j += 1;
            } else if j < m && (i == n || t[i * (m + 1) + j + 1] >= t[(i + 1) * (m + 1) + j]) {
                mid.push(Op::Ins);
                j += 1;
            } else {
                mid.push(Op::Del);
                i += 1;
            }
        }
        // show deletions before insertions inside each changed run
        let mut k = 0;
        while k < mid.len() {
            if mid[k] == Op::Eq {
                k += 1;
                continue;
            }
            let s = k;
            while k < mid.len() && mid[k] != Op::Eq {
                k += 1;
            }
            mid[s..k].sort_by_key(|o| if *o == Op::Del { 0 } else { 1 });
        }
    } else {
        mid.extend(std::iter::repeat_n(Op::Del, am.len()));
        mid.extend(std::iter::repeat_n(Op::Ins, bm.len()));
    }
    let (mut i, mut j) = (prefix, prefix);
    for o in mid {
        match o {
            Op::Eq => {
                ops.push((Op::Eq, i, j));
                i += 1;
                j += 1;
            }
            Op::Del => {
                ops.push((Op::Del, i, j));
                i += 1;
            }
            Op::Ins => {
                ops.push((Op::Ins, i, j));
                j += 1;
            }
        }
    }
    for _ in 0..suffix {
        ops.push((Op::Eq, i, j));
        i += 1;
        j += 1;
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_track_content_and_round_trip_hex() {
        let a = FileVersion::of(b"hello");
        assert_eq!(a, FileVersion::of(b"hello"));
        assert_ne!(a, FileVersion::of(b"hello!"));
        assert_eq!(FileVersion::from_hex(&a.to_hex()), Some(a));
        assert_eq!(FileVersion::from_hex("xyz"), None);
    }

    #[test]
    fn lexical_fence_refuses_climbing_out_and_foreign_absolute_paths() {
        let root = "/ws/proj";
        assert_eq!(lexical_rel(root, "src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(lexical_rel(root, "./src/../b.rs").unwrap(), "b.rs");
        assert_eq!(lexical_rel(root, "/ws/proj/src//a.rs").unwrap(), "src/a.rs");
        assert_eq!(lexical_rel(root, "/ws/proj").unwrap(), ".");
        assert_eq!(lexical_rel(root, "").unwrap(), ".");
        assert!(matches!(lexical_rel(root, "../x"), Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(lexical_rel(root, "src/../../x"), Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(lexical_rel(root, "/ws/proj/../other/x"), Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(lexical_rel(root, "/ws/projection/x"), Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(lexical_rel(root, "/etc/passwd"), Err(FsError::OutsideWorkspace(_))));
    }

    #[test]
    fn hunks_carry_three_lines_of_context_and_correct_headers() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let h = unified_hunks(&old, &new, 3);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0], "@@ -7,7 +7,7 @@\n l7\n l8\n l9\n-l10\n+L10\n l11\n l12\n l13");
    }

    #[test]
    fn distant_changes_split_into_hunks_and_near_ones_merge() {
        let old: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let far = old.replace("l2\n", "X\n").replace("l25\n", "Y\n");
        assert_eq!(unified_hunks(&old, &far, 3).len(), 2);
        let near = old.replace("l10\n", "X\n").replace("l15\n", "Y\n");
        assert_eq!(unified_hunks(&old, &near, 3).len(), 1);
    }

    #[test]
    fn create_and_delete_render_as_single_sided_hunks() {
        assert_eq!(unified_hunks("", "a\nb\n", 3), vec!["@@ -0,0 +1,2 @@\n+a\n+b".to_string()]);
        assert_eq!(unified_hunks("a\n", "", 3), vec!["@@ -1,1 +0,0 @@\n-a".to_string()]);
        assert!(unified_hunks("same\n", "same\n", 3).is_empty());
    }

    #[test]
    fn insertion_in_the_middle_is_minimal() {
        let h = unified_hunks("a\nb\nc\n", "a\nb\nnew\nc\n", 3);
        assert_eq!(h, vec!["@@ -1,3 +1,4 @@\n a\n b\n+new\n c".to_string()]);
    }
}
