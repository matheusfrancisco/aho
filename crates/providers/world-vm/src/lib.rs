//! QEMU machines for a session (07): `vm-qemu` is the session's VM lab (the `vm` seam behind the
//! 13 `vm_*` tools, under an operator-granted lease), and `world-vm` (`seams.world = vm`) makes
//! one of its machines — the sandbox VM, built from `vm.recipe` — the session's world: files,
//! processes and terminals cross the guest agent on virtio-serial, the screen is the session
//! desktop over RFB, and the only NIC is the session's aho-net network.
//!
//! Every machine is supervised by `aho-vm-supervisor` (so a lease still expires while no aho
//! process runs), identified by a per-machine UUID checked over QMP before every control effect,
//! and built only from catalog media the operator imported (`aho media import`) through a recipe
//! the operator wrote (`aho add recipe`). QEMU's argv is fixed here: a recipe cannot add devices,
//! host mounts or a second NIC.

mod host;
mod manager;
mod media;
mod network;
mod qmp;
mod storage;
mod supervisor;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::{Entry, EntryKind, LogStore};
use aho_net::NetworkHandle;
use aho_sandbox::Sandbox;
use aho_types::{BoxFuture, SessionId};
use aho_vm::{Architecture, LeaseLimits, Machine, MediaKind, VmError};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use host::{MachineOrphan, destroy_machine, orphans, revoke_lease, sandbox_guest, sandbox_record, set_lease};
pub use manager::{SANDBOX_NAME, VmManager, nic_name};
pub use media::{Catalog, validate_recipe};
pub use supervisor::supervise;

pub const ENFORCED_BY: &str = "QEMU guest boundary; guest agent on virtio-serial; aho-net only NIC";
/// The installed watchdog that owns every QEMU process.
pub const SUPERVISOR: &str = "aho-vm-supervisor";
/// `LeaseGranted.subject` prefix of a VM lease; the rest is the limits as JSON.
pub const LEASE_SUBJECT: &str = "vm:";

/// Everything a manager needs; built from `VmSettings` plus the session.
#[derive(Clone, Debug)]
pub struct VmConfig {
    pub owner_session: String,
    pub lease: LeaseLimits,
    /// `<state>/<owner_session>/machines/<name>/…`
    pub state_dir: PathBuf,
    /// Operator media and recipes: `<catalog>/media`, `<catalog>/recipes`.
    pub catalog_dir: PathBuf,
    pub qemu_img: PathBuf,
    pub qemu_aarch64: PathBuf,
    pub qemu_x86_64: PathBuf,
    pub supervisor: PathBuf,
    /// The owning project's `session.dir`, recorded so `aho gc` there judges only its own machines.
    pub sessions_dir: Option<PathBuf>,
}

impl VmConfig {
    pub fn qemu(&self, arch: &Architecture) -> &Path {
        match arch {
            Architecture::Aarch64 => &self.qemu_aarch64,
            Architecture::X86_64 => &self.qemu_x86_64,
        }
    }
}

/// `vm.*`: binaries, where the catalog and machine state live, and the sandbox recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmSettings {
    pub qemu_img: PathBuf,
    pub qemu_aarch64: PathBuf,
    pub qemu_x86_64: PathBuf,
    pub supervisor: PathBuf,
    /// Default: `~/.aho/vm` (short enough for the machines' unix sockets).
    pub dir: Option<PathBuf>,
    /// The recipe of the session's sandbox VM under `seams.world = vm`.
    pub recipe: Option<String>,
    pub boot_timeout_s: u64,
}

