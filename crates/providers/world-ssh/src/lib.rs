//! The ssh world: `Fs + Subprocess + Sandbox + Snapshot + Egress` on another machine, reached
//! with the system `ssh` client. Providers `fs-ssh`, `proc-ssh`, `sandbox-ssh`; the workspace is
//! a remote directory; any local platform works. The workspace lives on the remote (sandbox
//! worlds keep it inside); processes, files and the network are the remote's own, so `aho plan`
//! prints `enforced by: remote host`, `NetworkEgress` included.
//!
//! A third-party-style world in one crate: selected with `[seams] world = "ssh"`, configured
//! under `[world.ssh]`, exclusive (fs, subprocess, sandbox, snapshot and egress bind together).
//!
//! * transport: one ControlMaster per world (socket under `world.ssh.control_dir`, by default
//!   `<session.dir>/ssh`), `BatchMode=yes`, host keys checked by ssh itself ([`ssh`]);
//! * fs: POSIX sh on the remote with every argument single-quoted, bytes raw over the channel ([`fs`]);
//! * processes: one perl launcher per command, with its own process group, exact exit status and
//!   group kill ([`proc`]);
//! * snapshots: a shadow SHA-256 git store on the remote, same semantics as the host's ([`snapshot`]);
//! * egress: `web_fetch` & co. leave from the remote, one remote `curl` per hop, redirects and the
//!   operator's host list handled here ([`egress`]).
//!
//! Remote requirements (checked when the world starts): a POSIX `/bin/sh`, `perl`, `git ≥ 2.29`,
//! and a login shell that reads POSIX single quotes (sh, bash, zsh, dash, ksh, fish; not csh).
//! `curl` is needed only for egress; without it a fetch fails with a message naming the fix.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod egress;
pub mod fs;
pub mod proc;
pub mod sandbox;
pub mod snapshot;
pub mod ssh;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_egress::EgressHandle;
use aho_fs::{FsRead, FsWrite, SnapshotHandle};
use aho_proc::SubprocessHandle;
use aho_sandbox::SandboxHandle;
use serde_json::json;

pub use crate::egress::SshEgress;
pub use crate::fs::SshFs;
pub use crate::proc::SshSubprocess;
pub use crate::sandbox::SshSandbox;
pub use crate::snapshot::RemoteShadowGit;
pub use crate::ssh::{Ssh, SshError, Target};

/// The `enforced by:` text `aho plan` prints for this world.
pub const ENFORCED_BY: &str = "remote host";

/// Default remote directory for shadow repositories, relative to the remote `$HOME`.
pub const DEFAULT_SHADOW_DIR: &str = ".aho/shadow";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConfig {
    pub target: Target,
    /// Remote workspace directory; relative paths are under the remote `$HOME`. Created if missing.
    pub workspace: String,
    /// Remote directory holding shadow repos; relative paths are under the remote `$HOME`.
    pub shadow_dir: String,
    /// Local directory for the ControlMaster socket.
    pub control_dir: PathBuf,
}

impl SshConfig {
    /// `host` + remote `workspace`, control socket under `control_dir`, default shadow dir.
    pub fn new(host: impl Into<String>, workspace: impl Into<String>, control_dir: impl Into<PathBuf>) -> SshConfig {
        SshConfig {
            target: Target { host: host.into(), ..Target::default() },
            workspace: workspace.into(),
            shadow_dir: DEFAULT_SHADOW_DIR.into(),
            control_dir: control_dir.into(),
        }
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: world-ssh-config-home")]
fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(p),
    }
}

fn opt_str(l: &Loadout, key: &str) -> Option<String> {
    l.str(key).filter(|s| !s.is_empty()).map(str::to_string)
}

