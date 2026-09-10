//! `07` — the host world: `Fs + Subprocess + Sandbox + Snapshot + Egress` on your own disk and
//! network, as your user. Nothing is confined; `aho plan` prints `enforced by: policy only`, and
//! that honesty is the point. The workspace is the configured directory, or a git worktree on branch
//! `aho/<name>` under `.aho/worktrees/<name>` with `workspace.layout = worktree`. Egress is the host
//! network (`HostEgress`); the operator's `egress.allow`/`egress.deny` list is enforced per hop for
//! the tools that use it (`web_fetch`, MCP over HTTP) — `bash` can still reach anything.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![deny(unsafe_code)] // one audited exception: `proc::signal_group` (kill(2) on a process group)

pub mod egress;
#[allow(clippy::disallowed_methods, reason = "authority: world-host-fs")]
pub mod fs;
pub mod proc;
pub mod sandbox;
pub mod snapshot;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_fs::{FsRead, FsWrite, SnapshotHandle};
use aho_proc::SubprocessHandle;
use aho_sandbox::SandboxHandle;
use serde_json::json;

pub use crate::egress::HostEgress;
pub use crate::fs::HostFs;
pub use crate::proc::HostSubprocess;
pub use crate::sandbox::HostSandbox;
pub use crate::snapshot::ShadowGit;
use aho_egress::EgressHandle;

pub const ENFORCED_BY: &str = "policy only";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Work in `workspace.root` itself, like other harnesses.
    InPlace,
    /// Work in `.aho/worktrees/<workspace.worktree>` on branch `aho/<name>`.
    Worktree,
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub root: PathBuf,
    pub layout: Layout,
    pub worktree: Option<String>,
    /// Directory holding shadow repos (11).
    pub shadow_dir: PathBuf,
    /// Operator login shell for `aho shell`.
    pub shell: String,
}

impl HostConfig {
    /// In-place layout at `root`, shadow repos under `shadow_dir`.
    pub fn in_place(root: impl Into<PathBuf>, shadow_dir: impl Into<PathBuf>) -> HostConfig {
        HostConfig { root: root.into(), layout: Layout::InPlace, worktree: None, shadow_dir: shadow_dir.into(), shell: "/bin/sh".into() }
    }
}

fn valid_worktree_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && !n.contains("..")
        && n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Config for HostConfig {
    const PREFIX: &'static str = "workspace";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "workspace.root",
                Ty::String,
                None,
                "Workspace root of the host world; default: the directory aho was started in.",
            ),
            ConfigKey::new(
                "workspace.layout",
                Ty::String,
                Some(json!("in-place")),
                "`in-place` works in workspace.root; `worktree` works in `.aho/worktrees/<workspace.worktree>` on branch `aho/<name>` (07).",
            ),
            ConfigKey::new(
                "workspace.worktree",
                Ty::String,
                None,
                "Worktree name for `workspace.layout = worktree` (letters, digits, `.`, `_`, `-`).",
            ),
            ConfigKey::new("workspace.shadow_dir", Ty::String, None, "Where checkpoint trees are stored (11); default: `~/.aho/shadow`."),
        ]
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-config")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let root = match l.str("workspace.root") {
            Some(r) if !r.is_empty() => PathBuf::from(r),
            _ => std::env::current_dir().map_err(|_| ConfigError::MissingKey("workspace.root".into()))?,
        };
        let layout = match l.str("workspace.layout").unwrap_or("in-place") {
            "in-place" => Layout::InPlace,
            "worktree" => Layout::Worktree,
            _ => return Err(ConfigError::Type { key: "workspace.layout".into(), expected: "`in-place` or `worktree`" }),
        };
        let worktree = l.str("workspace.worktree").filter(|s| !s.is_empty()).map(str::to_string);
        if layout == Layout::Worktree {
            match &worktree {
                None => return Err(ConfigError::MissingKey("workspace.worktree".into())),
                Some(n) if !valid_worktree_name(n) => {
                    return Err(ConfigError::Type {
                        key: "workspace.worktree".into(),
                        expected: "a name of letters, digits, `.`, `_`, `-`",
                    });
                }
                Some(_) => {}
            }
        }
        let shadow_dir = match l.str("workspace.shadow_dir") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(".aho/shadow"),
                None => root.join(".aho/shadow"),
            },
        };
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        Ok(HostConfig { root, layout, worktree, shadow_dir, shell })
    }
}

pub struct HostWorld {
    root: PathBuf,
    fs: HostFs,
    proc: Arc<HostSubprocess>,
    sandbox: Arc<HostSandbox>,
    snapshot: ShadowGit,
    egress: HostEgress,
}

impl HostWorld {
    /// Canonical workspace root every tool resolves against.
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn fs(&self) -> &HostFs {
        &self.fs
    }
    pub fn subprocess(&self) -> Arc<HostSubprocess> {
        self.proc.clone()
    }
    pub fn sandbox(&self) -> Arc<HostSandbox> {
        self.sandbox.clone()
    }
    pub fn snapshot(&self) -> &ShadowGit {
        &self.snapshot
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: world-host-worktree-git")]
fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, BuildError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| BuildError::Probe(format!("could not run git ({e}); install git or set workspace.layout = in-place")))
}

