//! `world = namespace`: a rootless Linux sandbox per session. All tool I/O happens after
//! entering the sandbox's private user, mount, network, IPC and UTS namespaces; each command
//! gets its own PID namespace (it is PID 2) and dies with it. The sandbox's only NIC is plugged
//! into the session's aho-net network; the workspace is the sandbox's private copy of the
//! project, made once and persistent (with `/session`) until `aho sandbox destroy`. The recipe's
//! root is an overlay whose writes last while the sandbox runs.
//!
//! Two ways in: a session opens it with its network (`NamespaceWorld::open`: create on first
//! use, start the keeper, bridge the NIC); operator verbs (`aho shell`, `aho export`,
//! `aho sandbox`) reach an existing sandbox without a network (`NamespaceWorld::existing`),
//! because the session that owns the network may be running in another process.

mod fs;
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods, reason = "authority: world-ns-boundary")]
pub mod linux;
mod proc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_net::{Network, NetworkHandle, Nic};
use aho_sandbox::Sandbox;
use aho_types::{BoxFuture, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use fs::NamespaceFs;
pub use proc::NamespaceProcesses;
pub const ENFORCED_BY: &str = "Linux user+pid+mount+net namespaces; aho-net only NIC";
pub const WORKSPACE: &str = "/workspace";
/// The sandbox's NIC name on the session network: `sandbox.aho`, the default `aho net map` target.
pub const NIC: &str = "sandbox";
/// The helper's name on PATH; [`INSTALL_HELPER`] installs it.
pub const HELPER: &str = "aho-ns-helper";
pub const INSTALL_HELPER: &str = "cargo install --locked --path crates/providers/world-ns --bin aho-ns-helper";

/// `world.ns.*`: what the operator chooses. The sandbox's location follows from the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NsSettings {
    /// Trusted, unpacked Linux recipe. Never the host root.
    pub rootfs: PathBuf,
    /// The workspace seed, copied once on creation; `None` = the session's workspace (the project).
    pub workspace: Option<PathBuf>,
    pub helper: PathBuf,
    pub shell: String,
}

impl Config for NsSettings {
    const PREFIX: &'static str = "world.ns";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "world.ns.rootfs",
                Ty::String,
                None,
                "Unpacked Linux root filesystem (the recipe) with /bin/sh; never the host root. Copied once per sandbox.",
            ),
            ConfigKey::new(
                "world.ns.workspace",
                Ty::String,
                None,
                "Workspace seed copied into a new sandbox (default: the project; its .aho/ is never copied).",
            ),
            ConfigKey::new(
                "world.ns.helper",
                Ty::String,
                Some(json!(HELPER)),
                "The namespace helper executable (a name on PATH, or a path).",
            ),
            ConfigKey::new("world.ns.shell", Ty::String, Some(json!("/bin/sh")), "The operator shell inside the sandbox (`aho shell`)."),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let rootfs = l.str("world.ns.rootfs").filter(|s| !s.is_empty()).ok_or_else(|| ConfigError::MissingKey("world.ns.rootfs".into()))?;
        Ok(Self {
            rootfs: rootfs.into(),
            workspace: l.str("world.ns.workspace").filter(|s| !s.is_empty()).map(PathBuf::from),
            helper: l.str("world.ns.helper").unwrap_or(HELPER).into(),
            shell: l.str("world.ns.shell").unwrap_or("/bin/sh").into(),
        })
    }
}

/// One session's sandbox: settings plus where it lives. Persisted as `config.json` on creation;
/// a later open with a different recipe, seed, helper or shell is refused.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NsConfig {
    pub rootfs: PathBuf,
    pub workspace: PathBuf,
    /// Private (0700) state: `<sessions>/<id>.ns`.
    pub dir: PathBuf,
    pub helper: PathBuf,
    pub shell: String,
}

impl NsConfig {
    pub fn new(settings: &NsSettings, sessions_dir: &Path, session: SessionId, project: &Path) -> Self {
        Self {
            rootfs: settings.rootfs.clone(),
            workspace: settings.workspace.clone().unwrap_or_else(|| project.to_path_buf()),
            dir: sandbox_dir(sessions_dir, session),
            helper: settings.helper.clone(),
            shell: settings.shell.clone(),
        }
    }
}