impl Config for VmSettings {
    const PREFIX: &'static str = "vm";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("vm.qemu_img", Ty::String, Some(json!("qemu-img")), "qemu-img, for disks and media checks."),
            ConfigKey::new("vm.qemu_aarch64", Ty::String, Some(json!("qemu-system-aarch64")), "QEMU for aarch64 recipes."),
            ConfigKey::new("vm.qemu_x86_64", Ty::String, Some(json!("qemu-system-x86_64")), "QEMU for x86_64 recipes."),
            ConfigKey::new("vm.supervisor", Ty::String, Some(json!(SUPERVISOR)), "The VM watchdog executable (a name on PATH, or a path)."),
            ConfigKey::new(
                "vm.dir",
                Ty::String,
                None,
                "Media, recipes and machine state (default ~/.aho/vm). Keep it short: machines listen on unix sockets inside it.",
            ),
            ConfigKey::new(
                "vm.recipe",
                Ty::String,
                None,
                "The recipe of the session's sandbox VM (`seams.world = vm`); `aho add recipe` writes recipes.",
            ),
            ConfigKey::new(
                "vm.boot_timeout_s",
                Ty::Int,
                Some(json!(180)),
                "How long a sandbox VM may take to boot until its guest agent answers.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let s = |k: &str, d: &str| PathBuf::from(l.str(k).filter(|s| !s.is_empty()).unwrap_or(d));
        Ok(VmSettings {
            qemu_img: s("vm.qemu_img", "qemu-img"),
            qemu_aarch64: s("vm.qemu_aarch64", "qemu-system-aarch64"),
            qemu_x86_64: s("vm.qemu_x86_64", "qemu-system-x86_64"),
            supervisor: s("vm.supervisor", SUPERVISOR),
            dir: l.str("vm.dir").filter(|s| !s.is_empty()).map(PathBuf::from),
            recipe: l.str("vm.recipe").filter(|s| !s.is_empty()).map(str::to_string),
            boot_timeout_s: l.get("vm.boot_timeout_s").and_then(serde_json::Value::as_u64).unwrap_or(180),
        })
    }
}

impl VmSettings {
    /// The catalog root: `vm.dir`, else `<home>/.aho/vm`.
    pub fn root(&self, home: &Path) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| home.join(".aho").join("vm"))
    }
    pub fn config(&self, home: &Path, session: SessionId, lease: LeaseLimits, sessions_dir: Option<PathBuf>) -> VmConfig {
        let root = self.root(home);
        VmConfig {
            owner_session: session_name(session),
            lease,
            state_dir: root.join("state"),
            catalog_dir: root,
            qemu_img: self.qemu_img.clone(),
            qemu_aarch64: self.qemu_aarch64.clone(),
            qemu_x86_64: self.qemu_x86_64.clone(),
            supervisor: self.supervisor.clone(),
            sessions_dir,
        }
    }
}

/// A session's name as VM owner (the log's hex id).
pub fn session_name(id: SessionId) -> String {
    format!("{:x}", id.0)
}

/// The limits an operator grants (`aho lease grant`), carried in `LeaseGranted.subject`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseTerms {
    pub max_machines: u32,
    pub max_disk_bytes: u64,
    pub max_cpu_s: u64,
}

impl LeaseTerms {
    pub fn subject(&self) -> String {
        format!("{LEASE_SUBJECT}{}", serde_json::to_string(self).unwrap_or_default())
    }
}

/// The session's live VM lease: the latest `LeaseGranted` with a VM subject not revoked since.
/// `granted_ref` is that entry's id, so every machine names the log entry that authorised it.
pub fn lease_of(entries: &[Entry]) -> Option<LeaseLimits> {
    let mut live: Option<LeaseLimits> = None;
    for e in entries {
        match &e.kind {
            EntryKind::LeaseGranted { lease, subject, expires } => {
                if let Some(terms) = subject.strip_prefix(LEASE_SUBJECT).and_then(|t| serde_json::from_str::<LeaseTerms>(t).ok()) {
                    live = Some(LeaseLimits {
                        id: lease.0.to_string(),
                        granted_ref: e.id.0.to_string(),
                        expires_at_ms: expires.0,
                        max_machines: terms.max_machines,
                        max_disk_bytes: terms.max_disk_bytes,
                        max_cpu_s: terms.max_cpu_s,
                        revoked: false,
                    });
                }
            }
            EntryKind::LeaseRevoked { lease, .. } if live.as_ref().is_some_and(|l| l.id == lease.0.to_string()) => live = None,
            _ => {}
        }
    }
    live
}

fn branch(log: &dyn LogStore) -> Vec<Entry> {
    log.head().and_then(|h| log.read_branch(h).ok()).unwrap_or_default()
}

fn vm_err(e: VmError) -> BuildError {
    BuildError::Probe(e.to_string())
}

/// The session's VM manager, for the world that runs on one of its machines.
#[derive(Clone)]
pub struct VmManagerHandle(pub Arc<VmManager>);
impl Handle for VmManagerHandle {
    const KIND: HandleKind = HandleKind::Custom("vm-manager");
}

// ───────────────────────────── vm-qemu: the session's lab ─────────────────────────────

pub struct VmDeps {
    pub network: NetworkHandle,
    pub session: Option<SessionId>,
    pub home: Option<String>,
    pub log: Option<Arc<dyn LogStore>>,
    pub sessions_dir: Option<PathBuf>,
}
impl Deps for VmDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::NetAttach];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(VmDeps {
            network: r.handle()?,
            session: r.service("aho.session_id"),
            home: r.service("aho.home"),
            log: r.service("aho.log"),
            sessions_dir: r.service("aho.session_dir"),
        })
    }
}