impl Config for SshConfig {
    const PREFIX: &'static str = "world.ssh";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "world.ssh.host",
                Ty::String,
                None,
                "Remote host of the ssh world: a host name, address, or alias from your ssh config.",
            ),
            ConfigKey::new(
                "world.ssh.user",
                Ty::String,
                None,
                "Remote user; default: whatever your ssh config says (usually your local user).",
            ),
            ConfigKey::new("world.ssh.port", Ty::Int, None, "Remote ssh port; default: your ssh config's (usually 22)."),
            ConfigKey::new(
                "world.ssh.identity",
                Ty::String,
                None,
                "Private key file for the login (`ssh -i`, with IdentitiesOnly); default: your ssh config and agent.",
            ),
            ConfigKey::new(
                "world.ssh.workspace",
                Ty::String,
                None,
                "Remote workspace directory (created if missing); relative paths are under the remote $HOME.",
            ),
            ConfigKey::new(
                "world.ssh.shadow_dir",
                Ty::String,
                Some(json!(DEFAULT_SHADOW_DIR)),
                "Remote directory for checkpoint trees; relative paths are under the remote $HOME.",
            ),
            ConfigKey::new(
                "world.ssh.control_dir",
                Ty::String,
                None,
                "Local directory for the ssh ControlMaster socket; default: `<session.dir>/ssh`. Keep it short (unix socket paths are limited).",
            ),
            ConfigKey::new("world.ssh.config", Ty::String, None, "ssh_config file to use instead of ~/.ssh/config (`ssh -F`)."),
            ConfigKey::new(
                "world.ssh.known_hosts",
                Ty::String,
                None,
                "known_hosts file to check the host key against (`UserKnownHostsFile`).",
            ),
        ]
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-config-cwd")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let host = opt_str(l, "world.ssh.host").ok_or_else(|| ConfigError::MissingKey("world.ssh.host".into()))?;
        let workspace = opt_str(l, "world.ssh.workspace").ok_or_else(|| ConfigError::MissingKey("world.ssh.workspace".into()))?;
        let port = match l.get("world.ssh.port") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p > 0)
                    .ok_or(ConfigError::Type { key: "world.ssh.port".into(), expected: "a port number 1–65535" })?,
            ),
        };
        let control_dir = match opt_str(l, "world.ssh.control_dir") {
            Some(d) => expand_home(&d),
            None => expand_home(l.str("session.dir").filter(|s| !s.is_empty()).unwrap_or(".aho/sessions")).join("ssh"),
        };
        let control_dir = if control_dir.is_absolute() {
            control_dir
        } else {
            std::env::current_dir().map_err(|_| ConfigError::MissingKey("world.ssh.control_dir".into()))?.join(control_dir)
        };
        Ok(SshConfig {
            target: Target {
                host,
                user: opt_str(l, "world.ssh.user"),
                port,
                identity: opt_str(l, "world.ssh.identity").map(|p| expand_home(&p)),
                config: opt_str(l, "world.ssh.config").map(|p| expand_home(&p)),
                known_hosts: opt_str(l, "world.ssh.known_hosts").map(|p| expand_home(&p)),
            },
            workspace,
            shadow_dir: opt_str(l, "world.ssh.shadow_dir").unwrap_or_else(|| DEFAULT_SHADOW_DIR.into()),
            control_dir,
        })
    }
}

