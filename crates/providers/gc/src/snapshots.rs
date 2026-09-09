//! Checkpoint trees (11: "a tree is reachable while any log branch references it; `aho gc`
//! prunes the rest").
//!
//! world-host keeps one bare SHA-256 shadow repository per workspace root,
//! `<workspace.shadow_dir>/<blake3(root)[..16]>.git`, and writes checkpoint trees with
//! `git write-tree`: no commits, no refs. So git's own reachability protects nothing, and
//! `git prune` would drop every checkpoint. gc computes reachability itself: the roots are the
//! trees no other tree names; a root no `Workspace/Checkpoint` references is an orphan, and
//! reclaiming it deletes the loose objects only orphans reach — never an object reachable from a
//! referenced tree, from a root that stays, or from the shadow index (the next checkpoint's
//! `git add` reuses those).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use aho_types::{Hash, Timestamp};

use crate::logs::Sessions;
use crate::{GcReport, Kept, Orphan, OrphanKind, ScanInputs, Target, age_of, git};

/// One shadow repository's object graph.
pub(crate) struct Repo {
    path: PathBuf,
    /// Every tree, with the trees and blobs it names.
    trees: HashMap<String, Vec<String>>,
    /// Blobs the shadow index holds (the next `git add` reuses them).
    index: HashSet<String>,
}

impl Repo {
    #[allow(clippy::disallowed_methods, reason = "authority: gc-snapshot-index")]
    pub(crate) fn load(path: &Path) -> Result<Repo, String> {
        let dir = path.as_os_str();
        let what = format!("list the objects of {}", path.display());
        let out = git::run(
            [std::ffi::OsStr::new("--git-dir"), dir, "cat-file".as_ref(), "--batch-all-objects".as_ref(), "--batch-check".as_ref()],
            &what,
        )?;
        let mut tree_ids = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut w = line.split_whitespace();
            match (w.next(), w.next()) {
                (Some(oid), Some("tree")) => tree_ids.push(oid.to_string()),
                (Some(_), Some("blob")) => {}
                (Some(oid), Some(other)) => {
                    return Err(format!(
                        "{} holds a {other} ({oid}); aho's shadow repositories hold only trees and blobs, so gc leaves this one alone",
                        path.display()
                    ));
                }
                _ => {}
            }
        }
        let trees = read_trees(path, &tree_ids)?;
        let mut index = HashSet::new();
        if path.join("index").is_file() {
            let out = git::run([std::ffi::OsStr::new("--git-dir"), dir, "ls-files".as_ref(), "--stage".as_ref()], "read the shadow index")?;
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(oid) = line.split_whitespace().nth(1) {
                    index.insert(oid.to_string());
                }
            }
        }
        Ok(Repo { path: path.to_path_buf(), trees, index })
    }

    fn loose(&self, oid: &str) -> PathBuf {
        let (fan, rest) = oid.split_at(2.min(oid.len()));
        self.path.join("objects").join(fan).join(rest)
    }

    /// Trees no other tree names: what `git write-tree` produced for a whole workspace.
    fn roots(&self) -> Vec<String> {
        let named: HashSet<&String> = self.trees.values().flatten().collect();
        let mut v: Vec<String> = self.trees.keys().filter(|t| !named.contains(t)).cloned().collect();
        v.sort();
        v
    }

    fn closure<'a>(&self, starts: impl IntoIterator<Item = &'a String>) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack: Vec<&String> = starts.into_iter().collect();
        while let Some(o) = stack.pop() {
            if !seen.insert(o.clone()) {
                continue;
            }
            if let Some(kids) = self.trees.get(o) {
                stack.extend(kids.iter());
            }
        }
        seen
    }

    /// What must survive: everything referenced trees and staying roots reach, plus the index.
    fn keep(&self, referenced: &HashSet<String>, staying: &[&String]) -> HashSet<String> {
        let starts = self.trees.keys().filter(|t| referenced.contains(*t)).chain(staying.iter().copied());
        let mut k = self.closure(starts);
        k.extend(self.index.iter().cloned());
        k
    }

    /// The loose files of `objects` with their sizes on disk (packed objects are not ours to cut).
    #[allow(clippy::disallowed_methods, reason = "authority: gc-snapshot-object-size")]
    fn loose_files(&self, objects: &HashSet<String>) -> Vec<(PathBuf, u64)> {
        let mut v: Vec<(PathBuf, u64)> = objects
            .iter()
            .filter_map(|o| {
                let p = self.loose(o);
                std::fs::metadata(&p).ok().map(|m| (p, m.len()))
            })
            .collect();
        v.sort();
        v
    }

    /// Age of a loose root (git refreshes the mtime whenever a write finds the object present).
    fn age(&self, oid: &str, now: Timestamp) -> Option<u64> {
        age_of(&self.loose(oid), now)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `git cat-file --batch` over every tree: `<oid> tree <size>\n<raw>\n` per object.
#[allow(clippy::disallowed_methods, reason = "authority: gc-snapshot-read-trees")]
fn read_trees(path: &Path, ids: &[String]) -> Result<HashMap<String, Vec<String>>, String> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let fail = |e: std::io::Error| format!("could not read the trees of {} ({e})", path.display());
    let mut child = git::command([std::ffi::OsStr::new("--git-dir"), path.as_os_str(), "cat-file".as_ref(), "--batch".as_ref()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(fail)?;
    let mut stdin = child.stdin.take().ok_or_else(|| format!("git cat-file on {} has no stdin", path.display()))?;
    let input = format!("{}\n", ids.join("\n"));
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let res = child.wait_with_output().map_err(fail)?;
    writer.join().map_err(|_| format!("the writer feeding git cat-file on {} panicked", path.display()))?.map_err(fail)?;
    if !res.status.success() {
        return Err(format!("git could not read the trees of {}: {}", path.display(), String::from_utf8_lossy(&res.stderr).trim()));
    }
    let buf = res.stdout;
    let bad = || format!("git cat-file printed something that is not a tree listing for {}", path.display());
    let mut i = 0;
    while i < buf.len() {
        let nl = buf[i..].iter().position(|b| *b == b'\n').ok_or_else(bad)? + i;
        let header = String::from_utf8_lossy(&buf[i..nl]).into_owned();
        i = nl + 1;
        let mut w = header.split_whitespace();
        let (Some(oid), Some(kind)) = (w.next(), w.next()) else { return Err(bad()) };
        if kind == "missing" {
            continue;
        }
        let size: usize = w.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let body = buf.get(i..i + size).ok_or_else(bad)?;
        i += size + 1;
        if kind == "tree" {
            out.insert(oid.to_string(), parse_tree(body, oid.len() / 2).ok_or_else(bad)?);
        }
    }
    Ok(out)
}

/// Raw tree entries `<mode> <name>\0<oid bytes>`; gitlinks (another repository's commits) are skipped.
fn parse_tree(body: &[u8], hash_len: usize) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let sp = body[i..].iter().position(|b| *b == b' ')? + i;
        let mode = &body[i..sp];
        let nul = body[sp..].iter().position(|b| *b == 0)? + sp;
        let raw = body.get(nul + 1..nul + 1 + hash_len)?;
        i = nul + 1 + hash_len;
        if mode != b"160000" {
            out.push(hex(raw));
        }
    }
    Some(out)
}