pub struct VmQemu {
    manager: Arc<VmManager>,
}

impl VmQemu {
    pub fn manager(&self) -> Arc<VmManager> {
        self.manager.clone()
    }
}

impl Capability for VmQemu {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VmDeps;
    type Config = VmSettings;
    fn build(deps: VmDeps, settings: VmSettings) -> Result<Self, BuildError> {
        let (Some(session), Some(home), Some(log)) = (deps.session, deps.home, deps.log) else {
            return Err(BuildError::Unsupported("the VM lab belongs to a session (its id, home and log); none was seeded"));
        };
        let Some(lease) = lease_of(&branch(&*log)) else {
            return Err(BuildError::Probe(
                "this session holds no VM lease; the operator grants one with `aho lease grant` (machines, disk, CPU time, expiry), and no machine runs without it".into(),
            ));
        };
        let config = settings.config(Path::new(&home), session, lease, deps.sessions_dir);
        let manager = VmManager::open(config, deps.network.0).map_err(vm_err)?;
        Ok(VmQemu { manager: Arc::new(manager) })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(aho_vm::VmControl(self.manager.clone()));
        r.provide(VmManagerHandle(self.manager.clone()));
    }
}

declare_capability!(
    VmQemu,
    "vm-qemu",
    kind = Kind::Provider,
    provides = [aho_vm::SEAM],
    requires = [
        Requirement::AnyOf(&[Requirement::Platform(Os::Linux), Requirement::Platform(Os::MacOs)]),
        Requirement::Seam(aho_net::SEAM),
        Requirement::AnyOf(&[Requirement::Binary("qemu-system-aarch64"), Requirement::Binary("qemu-system-x86_64")]),
        Requirement::Binary("qemu-img"),
        Requirement::Binary(SUPERVISOR)
    ],
    tools = &[],
    provides_handles = [HandleKind::VmControl, HandleKind::Custom("vm-manager")],
    unlock = Fix::cli("cargo install --locked --path crates/providers/world-vm --bin aho-vm-supervisor"),
    enforced_by = ENFORCED_BY,
    doc = "The session's QEMU lab under an operator lease (`aho lease grant`): machines from operator recipes over catalog media, each supervised (the lease expires with no aho running), identified by UUID over QMP, on the session's aho-net network (`<name>.aho`)."
);

// ───────────────────────────── world-vm: the sandbox VM as the world ─────────────────────────────

pub struct VmWorldDeps {
    pub manager: VmManagerHandle,
}
impl Deps for VmWorldDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Custom("vm-manager")];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(VmWorldDeps { manager: r.handle()? })
    }
}

pub struct VmWorld {
    manager: Arc<VmManager>,
    guest: Arc<vm_guest::Guest>,
    sandbox: Arc<VmSandbox>,
}

