//! Checkpoint trees in a shadow bare git repository **on the remote host**: trees are hashed
//! remotely and only the hash crosses the wire.
//!
//! Same semantics as `world-host`'s `ShadowGit`: `<shadow_dir>/<blake3(root) 16 hex>.git`
//! (SHA-256 object format, so a tree id *is* a 32-byte `Hash`), driven with
//! `git --git-dir=<shadow> --work-tree=<root>` and the `GIT_*` environment cleared, so the user's
//! own `.git` is never read or written. `.aho/` is excluded. Restore applies the minimal delta
//! with `read-tree --reset -u` from the tree just recorded, which writes changed files, deletes
//! vanished ones (pruning emptied directories) and leaves ignored paths alone.

use std::sync::Arc;

use aho_fs::{FsError, RestoreDelta, Snapshot};
use aho_types::{BoxFuture, Hash};

use crate::ssh::{Out, Ssh};

/// `$1` root, `$2` git dir. Exit codes: 30 unknown tree, 31/32 cannot create the store, 33 git failed.
const PRELUDE: &str = r#"unset CDPATH GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_CEILING_DIRECTORIES
ROOT=$1; G=$2; shift 2
cd -P -- "$ROOT" || exit 20
aho_git() { git --git-dir="$G" --work-tree="$ROOT" -c core.autocrlf=false -c core.quotePath=false -c core.excludesFile="$ROOT/.aho/ignore" "$@"; }
aho_ensure() {
  [ -f "$G/HEAD" ] && return 0
  mkdir -p -- "${G%/*}" || exit 31
  git init --quiet --bare --object-format=sha256 "$G" >&2 || exit 32
  { mkdir -p -- "$G/info" && printf '.aho/\n' > "$G/info/exclude"; } || exit 31
}
aho_ensure
"#;

const TREE: &str = r#"aho_git add --all >&2 || exit 33
aho_git write-tree || exit 33
"#;

const DIFF: &str = r#"aho_git diff-tree -r -z --no-renames --name-status "$1" "$2" || exit 33
"#;

/// stdout = `diff-tree -z` from the current tree to `$1` (the delta), then the switch.
const RESTORE: &str = r#"[ "$(aho_git cat-file -t "$1" 2>/dev/null)" = tree ] || exit 30
aho_git add --all >&2 || exit 33
_now=$(aho_git write-tree) || exit 33
aho_git diff-tree -r -z --no-renames --name-status "$_now" "$1" || exit 33
aho_git read-tree --reset -u "$1" >&2 || exit 33
aho_git update-index -q --refresh >/dev/null 2>&1
exit 0
"#;

#[derive(Clone)]
pub struct RemoteShadowGit {
    core: Arc<Core>,
}

struct Core {
    ssh: Arc<Ssh>,
    root: String,
    git_dir: String,
    /// One git operation at a time: they share the shadow index.
    lock: tokio::sync::Mutex<()>,
}

impl RemoteShadowGit {
    /// `root` is the canonical remote root; `shadow_home` the remote directory holding shadow repos.
    pub fn new(ssh: Arc<Ssh>, root: &str, shadow_home: &str) -> RemoteShadowGit {
        let id = Hash::of(root.as_bytes()).to_hex();
        let git_dir = format!("{}/{}.git", shadow_home.trim_end_matches('/'), &id[..16]);
        RemoteShadowGit { core: Arc::new(Core { ssh, root: root.to_string(), git_dir, lock: tokio::sync::Mutex::new(()) }) }
    }

    /// The shadow repository's path on the remote.
    pub fn git_dir(&self) -> &str {
        &self.core.git_dir
    }
}

impl Snapshot for RemoteShadowGit {
    fn tree(&self) -> BoxFuture<'_, Result<Hash, FsError>> {
        Box::pin(async move {
            let _g = self.core.lock.lock().await;
            let out = self.core.run(TREE, &[]).await?;
            let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Hash::from_hex(&hex).ok_or_else(|| {
                FsError::Io(format!(
                    "the shadow store {} on {} returned tree id `{hex}`, not a SHA-256 id; delete that directory there so it is re-created.",
                    self.core.git_dir,
                    self.core.host()
                ))
            })
        })
    }

    fn restore(&self, tree: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        Box::pin(async move {
            let _g = self.core.lock.lock().await;
            let out = self.core.run(RESTORE, &[&tree.to_hex()]).await.map_err(|e| match e {
                FsError::NotFound(_) => FsError::Invalid(format!(
                    "checkpoint tree {} is not in the shadow store {} on {}; nothing was restored.",
                    tree.short(),
                    self.core.git_dir,
                    self.core.host()
                )),
                e => e,
            })?;
            Ok(parse_delta(&out.stdout))
        })
    }

    fn diff(&self, a: Hash, b: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        Box::pin(async move {
            let _g = self.core.lock.lock().await;
            let out = self.core.run(DIFF, &[&a.to_hex(), &b.to_hex()]).await?;
            Ok(parse_delta(&out.stdout))
        })
    }
}

impl Core {
    fn host(&self) -> String {
        self.ssh.target().display()
    }

    /// Runs a snapshot script; exit 30 comes back as `NotFound` for `restore` to reword.
    async fn run(&self, body: &str, args: &[&str]) -> Result<Out, FsError> {
        let script = format!("{PRELUDE}{body}");
        let mut all = vec![self.root.as_str(), self.git_dir.as_str()];
        all.extend_from_slice(args);
        let out = self.ssh.run(&script, &all, None).await.map_err(|e| FsError::Io(e.to_string()))?;
        let host = self.host();
        let err = out.stderr.trim();
        match out.code {
            0 => Ok(out),
            30 => Err(FsError::NotFound(String::new())),
            31 | 32 => Err(FsError::Io(format!(
                "could not create the shadow store {} on {host} (needs git ≥ 2.29 there for SHA-256): {err}; nothing was recorded.",
                self.git_dir
            ))),
            20 => Err(FsError::Io(format!("the workspace {} is gone on {host}: {err}; nothing was recorded.", self.root))),
            code => Err(FsError::Io(format!(
                "git failed in the shadow store {} on {host} (exit {code}): {err}; nothing was recorded or restored.",
                self.git_dir
            ))),
        }
    }
}

/// `diff-tree -z --name-status` → sorted changed / deleted paths.
fn parse_delta(out: &[u8]) -> RestoreDelta {
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
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_status_listing_splits_into_changed_and_deleted() {
        let d = parse_delta(b"M\0b.txt\0D\0gone.txt\0A\0a b.txt\0");
        assert_eq!(d.changed, vec!["a b.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(d.deleted, vec!["gone.txt".to_string()]);
        assert_eq!(parse_delta(b""), RestoreDelta::default());
    }
}