/// The shadow repositories of this project's workspace roots: the project itself and each
/// worktree (existing, or named by a session log), under every shadow dir in use.
#[allow(clippy::disallowed_methods, reason = "authority: gc-snapshot-repositories")]
fn repos(inputs: &ScanInputs, sessions: &Sessions, report: &mut GcReport) -> Vec<PathBuf> {
    let base = match std::fs::canonicalize(&inputs.project) {
        Ok(b) => b,
        Err(e) => {
            report.notes.push(format!("snapshot trees were not judged: the project {} cannot be opened ({e})", inputs.project.display()));
            return Vec::new();
        }
    };
    let wt_dir = base.join(".aho/worktrees");
    let mut roots = vec![base.clone()];
    let mut names: Vec<String> = sessions.logs.iter().filter_map(|l| l.worktree()).collect();
    if let Ok(entries) = std::fs::read_dir(&wt_dir) {
        names.extend(entries.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()));
    }
    names.sort();
    names.dedup();
    for n in names {
        let p = wt_dir.join(&n);
        roots.push(std::fs::canonicalize(&p).unwrap_or(p));
    }
    let mut shadows = vec![inputs.shadow_dir.clone()];
    shadows.extend(sessions.logs.iter().filter_map(|l| l.config("workspace.shadow_dir")).map(PathBuf::from));
    shadows.sort();
    shadows.dedup();
    let mut out = Vec::new();
    for s in &shadows {
        for r in &roots {
            let id = Hash::of(r.to_string_lossy().as_bytes()).to_hex();
            let repo = s.join(format!("{}.git", &id[..16]));
            if repo.is_dir() && !out.contains(&repo) {
                out.push(repo);
            }
        }
    }
    out
}