impl VmWorld {
    /// Creates the sandbox VM from `recipe` on first use, starts it, and waits until its guest
    /// agent answers (at most `boot`).
    pub fn open(manager: Arc<VmManager>, recipe: &str, boot: std::time::Duration) -> Result<Self, VmError> {
        let machine = manager.ensure_sandbox(recipe)?;
        manager.start_sandbox()?;
        let socket = manager.guest_socket(SANDBOX_NAME)?;
        let until = std::time::Instant::now() + boot;
        let guest = loop {
            match vm_guest::Guest::connect(socket.clone(), machine.recipe.workspace.clone(), manager.guard(SANDBOX_NAME)) {
                Ok(guest) => break guest,
                Err(e) if std::time::Instant::now() < until => {
                    let _ = e;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => {
                    return Err(VmError::Locked(format!(
                        "the sandbox VM did not answer within {}s: {e}. Its console (`aho sandbox status`) shows how far it booted; the recipe's image must run vm-guest-agent",
                        boot.as_secs()
                    )));
                }
            }
        };
        Ok(Self::with(manager, guest, machine))
    }

    /// The operator's way in: a sandbox VM that already runs, without starting anything.
    pub fn existing(manager: Arc<VmManager>) -> Result<Self, VmError> {
        let machine = manager.sandbox_status()?;
        let guest =
            vm_guest::Guest::connect(manager.guest_socket(SANDBOX_NAME)?, machine.recipe.workspace.clone(), manager.guard(SANDBOX_NAME))
                .map_err(|e| VmError::Locked(format!("the sandbox VM is not reachable ({e}); it runs while its session does")))?;
        Ok(Self::with(manager, guest, machine))
    }

    fn with(manager: Arc<VmManager>, guest: Arc<vm_guest::Guest>, machine: Machine) -> Self {
        let sandbox = Arc::new(VmSandbox { workspace: machine.recipe.workspace.clone(), subprocess: guest.subprocess() });
        VmWorld { manager, guest, sandbox }
    }
    pub fn fs(&self) -> Arc<dyn aho_fs::Fs> {
        self.guest.fs()
    }
    pub fn subprocess(&self) -> Arc<dyn aho_proc::Subprocess> {
        self.guest.subprocess()
    }
    pub fn manager(&self) -> &Arc<VmManager> {
        &self.manager
    }
}

pub struct VmSandbox {
    workspace: String,
    subprocess: Arc<dyn aho_proc::Subprocess>,
}
impl Sandbox for VmSandbox {
    fn bound(&self) -> BoundWorld {
        BoundWorld { name: "vm".into(), confines: true, enforced_by: ENFORCED_BY.into() }
    }
    fn writable_roots(&self) -> Vec<String> {
        vec![self.workspace.clone(), "/tmp".into()]
    }
    /// The guest's shell runs through the agent, not on this terminal: `aho shell -- <cmd>`.
    fn operator_shell(&self) -> Vec<String> {
        vec!["/bin/sh".into()]
    }
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let child = self
                .subprocess
                .spawn(aho_proc::SpawnSpec { timeout_ms: Some(10_000), ..aho_proc::SpawnSpec::new(&["/bin/sh", "-c", "test -d \"$PWD\""]) })
                .await
                .map_err(|e| e.to_string())?;
            let exit = child.wait().await.map_err(|e| e.to_string())?;
            if exit.code == 0 { Ok(()) } else { Err(format!("sandbox VM probe exited {}", exit.code)) }
        })
    }
}

impl Capability for VmWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VmWorldDeps;
    type Config = VmSettings;
    fn build(deps: VmWorldDeps, settings: VmSettings) -> Result<Self, BuildError> {
        let recipe = settings.recipe.ok_or(BuildError::Unsupported("`vm.recipe` names the sandbox VM's recipe; none is set"))?;
        VmWorld::open(deps.manager.0, &recipe, std::time::Duration::from_secs(settings.boot_timeout_s)).map_err(vm_err)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        let fs = self.guest.fs();
        r.provide(aho_fs::FsRead(fs.clone()));
        r.provide(aho_fs::FsWrite(fs));
        r.provide(aho_proc::SubprocessHandle(self.guest.subprocess()));
        r.provide(aho_term::TerminalsHandle(self.guest.terminals()));
        r.provide(aho_sandbox::SandboxHandle(self.sandbox.clone()));
        if let Ok(desktop) = self.manager.sandbox_desktop() {
            r.provide(aho_display::DesktopHandle(desktop));
        }
    }
}

declare_capability!(
    VmWorld,
    "world-vm",
    kind = Kind::Provider,
    provides = [aho_fs::SEAM, aho_proc::SEAM, aho_sandbox::SEAM, aho_term::SEAM, aho_display::SEAM],
    requires = [Requirement::Seam(aho_vm::SEAM), Requirement::Setting { key: "vm.recipe", pred: Pred::Set }],
    tools = &[],
    provides_handles = [
        HandleKind::FsRead,
        HandleKind::FsWrite,
        HandleKind::Subprocess,
        HandleKind::Terminals,
        HandleKind::Desktop,
        HandleKind::Custom("sandbox")
    ],
    exclusive = true,
    unlock = Fix::cli("aho set vm.recipe <recipe>"),
    enforced_by = ENFORCED_BY,
    doc = "The session's sandbox VM as its world (`seams.world = vm`): created from `vm.recipe` on first use and persistent; files, processes and terminals through the guest agent; its screen is the session desktop (RFB); the session's aho-net network is its only NIC (`sandbox.aho`). Checkpoints are not available in this world."
);

/// The catalog media kinds `aho media import` accepts, by name.
pub fn media_kind(name: &str) -> Option<MediaKind> {
    Some(match name {
        "raw" => MediaKind::Raw,
        "qcow2" => MediaKind::Qcow2,
        "iso" => MediaKind::Iso,
        "kernel" => MediaKind::Kernel,
        "initrd" => MediaKind::Initrd,
        "firmware" => MediaKind::Firmware,
        _ => return None,
    })
}