/// The directory of session `id`'s sandbox.
pub fn sandbox_dir(sessions_dir: &Path, id: SessionId) -> PathBuf {
    sessions_dir.join(format!("{:x}.ns", id.0))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NsStatus {
    pub exists: bool,
    pub running: bool,
    pub keeper_pid: Option<u32>,
    pub nic: Option<Nic>,
    pub workspace: String,
    pub state_dir: PathBuf,
    pub enforced_by: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Keeper {
    pid: u32,
    start: u64,
}

pub(crate) struct Core {
    cfg: NsConfig,
    keeper: Keeper,
    // A dropped world disconnects its network bridge, not its persistent volume.
    #[cfg(unix)]
    bridge: Option<std::os::unix::net::UnixStream>,
}

impl Core {
    pub(crate) fn dir(&self) -> &Path {
        &self.cfg.dir
    }
    pub(crate) fn argv(&self, mode: &str, payload: &str) -> Vec<String> {
        vec![
            self.cfg.helper.to_string_lossy().into_owned(),
            "enter".into(),
            self.cfg.dir.to_string_lossy().into_owned(),
            self.keeper.pid.to_string(),
            self.keeper.start.to_string(),
            mode.into(),
            payload.into(),
        ]
    }
    pub(crate) fn alive(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            linux::check_keeper(&self.keeper)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(unsupported())
        }
    }
}

#[cfg(unix)]
impl Drop for Core {
    fn drop(&mut self) {
        if let Some(bridge) = &self.bridge {
            let _ = bridge.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub struct NamespaceWorld {
    core: Arc<Core>,
    fs: Arc<NamespaceFs>,
    processes: Arc<NamespaceProcesses>,
    sandbox: Arc<NamespaceSandbox>,
}

impl NamespaceWorld {
    /// The session's way in: creates the sandbox on first use, (re)starts its keeper, and plugs
    /// its NIC into `network`.
    pub fn open(cfg: NsConfig, network: Arc<dyn Network>) -> Result<Self, BuildError> {
        Self::start(cfg, Some(network))
    }

    /// The operator's way into a sandbox that already exists, without its network: a keeper that
    /// stopped (a reboot) is restarted offline and the owning session re-plugs it when it opens.
    pub fn existing(cfg: NsConfig) -> Result<Self, BuildError> {
        Self::start(cfg, None)
    }

    fn start(cfg: NsConfig, network: Option<Arc<dyn Network>>) -> Result<Self, BuildError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (cfg, network);
            Err(BuildError::Probe(unsupported()))
        }
        #[cfg(target_os = "linux")]
        {
            let core = Arc::new(linux::open(cfg, network).map_err(BuildError::Probe)?);
            let processes = Arc::new(NamespaceProcesses::new(core.clone()));
            let fs = Arc::new(NamespaceFs::new(core.clone()));
            let sandbox = Arc::new(NamespaceSandbox { core: core.clone(), processes: processes.clone() });
            Ok(Self { core, fs, processes, sandbox })
        }
    }
    pub fn root(&self) -> &Path {
        Path::new(WORKSPACE)
    }
    pub fn fs(&self) -> Arc<NamespaceFs> {
        self.fs.clone()
    }
    pub fn subprocess(&self) -> Arc<NamespaceProcesses> {
        self.processes.clone()
    }
    pub fn sandbox(&self) -> Arc<NamespaceSandbox> {
        self.sandbox.clone()
    }
    pub fn status(&self) -> NsStatus {
        inspect(&self.core.cfg.dir)
    }
    /// Stop the namespace and every attached command, then remove its persistent volume.
    pub fn destroy(&self) -> Result<bool, String> {
        destroy(&self.core.cfg.dir)
    }
}

/// The state of the sandbox in `dir`, read without starting anything.
pub fn inspect(dir: &Path) -> NsStatus {
    #[cfg(target_os = "linux")]
    let (exists, keeper, nic) = linux::inspect(dir);
    // No sandbox can exist where namespaces do not.
    #[cfg(not(target_os = "linux"))]
    let (exists, keeper, nic): (bool, Option<u32>, Option<Nic>) = (false, None, None);
    NsStatus {
        exists,
        running: keeper.is_some(),
        keeper_pid: keeper,
        nic,
        workspace: WORKSPACE.into(),
        state_dir: dir.to_path_buf(),
        enforced_by: ENFORCED_BY.into(),
    }
}

/// Stops the sandbox in `dir` (its keeper, and so every command in it) and removes it.
/// `Ok(false)` when there was none.
pub fn destroy(dir: &Path) -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        linux::destroy(dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        Err(unsupported())
    }
}

pub struct NamespaceSandbox {
    core: Arc<Core>,
    processes: Arc<NamespaceProcesses>,
}
impl Sandbox for NamespaceSandbox {
    fn bound(&self) -> BoundWorld {
        BoundWorld { name: "namespace".into(), confines: true, enforced_by: ENFORCED_BY.into() }
    }
    fn writable_roots(&self) -> Vec<String> {
        vec![WORKSPACE.into(), "/session".into(), "/tmp".into(), "/dev/shm".into()]
    }
    /// The host-side argv that runs the operator's shell inside the sandbox, on this terminal.
    fn operator_shell(&self) -> Vec<String> {
        let command = proc::Command { argv: vec![self.core.cfg.shell.clone()], cwd: WORKSPACE.into(), env: vec![] };
        self.core.argv("exec", &serde_json::to_string(&command).unwrap_or_default())
    }
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        use aho_proc::{SpawnSpec, Subprocess};
        Box::pin(async move {
            self.core.alive()?;
            let child = self
                .processes
                .spawn(SpawnSpec {
                    timeout_ms: Some(5000),
                    ..SpawnSpec::new(&[
                        "/bin/sh",
                        "-c",
                        "test $$ -eq 2 && test -d /workspace && test -w /tmp && test -w /dev/shm && test ! -e /.oldroot/proc",
                    ])
                })
                .await
                .map_err(|e| e.to_string())?;
            let exit = child.wait().await.map_err(|e| e.to_string())?;
            if exit.code == 0 { Ok(()) } else { Err(format!("namespace functional probe exited {}", exit.code)) }
        })
    }
}