/// Creates the workspace, resolves the remote paths and checks the remote tools.
/// Output lines: canonical root, root as configured, shadow home, then `missing:<tool>` lines.
const PROBE: &str = r#"unset CDPATH
case $1 in /*) _ws=$1 ;; *) _ws=$HOME/$1 ;; esac
case $2 in /*) _sh=$2 ;; *) _sh=$HOME/$2 ;; esac
mkdir -p -- "$_ws" || exit 40
_canon=$(cd -P -- "$_ws" && pwd -P) || exit 41
printf '%s\n%s\n%s\n' "$_canon" "$_ws" "$_sh"
command -v perl >/dev/null 2>&1 || echo missing:perl
command -v git >/dev/null 2>&1 || echo missing:git
exit 0
"#;

pub struct SshWorld {
    ssh: Arc<Ssh>,
    root: String,
    fs: SshFs,
    proc: Arc<SshSubprocess>,
    sandbox: Arc<SshSandbox>,
    snapshot: RemoteShadowGit,
    egress: SshEgress,
}

impl SshWorld {
    /// Connects (starting the ControlMaster), creates the remote workspace and checks the remote
    /// tools. Unknown host key, refused login, missing perl/git → `BuildError::Probe` whose text
    /// starts with `Locked:` and names the fix.
    pub fn connect(cfg: SshConfig) -> Result<SshWorld, BuildError> {
        let ssh = Arc::new(Ssh::new(cfg.target.clone(), &cfg.control_dir).map_err(BuildError::Probe)?);
        let host = cfg.target.display();
        let out = ssh.run_blocking(PROBE, &[&cfg.workspace, &cfg.shadow_dir]).map_err(|e| BuildError::Probe(e.to_string()))?;
        match out.code {
            0 => {}
            40 | 41 => {
                return Err(BuildError::Probe(format!(
                    "could not create or open the workspace `{}` on {host} ({}); nothing else was started. Set world.ssh.workspace to a directory you can write there.",
                    cfg.workspace,
                    out.stderr.trim()
                )));
            }
            code => {
                return Err(BuildError::Probe(format!(
                    "the startup check on {host} failed (exit {code}: {}); nothing was started. Check that /bin/sh works there.",
                    out.stderr.trim()
                )));
            }
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let (Some(root), Some(configured), Some(shadow)) = (lines.next(), lines.next(), lines.next()) else {
            return Err(BuildError::Probe(format!("{host} answered the startup check with `{}`; nothing was started.", text.trim())));
        };
        let missing: Vec<&str> = lines.filter_map(|l| l.strip_prefix("missing:")).collect();
        if !missing.is_empty() {
            return Err(BuildError::Probe(
                SshError::Locked {
                    what: format!(
                        "{host} has no {} (the ssh world runs commands through perl and keeps checkpoints with git)",
                        missing.join(" or ")
                    ),
                    fix: format!("install {} (git ≥ 2.29) on {host}", missing.join(" and ")),
                }
                .to_string(),
            ));
        }
        if root == "/" || !root.starts_with('/') {
            return Err(BuildError::Probe(format!(
                "the workspace resolved to `{root}` on {host}; refusing to use that as a workspace root. Set world.ssh.workspace to a project directory."
            )));
        }
        let root = root.to_string();
        let fs = SshFs::new(ssh.clone(), root.clone(), Some(configured.to_string()));
        let proc = Arc::new(SshSubprocess::new(ssh.clone(), root.clone()));
        let sandbox = Arc::new(SshSandbox::new(root.clone(), ssh.clone(), proc.clone()));
        let snapshot = RemoteShadowGit::new(ssh.clone(), &root, shadow);
        let egress = SshEgress::new(ssh.clone());
        Ok(SshWorld { ssh, root, fs, proc, sandbox, snapshot, egress })
    }

    /// Canonical remote workspace root every tool resolves against.
    pub fn root(&self) -> &str {
        &self.root
    }
    pub fn fs(&self) -> &SshFs {
        &self.fs
    }
    pub fn subprocess(&self) -> Arc<SshSubprocess> {
        self.proc.clone()
    }
    pub fn sandbox(&self) -> Arc<SshSandbox> {
        self.sandbox.clone()
    }
    pub fn snapshot(&self) -> &RemoteShadowGit {
        &self.snapshot
    }
    /// The remote's network: one remote `curl` per hop.
    pub fn egress(&self) -> &SshEgress {
        &self.egress
    }
    /// The local ControlMaster socket (exists while the master runs).
    pub fn control_path(&self) -> &Path {
        self.ssh.control_path()
    }
}

impl Capability for SshWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = SshConfig;

    fn build(_: (), cfg: SshConfig) -> Result<Self, BuildError> {
        SshWorld::connect(cfg)
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
    SshWorld,
    "world-ssh",
    kind = Kind::Provider,
    provides = [aho_fs::SEAM, aho_proc::SEAM, aho_sandbox::SEAM, aho_fs::SNAPSHOT_SEAM, aho_egress::SEAM],
    requires = [
        Requirement::Binary("ssh"),
        Requirement::Setting { key: "world.ssh.host", pred: Pred::Set },
        Requirement::Setting { key: "world.ssh.workspace", pred: Pred::Set },
    ],
    tools = &[],
    provides_handles = [
        HandleKind::FsRead,
        HandleKind::FsWrite,
        HandleKind::Subprocess,
        HandleKind::Snapshot,
        HandleKind::Custom("sandbox"),
        HandleKind::Egress(Route::Sandbox),
    ],
    exclusive = true,
    unlock = Fix::cli("aho set world.ssh.host <host>"),
    enforced_by = ENFORCED_BY,
    doc = "SSH world: the workspace, processes and network are a remote host's, reached with the system ssh client over one ControlMaster; checkpoints in a shadow git store on the remote."
);

#[cfg(test)]
mod tests {
    use super::*;
    use aho_config::{ResolveInputs, Schema, resolve};

    fn loadout(sets: &[(&str, &str)]) -> Loadout {
        let mut s = Schema::default();
        for k in Composition::new("t").provide::<SshWorld>().config_keys() {
            s.add(k);
        }
        let mut i = ResolveInputs::default();
        i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        resolve(&i, &s, false).unwrap()
    }

    #[test]
    fn config_reads_every_key_and_validates_the_port() {
        let l = loadout(&[
            ("world.ssh.host", "box"),
            ("world.ssh.user", "dev"),
            ("world.ssh.port", "2222"),
            ("world.ssh.identity", "/k/id"),
            ("world.ssh.workspace", "src/app"),
            ("world.ssh.control_dir", "/c"),
            ("world.ssh.known_hosts", "/k/kh"),
        ]);
        let c = SshConfig::from_loadout(&l).unwrap();
        assert_eq!(
            c.target,
            Target {
                host: "box".into(),
                user: Some("dev".into()),
                port: Some(2222),
                identity: Some("/k/id".into()),
                config: None,
                known_hosts: Some("/k/kh".into())
            }
        );
        assert_eq!((c.workspace.as_str(), c.shadow_dir.as_str(), c.control_dir.as_path()), ("src/app", ".aho/shadow", Path::new("/c")));

        let l = loadout(&[("world.ssh.host", "box"), ("world.ssh.workspace", "w"), ("world.ssh.port", "70000")]);
        assert!(matches!(SshConfig::from_loadout(&l), Err(ConfigError::Type { .. })));
        let l = loadout(&[("world.ssh.workspace", "w")]);
        assert!(matches!(SshConfig::from_loadout(&l), Err(ConfigError::MissingKey(k)) if k == "world.ssh.host"));
    }

    #[test]
    fn the_control_socket_defaults_to_the_session_directory() {
        let l = loadout(&[("world.ssh.host", "box"), ("world.ssh.workspace", "w"), ("session.dir", "/s/sessions")]);
        let c = SshConfig::from_loadout(&l).unwrap();
        assert_eq!(c.control_dir, PathBuf::from("/s/sessions/ssh"));
    }

    fn plan_with(sets: &[(&str, &str)], ssh_on_path: bool) -> Plan {
        let comp = Composition::new("t").provide::<world_host::HostWorld>().provide::<SshWorld>();
        let mut s = Schema::default();
        for k in comp.config_keys() {
            s.add(k);
        }
        let mut i = ResolveInputs::default();
        i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let l = resolve(&i, &s, false).unwrap();
        let mut env = Env::empty(Os::current());
        env.binaries.insert("git".into());
        if ssh_on_path {
            env.binaries.insert("ssh".into());
        }
        let policy = PolicyInput { preset: "workspace".into(), allowed: aho_types::ConsequenceSet::ALL };
        plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap()
    }

    #[test]
    fn seams_world_ssh_binds_the_whole_bundle_and_prints_remote_host() {
        let p = plan_with(&[("seams.world", "ssh"), ("world.ssh.host", "box"), ("world.ssh.workspace", "w")], true);
        assert_eq!(p.world, BoundWorld { name: "ssh".into(), confines: true, enforced_by: "remote host".into() });
        for seam in ["fs", "subprocess", "sandbox", "snapshot", "egress"] {
            assert_eq!(p.seams.get(seam), Some(&"world-ssh"), "{seam}");
        }
        assert_eq!(p.row("world-ssh").unwrap().state, aho_log::CapState::Active);
        assert!(matches!(p.row("world-host").unwrap().state, aho_log::CapState::Unlocked), "the other world is linked but not bound");
    }

    #[test]
    fn missing_host_or_ssh_client_locks_the_world_with_a_fix() {
        let p = plan_with(&[("seams.world", "ssh"), ("world.ssh.workspace", "w")], true);
        let row = p.row("world-ssh").unwrap();
        assert!(matches!(&row.state, aho_log::CapState::Locked { missing } if missing.iter().any(|m| m.contains("world.ssh.host"))));
        assert!(row.missing.iter().any(|m| m.fix.as_deref() == Some("aho set world.ssh.host <value>")), "{:?}", row.missing);

        let p = plan_with(&[("seams.world", "ssh"), ("world.ssh.host", "box"), ("world.ssh.workspace", "w")], false);
        let row = p.row("world-ssh").unwrap();
        assert!(matches!(&row.state, aho_log::CapState::Locked { missing } if missing.iter().any(|m| m == "Binary(ssh)")));
    }
}