/// `git worktree add .aho/worktrees/<name> -b aho/<name>`, idempotently.
#[allow(clippy::disallowed_methods, reason = "authority: world-host-worktree-root")]
fn ensure_worktree(base: &Path, name: &str) -> Result<PathBuf, BuildError> {
    let path = base.join(".aho/worktrees").join(name);
    if path.join(".git").exists() {
        return std::fs::canonicalize(&path).map_err(|e| BuildError::Probe(format!("worktree {} cannot be opened: {e}", path.display())));
    }
    if !git_in(base, &["rev-parse", "--git-dir"])?.status.success() {
        return Err(BuildError::Probe(format!(
            "`workspace.layout = worktree` needs a git repository at {}; run `git init` there or set workspace.layout = in-place",
            base.display()
        )));
    }
    let _ = git_in(base, &["worktree", "prune"])?;
    let branch = format!("aho/{name}");
    let exists = git_in(base, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])?.status.success();
    let target = path.to_string_lossy().into_owned();
    let add: Vec<&str> = if exists { vec!["worktree", "add", &target, &branch] } else { vec!["worktree", "add", &target, "-b", &branch] };
    let out = git_in(base, &add)?;
    if !out.status.success() {
        return Err(BuildError::Probe(format!(
            "`git worktree add {target}` failed: {}. A worktree needs at least one commit; or set workspace.layout = in-place",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    std::fs::canonicalize(&path).map_err(|e| BuildError::Probe(format!("worktree {} cannot be opened: {e}", path.display())))
}

impl Capability for HostWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = HostConfig;

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-build-root")]
    fn build(_: (), cfg: HostConfig) -> Result<Self, BuildError> {
        let base = std::fs::canonicalize(&cfg.root)
            .map_err(|e| BuildError::Probe(format!("workspace root {} cannot be opened ({e}); set workspace.root", cfg.root.display())))?;
        let root = match (cfg.layout, &cfg.worktree) {
            (Layout::Worktree, Some(name)) => ensure_worktree(&base, name)?,
            (Layout::Worktree, None) => return Err(BuildError::Config(ConfigError::MissingKey("workspace.worktree".into()))),
            (Layout::InPlace, _) => base,
        };
        let fs = HostFs::new(&root).map_err(BuildError::Probe)?;
        let proc = Arc::new(HostSubprocess::new(&root));
        let sandbox = Arc::new(HostSandbox::new(root.to_string_lossy().into_owned(), proc.clone(), cfg.shell.clone()));
        let snapshot = ShadowGit::new(&root, &cfg.shadow_dir);
        let egress = HostEgress::new().map_err(BuildError::Probe)?;
        Ok(HostWorld { root, fs, proc, sandbox, snapshot, egress })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let fs: Arc<dyn aho_fs::Fs> = Arc::new(self.fs.clone());
        r.provide(FsRead(fs.clone()));
        r.provide(FsWrite(fs));
        r.provide(SubprocessHandle(self.proc.clone()));
        r.provide(SnapshotHandle(Arc::new(self.snapshot.clone())));
        r.provide(SandboxHandle(self.sandbox.clone()));
        r.provide(EgressHandle(Arc::new(self.egress.clone())));
    }
}

declare_capability!(
    HostWorld,
    "world-host",
    kind = Kind::Provider,
    provides = [aho_fs::SEAM, aho_proc::SEAM, aho_sandbox::SEAM, aho_fs::SNAPSHOT_SEAM, aho_egress::SEAM],
    requires = [Requirement::Binary("git")],
    tools = &[],
    provides_handles = [
        HandleKind::FsRead,
        HandleKind::FsWrite,
        HandleKind::Subprocess,
        HandleKind::Snapshot,
        HandleKind::Custom("sandbox"),
        HandleKind::Egress(Route::Sandbox)
    ],
    exclusive = true,
    unlock = Fix::cli("./install-system-deps.sh"),
    enforced_by = ENFORCED_BY,
    doc = "Host world: tools run as your user in the workspace directory (or a git worktree); checkpoints in a shadow git store. Nothing is confined."
);

#[cfg(test)]
mod tests {
    use super::*;
    use aho_config::{ResolveInputs, Schema, resolve};

    fn world() -> (tempfile::TempDir, tempfile::TempDir, HostWorld) {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let w = HostWorld::build((), HostConfig::in_place(ws.path(), home.path())).unwrap();
        (ws, home, w)
    }

    macro_rules! conformance {
        ($($case:ident),* $(,)?) => {$(
            #[tokio::test]
            async fn $case() {
                let (_ws, _home, w) = world();
                let r = aho_sandbox::run_case(stringify!($case), w.fs(), w.subprocess().as_ref(), w.sandbox().as_ref()).await;
                assert!(r.is_ok(), "{}: {r:?}", stringify!($case));
            }
        )*};
    }

    // One named test per `aho_sandbox::CONFORMANCE` case; `the_whole_suite_passes_and_leaves_no_scratch`
    // runs the list itself, so a case added there is exercised even before it gets a name here.
    conformance!(
        probe_succeeds,
        bound_world_reports_its_enforcement_honestly,
        writable_roots_start_with_the_workspace_root,
        exit_status_passes_through_including_128_plus_signal,
        a_waited_read_returns_when_the_process_ends_not_when_its_descendant_does,
        killing_a_child_kills_its_whole_process_group,
        a_timeout_kills_the_whole_process_group,
        fs_fence_refuses_a_path_outside_the_workspace,
        fs_fence_refuses_a_symlink_pointing_outside,
        fs_fence_refuses_a_dangling_symlink,
        write_is_atomic_and_versioned,
        edit_refuses_a_non_unique_match_and_a_stale_version,
        glob_and_grep_honor_gitignore,
    );

    #[tokio::test]
    async fn the_whole_suite_passes_and_leaves_no_scratch() {
        let (ws, _home, w) = world();
        let results = aho_sandbox::run_conformance(w.fs(), w.subprocess().as_ref(), w.sandbox().as_ref()).await;
        assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
        assert_eq!(std::fs::read_dir(ws.path()).unwrap().count(), 0);
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    #[test]
    fn worktree_layout_is_idempotent_and_survives_a_deleted_directory() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: `git` is not on PATH");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        let cfg = HostConfig { layout: Layout::Worktree, worktree: Some("s1".into()), ..HostConfig::in_place(repo.path(), home.path()) };

        let a = HostWorld::build((), cfg.clone()).unwrap();
        assert!(a.root().ends_with(".aho/worktrees/s1"), "{}", a.root().display());
        let b = HostWorld::build((), cfg.clone()).unwrap();
        assert_eq!(a.root(), b.root());
        git(repo.path(), &["rev-parse", "--verify", "refs/heads/aho/s1"]);

        std::fs::remove_dir_all(a.root()).unwrap();
        let c = HostWorld::build((), cfg).unwrap();
        assert!(c.root().join(".git").exists(), "re-added on the existing branch");

        let plain = tempfile::tempdir().unwrap();
        let bad = HostConfig { layout: Layout::Worktree, worktree: Some("s1".into()), ..HostConfig::in_place(plain.path(), home.path()) };
        assert!(matches!(HostWorld::build((), bad), Err(BuildError::Probe(m)) if m.contains("git repository")));
    }

    fn loadout(sets: &[(&str, &str)]) -> Loadout {
        let mut s = Schema::default();
        for k in Composition::new("t").provide::<HostWorld>().config_keys() {
            s.add(k);
        }
        let mut i = ResolveInputs::default();
        i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        resolve(&i, &s, false).unwrap()
    }

    #[test]
    fn config_validates_layout_and_worktree_names() {
        let l = loadout(&[("workspace.root", "/tmp"), ("workspace.layout", "sideways")]);
        assert!(matches!(HostConfig::from_loadout(&l), Err(ConfigError::Type { .. })));
        let l = loadout(&[("workspace.layout", "worktree")]);
        assert!(matches!(HostConfig::from_loadout(&l), Err(ConfigError::MissingKey(k)) if k == "workspace.worktree"));
        let l = loadout(&[("workspace.layout", "worktree"), ("workspace.worktree", "../escape")]);
        assert!(matches!(HostConfig::from_loadout(&l), Err(ConfigError::Type { .. })));
        let l = loadout(&[("workspace.root", "/tmp"), ("workspace.shadow_dir", "/tmp/shadow")]);
        let c = HostConfig::from_loadout(&l).unwrap();
        assert_eq!((c.root, c.layout, c.shadow_dir), (PathBuf::from("/tmp"), Layout::InPlace, PathBuf::from("/tmp/shadow")));
    }

    #[test]
    fn starts_through_a_composition_and_provides_every_declared_handle() {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let comp = Composition::new("t").provide::<HostWorld>();
        let l = loadout(&[("workspace.root", &ws.path().to_string_lossy()), ("workspace.shadow_dir", &home.path().to_string_lossy())]);
        let mut env = Env::empty(Os::current());
        env.binaries.insert("git".into());
        let policy = PolicyInput { preset: "workspace".into(), allowed: aho_types::ConsequenceSet::ALL };
        let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
        assert_eq!(p.world, aho_cap::BoundWorld::host());
        let (reg, broken) = comp.start(&p, &l);
        assert!(broken.is_empty(), "{broken:?}");
        assert!(reg.handle::<FsRead>().is_some() && reg.handle::<FsWrite>().is_some());
        assert!(reg.handle::<SubprocessHandle>().is_some() && reg.handle::<SnapshotHandle>().is_some());
        assert_eq!(reg.handle::<EgressHandle>().map(|e| e.0.enforced_by().to_string()).as_deref(), Some("policy only"));
        assert_eq!(p.seams.get("egress"), Some(&"world-host"));
        let sb = reg.handle::<SandboxHandle>().unwrap();
        assert_eq!(sb.0.writable_roots(), vec![std::fs::canonicalize(ws.path()).unwrap().to_string_lossy().into_owned()]);

        let no_git = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
        assert!(matches!(&no_git.row("world-host").unwrap().state, aho_log::CapState::Locked { .. }));
    }
}