/// The session network, and where the session keeps its state and workspace.
pub struct NamespaceDeps {
    pub network: NetworkHandle,
    pub sessions_dir: Option<PathBuf>,
    pub session: Option<SessionId>,
    pub project: Option<String>,
}
impl Deps for NamespaceDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::NetAttach];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(Self {
            network: r.handle()?,
            sessions_dir: r.service("aho.session_dir"),
            session: r.service("aho.session_id"),
            project: r.service("aho.workspace"),
        })
    }
}

impl Capability for NamespaceWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = NamespaceDeps;
    type Config = NsSettings;
    fn build(deps: Self::Deps, settings: NsSettings) -> Result<Self, BuildError> {
        let (Some(sessions_dir), Some(session), Some(project)) = (deps.sessions_dir, deps.session, deps.project) else {
            return Err(BuildError::Unsupported(
                "the namespace world belongs to a session (its id, directory and workspace); none was seeded",
            ));
        };
        Self::open(NsConfig::new(&settings, &sessions_dir, session, Path::new(&project)), deps.network.0)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(aho_fs::FsRead(self.fs.clone()));
        r.provide(aho_fs::FsWrite(self.fs.clone()));
        r.provide(aho_fs::SnapshotHandle(self.fs.clone()));
        r.provide(aho_proc::SubprocessHandle(self.processes.clone()));
        r.provide(aho_term::TerminalsHandle(self.processes.clone()));
        r.provide(aho_sandbox::SandboxHandle(self.sandbox.clone()));
    }
}

declare_capability!(
    NamespaceWorld,
    "world-namespace",
    kind = Kind::Provider,
    provides = [aho_fs::SEAM, aho_proc::SEAM, aho_sandbox::SEAM, aho_fs::SNAPSHOT_SEAM, aho_term::SEAM],
    requires = [
        Requirement::Platform(Os::Linux),
        Requirement::Seam(aho_net::SEAM),
        Requirement::Setting { key: "world.ns.rootfs", pred: Pred::Set },
        Requirement::Binary(HELPER),
        Requirement::Binary("ip")
    ],
    tools = &[],
    provides_handles = [
        HandleKind::FsRead,
        HandleKind::FsWrite,
        HandleKind::Subprocess,
        HandleKind::Snapshot,
        HandleKind::Terminals,
        HandleKind::Custom("sandbox")
    ],
    exclusive = true,
    unlock = Fix::cli(INSTALL_HELPER),
    enforced_by = ENFORCED_BY,
    entry_kinds = ["Sandbox/Destroyed"],
    doc = "Rootless Linux namespace sandbox per session (`seams.world = namespace`): a private persistent copy of the project as its workspace, PID-namespace teardown per command, and the session's aho-net network as its only NIC (`sandbox.aho`). Needs an unpacked recipe (`world.ns.rootfs`), the aho-ns-helper binary and iproute2."
);

#[cfg(not(target_os = "linux"))]
fn unsupported() -> String {
    "the namespace world needs Linux user, PID, mount and network namespaces; this platform cannot enforce them. Select `seams.world = vm`, or run aho on Linux.".into()
}
