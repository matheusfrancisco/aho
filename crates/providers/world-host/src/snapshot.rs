//! Checkpoint trees in a shadow bare git repository.
//!
//! `~/.aho/shadow/<blake3(root) 16 hex>.git` (SHA-256 object format, so a tree id *is* a 32-byte
//! `Hash`), driven with `git --git-dir=<shadow> --work-tree=<root>`. The user's own `.git` is
//! never read or written: git never tracks a `.git` directory, and every invocation names the
//! shadow repo explicitly with the `GIT_*` environment cleared.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use aho_fs::{FsError, RestoreDelta, Snapshot};
use aho_types::{BoxFuture, Hash};

#[derive(Clone)]
pub struct ShadowGit {
    core: Arc<Core>,
}

struct Core {
    root: PathBuf,
    git_dir: PathBuf,
    /// One git operation at a time: they share the shadow index.
    lock: Mutex<()>,
}

impl ShadowGit {
    /// `root` should be canonical; `shadow_home` is the directory holding all shadow repos.
    pub fn new(root: &Path, shadow_home: &Path) -> ShadowGit {
        let id = Hash::of(root.to_string_lossy().as_bytes()).to_hex();
        let git_dir = shadow_home.join(format!("{}.git", &id[..16]));
        ShadowGit { core: Arc::new(Core { root: root.to_path_buf(), git_dir, lock: Mutex::new(()) }) }
    }

    pub fn git_dir(&self) -> &Path {
        &self.core.git_dir
    }

    fn off_thread<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Core) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'static, Result<T, FsError>> {
        let core = self.core.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || {
                let _g = core.lock.lock().unwrap_or_else(|p| p.into_inner());
                f(&core)
            })
            .await
            {
                Ok(r) => r,
                Err(e) => Err(FsError::Io(format!("the snapshot worker stopped unexpectedly ({e}); nothing was recorded or restored."))),
            }
        })
    }
}

impl Snapshot for ShadowGit {
    fn tree(&self) -> BoxFuture<'_, Result<Hash, FsError>> {
        self.off_thread(|c| c.tree())
    }
    fn restore(&self, tree: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        self.off_thread(move |c| c.restore(tree))
    }
    fn diff(&self, a: Hash, b: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        self.off_thread(move |c| {
            c.ensure()?;
            c.diff(a, b)
        })
    }
}