pub(crate) fn judge(inputs: &ScanInputs, sessions: &Sessions, referenced: &HashSet<String>, report: &mut GcReport) {
    if !sessions.unreadable.is_empty() {
        report.notes.push(format!(
            "snapshot trees were not judged: {} session log(s) could not be read, and any of them may reference a checkpoint",
            sessions.unreadable.len()
        ));
        return;
    }
    for path in repos(inputs, sessions, report) {
        let repo = match Repo::load(&path) {
            Ok(r) => r,
            Err(e) => {
                report.notes.push(e);
                continue;
            }
        };
        let roots = repo.roots();
        let mut listed: Vec<(&String, u64)> = Vec::new();
        for r in roots.iter().filter(|r| !referenced.contains(*r)) {
            let shown = format!("{} tree {}", path.display(), &r[..12.min(r.len())]);
            match repo.age(r, inputs.now) {
                None => report.kept.push(Kept {
                    kind: OrphanKind::Snapshot,
                    path: shown,
                    why: "stored in a pack; gc removes only loose objects".into(),
                }),
                Some(age) if age < inputs.grace_ms => report.kept.push(Kept {
                    kind: OrphanKind::Snapshot,
                    path: shown,
                    why: "written within the grace period (a checkpoint may be on its way into a log)".into(),
                }),
                Some(age) => listed.push((r, age)),
            }
        }
        let staying: Vec<&String> = roots.iter().filter(|r| !listed.iter().any(|(l, _)| l == r)).collect();
        let keep = repo.keep(referenced, &staying);
        for (root, age) in listed {
            let only: HashSet<String> = repo.closure([root]).into_iter().filter(|o| !keep.contains(o)).collect();
            let size = repo.loose_files(&only).iter().map(|(_, n)| n).sum();
            report.items.push(Orphan {
                owner: "-".into(),
                kind: OrphanKind::Snapshot,
                age_ms: Some(age),
                size: Some(size),
                path: format!("{} tree {}", path.display(), &root[..12.min(root.len())]),
                target: Target::Snapshot { repo: path.clone(), tree: root.clone() },
            });
        }
    }
}

/// Deletes the loose objects only `roots` reach, after checking again that each is still an
/// unreferenced root older than the grace period. Returns `(deleted roots, kept roots with why, bytes)`.
#[allow(clippy::type_complexity)]
#[allow(clippy::disallowed_methods, reason = "authority: gc-snapshot-delete")]
pub(crate) fn delete(
    path: &Path,
    roots: &[String],
    referenced: &HashSet<String>,
    now: Timestamp,
    grace_ms: u64,
) -> Result<(Vec<String>, Vec<(String, String)>, u64), String> {
    let repo = Repo::load(path)?;
    let all = repo.roots();
    let mut go = Vec::new();
    let mut kept = Vec::new();
    for r in roots {
        if referenced.contains(r) {
            kept.push((r.clone(), "a session log references it now".into()));
        } else if !all.contains(r) {
            kept.push((r.clone(), "it is no longer a root tree of the repository".into()));
        } else {
            match repo.age(r, now) {
                Some(age) if age >= grace_ms => go.push(r.clone()),
                Some(_) => kept.push((r.clone(), "it was written again within the grace period".into())),
                None => kept.push((r.clone(), "it is stored in a pack".into())),
            }
        }
    }
    let staying: Vec<&String> = all.iter().filter(|r| !go.contains(r)).collect();
    let keep = repo.keep(referenced, &staying);
    let victims: HashSet<String> = repo.closure(go.iter()).into_iter().filter(|o| !keep.contains(o)).collect();
    let mut freed = 0;
    for (file, n) in repo.loose_files(&victims) {
        std::fs::remove_file(&file).map_err(|e| {
            format!(
                "could not remove {} ({e}); {freed} bytes were freed before it, the remaining objects of these trees are intact",
                file.display()
            )
        })?;
        freed += n;
    }
    Ok((go, kept, freed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_tree_entries_parse_and_gitlinks_are_skipped() {
        let mut body = b"100644 a.txt\0".to_vec();
        body.extend([0xab; 32]);
        body.extend(b"160000 sub\0");
        body.extend([0x01; 32]);
        body.extend(b"40000 d\0");
        body.extend([0xcd; 32]);
        let kids = parse_tree(&body, 32).unwrap();
        assert_eq!(kids, vec!["ab".repeat(32), "cd".repeat(32)]);
        assert_eq!(parse_tree(b"100644 truncated\0\x01\x02", 32), None);
    }
}