fn clean_env(cmd: &mut Command) -> &mut Command {
    for k in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

impl Core {
    #[allow(clippy::disallowed_methods, reason = "authority: world-host-snapshot-git")]
    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, FsError> {
        let mut cmd = Command::new("git");
        clean_env(&mut cmd)
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.root.display()))
            .args(["-c", "core.autocrlf=false", "-c", "core.quotePath=false"])
            .arg("-c")
            .arg(format!("core.excludesFile={}", self.root.join(".aho/ignore").display()))
            .args(args)
            .current_dir(&self.root)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            FsError::Io(format!("could not run git for checkpoints ({e}); nothing was recorded. Install git or unset the snapshot seam."))
        })?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(bytes).map_err(|e| FsError::Io(format!("could not feed git: {e}")))?;
        }
        let out = child.wait_with_output().map_err(|e| FsError::Io(format!("git did not finish: {e}")))?;
        if !out.status.success() {
            return Err(FsError::Io(format!(
                "`git {}` failed in the shadow store {}: {}",
                args.first().copied().unwrap_or(""),
                self.git_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-snapshot-store")]
    fn ensure(&self) -> Result<(), FsError> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        let parent = self.git_dir.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|e| FsError::Io(format!("could not create the shadow store at {}: {e}", parent.display())))?;
        let out = clean_env(&mut Command::new("git"))
            .args(["init", "--quiet", "--bare", "--object-format=sha256"])
            .arg(&self.git_dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| FsError::Io(format!("could not run git to create the shadow store ({e}). Install git ≥ 2.29.")))?;
        if !out.status.success() {
            return Err(FsError::Io(format!(
                "could not create the shadow store at {} (needs git ≥ 2.29 for SHA-256): {}",
                self.git_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // The session log, worktrees and ignore file live under `.aho/`; they are not workspace content.
        let info = self.git_dir.join("info");
        std::fs::create_dir_all(&info)
            .and_then(|_| std::fs::write(info.join("exclude"), ".aho/\n"))
            .map_err(|e| FsError::Io(format!("could not write the shadow store's exclude file: {e}")))
    }

    fn tree(&self) -> Result<Hash, FsError> {
        self.ensure()?;
        self.git(&["add", "--all"], None)?;
        let out = self.git(&["write-tree"], None)?;
        let hex = String::from_utf8_lossy(&out).trim().to_string();
        Hash::from_hex(&hex).ok_or_else(|| {
            FsError::Io(format!(
                "the shadow store at {} returned tree id `{hex}`, not a SHA-256 id; delete that directory so it is re-created.",
                self.git_dir.display()
            ))
        })
    }

    fn diff(&self, a: Hash, b: Hash) -> Result<RestoreDelta, FsError> {
        let out = self.git(&["diff-tree", "-r", "-z", "--no-renames", "--name-status", &a.to_hex(), &b.to_hex()], None)?;
        let mut delta = RestoreDelta::default();
        let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            let path = String::from_utf8_lossy(path).into_owned();
            match status.first() {
                Some(b'D') => delta.deleted.push(path),
                Some(_) => delta.changed.push(path),
                None => {}
            }
        }
        delta.changed.sort();
        delta.deleted.sort();
        Ok(delta)
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-snapshot-restore")]
    fn restore(&self, target: Hash) -> Result<RestoreDelta, FsError> {
        self.ensure()?;
        let hex = target.to_hex();
        let kind = self.git(&["cat-file", "-t", &hex], None).map(|o| String::from_utf8_lossy(&o).trim().to_string());
        if kind.as_deref().ok() != Some("tree") {
            return Err(FsError::Invalid(format!(
                "checkpoint tree {} is not in the shadow store at {}; nothing was restored.",
                target.short(),
                self.git_dir.display()
            )));
        }
        let now = self.tree()?;
        let delta = self.diff(now, target)?;
        self.git(&["read-tree", &hex], None)?;
        if !delta.changed.is_empty() {
            let mut list = Vec::new();
            for p in &delta.changed {
                list.extend_from_slice(p.as_bytes());
                list.push(0);
            }
            self.git(&["checkout-index", "--force", "-z", "--stdin"], Some(&list))?;
        }
        for p in &delta.deleted {
            let path = self.root.join(p);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if !m.is_dir() => {
                    std::fs::remove_file(&path).map_err(|e| FsError::Io(format!("could not delete `{p}` while restoring: {e}")))?;
                    prune_empty_parents(&self.root, &path);
                }
                _ => {}
            }
        }
        // Refresh stat info so the next checkpoint only hashes what changes afterwards.
        let _ = self.git(&["update-index", "-q", "--refresh"], None);
        Ok(delta)
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: world-host-snapshot-prune")]
fn prune_empty_parents(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || std::fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::HostFs;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    #[tokio::test]
    async fn tree_restore_round_trip_leaves_the_users_git_alone() {
        if !git_available() {
            eprintln!("skipping: `git` is not on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        let init = Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap();
        assert!(init.success());
        let user_git_before = std::fs::read_dir(root.join(".git")).unwrap().count();

        let fs = HostFs::new(&root).unwrap();
        let snap = ShadowGit::new(&root, home.path());
        let results = aho_sandbox::run_snapshot_conformance(&fs, &snap).await;
        for (name, r) in &results {
            assert!(r.is_ok(), "{name}: {r:?}");
        }
        assert!(snap.git_dir().starts_with(home.path()));
        assert!(!root.join(".git/index").exists(), "the user's index was never written");
        assert_eq!(std::fs::read_dir(root.join(".git")).unwrap().count(), user_git_before);
        let status = Command::new("git").args(["status", "--porcelain"]).current_dir(&root).output().unwrap();
        assert!(status.status.success());
    }

    #[tokio::test]
    async fn restore_deletes_files_added_since_and_prunes_their_dirs() {
        if !git_available() {
            eprintln!("skipping: `git` is not on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        std::fs::write(root.join("keep.txt"), "k").unwrap();
        std::fs::create_dir_all(root.join(".aho/sessions")).unwrap();
        std::fs::write(root.join(".aho/sessions/log.jsonl"), "{}").unwrap();
        let snap = ShadowGit::new(&root, home.path());
        let t0 = snap.tree().await.unwrap();
        std::fs::create_dir_all(root.join("new/deep")).unwrap();
        std::fs::write(root.join("new/deep/x.txt"), "x").unwrap();
        std::fs::write(root.join(".aho/sessions/log.jsonl"), "{}\n{}").unwrap();
        let d = snap.restore(t0).await.unwrap();
        assert_eq!(d.deleted, vec!["new/deep/x.txt".to_string()]);
        assert!(d.changed.is_empty(), "{d:?}");
        assert!(!root.join("new").exists(), "emptied directories are pruned");
        assert_eq!(
            std::fs::read_to_string(root.join(".aho/sessions/log.jsonl")).unwrap(),
            "{}\n{}",
            ".aho/ is never snapshotted or restored"
        );
        assert!(matches!(snap.restore(Hash::of(b"nope")).await, Err(FsError::Invalid(_))));
    }
}
