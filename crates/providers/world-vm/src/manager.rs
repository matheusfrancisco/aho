use crate::{Catalog, VmConfig, network, qmp, storage, supervisor};
use aho_display::{Desktop, Guard};
use aho_net::Network;
use aho_vm::{
    Acceleration, Architecture, ConsoleChunk, Disk, LeaseLimits, Machine, MachineRole, MachineState, MediaKind, OrphanDisk, VmError, VmLab,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SANDBOX_NAME: &str = "_sandbox";

/// The machine's NIC (and `<nic>.aho` DNS) name on the session network.
pub fn nic_name(machine: &str) -> String {
    if machine == SANDBOX_NAME { "sandbox".into() } else { machine.into() }
}

pub struct VmManager {
    pub(crate) config: VmConfig,
    pub(crate) session: PathBuf,
    catalog: Catalog,
    network: Arc<dyn Network>,
    proxies: Mutex<BTreeMap<String, network::Proxy>>,
}
impl VmManager {
    /// Opens persistent metadata; this never silently grants a fresh lease on resume.
    pub fn open(mut config: VmConfig, network: Arc<dyn Network>) -> Result<Self, VmError> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Err(VmError::Locked("VM worlds require Linux or macOS".into()));
        }
        storage::name(&config.owner_session)?;
        validate_lease(&config.lease)?;
        config.state_dir = storage::directory(&config.state_dir)?;
        let catalog = Catalog::open(&config.catalog_dir, &config.qemu_img)?;
        config.catalog_dir = catalog.dir().to_path_buf();
        let session = storage::directory(&config.state_dir.join(&config.owner_session))?;
        storage::directory(&session.join("machines"))?;
        {
            let _lock = storage::lock(&session.join(".lock"))?;
            let path = session.join("lease.json");
            if storage::exists(&path) {
                let _: LeaseLimits = storage::load(&path)?;
            } else {
                storage::save(&path, &config.lease)?;
            }
            // Which project's sessions own this state: `aho gc` judges only its own.
            if let Some(dir) = &config.sessions_dir {
                storage::save(&session.join("owner.json"), dir)?;
            }
        }
        let manager = Self { config, session, catalog, network, proxies: Mutex::default() };
        for machine in manager.all()? {
            if machine.started_at_ms.is_some() {
                manager.ensure_proxy(&machine.name)?;
            }
        }
        Ok(manager)
    }
    pub fn config(&self) -> &VmConfig {
        &self.config
    }
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
    pub fn lease(&self) -> Result<LeaseLimits, VmError> {
        storage::load(&self.session.join("lease.json"))
    }
    /// Operator-only: grant reference identifies the already-appended Lease/Granted entry.
    pub fn grant_lease(&self, lease: LeaseLimits) -> Result<(), VmError> {
        validate_lease(&lease)?;
        let _lock = storage::lock(&self.session.join(".lock"))?;
        storage::save(&self.session.join("lease.json"), &lease)
    }
    pub fn revoke_lease(&self) -> Result<(), VmError> {
        let _lock = storage::lock(&self.session.join(".lock"))?;
        let mut lease = self.lease()?;
        lease.revoked = true;
        storage::save(&self.session.join("lease.json"), &lease)
    }
    pub fn check_lease(&self) -> Result<(), VmError> {
        supervisor::budget(&self.session, &self.lease()?, storage::now())
    }
    pub fn guard(&self, machine: &str) -> Guard {
        guard_at(self.session.clone(), self.machine_dir(machine))
    }
    pub(crate) fn machine_dir(&self, name: &str) -> PathBuf {
        self.session.join("machines").join(name)
    }
    pub fn guest_socket(&self, name: &str) -> Result<PathBuf, VmError> {
        self.machine(name)?;
        Ok(self.machine_dir(name).join("agent.sock"))
    }
    fn machine(&self, name: &str) -> Result<Machine, VmError> {
        storage::name(name)?;
        let path = self.machine_dir(name).join("machine.json");
        if !storage::exists(&path) {
            return Err(VmError::NotFound(format!("no machine {name} belongs to session {}", self.config.owner_session)));
        }
        let machine: Machine = storage::load(&path)?;
        if machine.name != name || machine.owner_session != self.config.owner_session {
            return Err(VmError::Locked("VM ownership metadata does not match this session".into()));
        }
        Ok(machine)
    }
    pub fn all(&self) -> Result<Vec<Machine>, VmError> {
        let mut machines = Vec::new();
        for path in storage::entries(&self.session.join("machines"))? {
            if !storage::exists(&path.join("machine.json")) {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| VmError::Invalid("invalid machine directory".into()))?;
            machines.push(self.machine(name)?);
        }
        Ok(machines)
    }
    fn quota(&self, extra_disks: u64, extra_machines: u32) -> Result<(), VmError> {
        self.check_lease()?;
        let lease = self.lease()?;
        let machines = self.all()?;
        let used = machines.iter().flat_map(|m| &m.disks).fold(0u64, |total, disk| total.saturating_add(disk.bytes));
        if machines.len().saturating_add(extra_machines as usize) > lease.max_machines as usize {
            return Err(VmError::Quota(format!("lease permits {} machines including the session sandbox", lease.max_machines)));
        }
        if used.saturating_add(extra_disks) > lease.max_disk_bytes {
            return Err(VmError::Quota(format!("lease permits {} virtual disk bytes; {} already reserved", lease.max_disk_bytes, used)));
        }
        Ok(())
    }
    pub fn ensure_sandbox(&self, recipe: &str) -> Result<Machine, VmError> {
        let _lock = storage::lock(&self.session.join(".lock"))?;
        self.check_lease()?;
        if storage::exists(&self.machine_dir(SANDBOX_NAME).join("machine.json")) {
            let machine = self.machine(SANDBOX_NAME)?;
            if machine.recipe.name != recipe {
                return Err(VmError::Locked(
                    "persistent sandbox uses a different recipe; destroy it explicitly before changing recipes".into(),
                ));
            }
            return Ok(machine);
        }
        self.create_inner(SANDBOX_NAME, recipe, MachineRole::Sandbox)
    }
    fn create_inner(&self, name: &str, recipe: &str, role: MachineRole) -> Result<Machine, VmError> {
        storage::name(name)?;
        if storage::exists(&self.machine_dir(name)) {
            return Err(VmError::Invalid(format!("machine {name} already exists")));
        }
        let recipe = self.catalog.recipe(recipe)?;
        self.quota(recipe.disk_bytes, 1)?;
        let dir = storage::directory(&self.machine_dir(name))?;
        if dir.join("agent.sock").as_os_str().len() > 100 {
            return Err(VmError::Invalid(
                "VM state directory is too long for Unix sockets; configure vm.state_dir to a shorter path".into(),
            ));
        }
        let at = storage::now();
        let disk = Disk {
            name: "root".into(),
            path: dir.join("root.qcow2"),
            bytes: recipe.disk_bytes,
            owner_session: self.config.owner_session.clone(),
            created_at_ms: at,
        };
        let machine = Machine {
            name: name.into(),
            owner_session: self.config.owner_session.clone(),
            role,
            recipe,
            lease_id: self.lease()?.id,
            state: MachineState::Stopped,
            created_at_ms: at,
            started_at_ms: None,
            cpu_budget_used_ms: 0,
            uuid: uuid::Uuid::new_v4().to_string(),
            pid: None,
            disks: vec![disk.clone()],
            mac: None,
        };
        // Ownership is durable before any disk allocation, including failed provisioning.
        storage::save(&dir.join("machine.json"), &machine)?;
        self.create_disk(&disk, machine.recipe.base_media.as_deref())?;
        Ok(machine)
    }
    #[allow(clippy::disallowed_methods, reason = "authority: vm-image-create")]
    fn create_disk(&self, disk: &Disk, base: Option<&str>) -> Result<(), VmError> {
        let mut cmd = Command::new(&self.config.qemu_img);
        cmd.args(["create", "-f", "qcow2"]);
        if let Some(base) = base {
            let media = self.catalog.resolve_media(base, &[MediaKind::Raw, MediaKind::Qcow2])?;
            cmd.arg("-F").arg(if media.kind == MediaKind::Raw { "raw" } else { "qcow2" }).arg("-b").arg(media.path);
        }
        let output = cmd
            .arg(&disk.path)
            .arg(disk.bytes.to_string())
            .output()
            .map_err(|e| VmError::Locked(format!("cannot run qemu-img: {e}; install QEMU and set vm.qemu_img")))?;
        if !output.status.success() {
            return Err(VmError::Io(format!(
                "qemu-img failed; owned disk is retained for inspection: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }
    fn ensure_proxy(&self, name: &str) -> Result<(), VmError> {
        let mut proxies = self.proxies.lock().unwrap_or_else(|p| p.into_inner());
        if !proxies.contains_key(name) {
            let proxy = network::listen(&self.machine_dir(name).join("net.sock"), nic_name(name), self.network.clone())?;
            proxies.insert(name.into(), proxy);
        }
        Ok(())
    }
    pub fn sandbox_status(&self) -> Result<Machine, VmError> {
        self.status_any(SANDBOX_NAME)
    }
    fn status_any(&self, name: &str) -> Result<Machine, VmError> {
        let mut machine = self.machine(name)?;
        if machine.started_at_ms.is_some() {
            let socket = self.machine_dir(name).join("qmp.sock");
            qmp::owned(&socket, &machine.uuid)?;
            let state = qmp::command(&socket, "query-status", json!({}))?;
            machine.state = match state.get("status").and_then(|s| s.as_str()) {
                Some("running") => MachineState::Running,
                Some("paused" | "prelaunch" | "suspended") => MachineState::Paused,
                _ => MachineState::Unknown,
            };
        }
        Ok(machine)
    }
    pub fn start_sandbox(&self) -> Result<Machine, VmError> {
        self.start_any(SANDBOX_NAME)
    }
    #[allow(clippy::disallowed_methods, reason = "authority: vm-supervisor-spawn")]
    fn start_any(&self, name: &str) -> Result<Machine, VmError> {
        self.check_lease()?;
        let dir = self.machine_dir(name);
        let lock = storage::lock(&self.session.join(".lock"))?;
        let mut machine = self.machine(name)?;
        if machine.started_at_ms.is_some() {
            drop(lock);
            self.ensure_proxy(name)?;
            return self.status_any(name);
        }
        let launch = self.launch(&machine)?;
        self.ensure_proxy(name)?;
        storage::save(&dir.join("launch.json"), &launch)?;
        machine.state = MachineState::Unknown;
        machine.started_at_ms = Some(storage::now());
        storage::save(&dir.join("machine.json"), &machine)?;
        let mut cmd = Command::new(&self.config.supervisor);
        cmd.arg(dir.join("launch.json")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let spawned = cmd.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                machine.started_at_ms = None;
                machine.state = MachineState::Stopped;
                storage::save(&dir.join("machine.json"), &machine)?;
                return Err(VmError::Locked(format!(
                    "cannot launch {}: {e}; build/install aho-vm-supervisor alongside aho",
                    self.config.supervisor.display()
                )));
            }
        };
        drop(lock);
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            if let Some(status) = child.try_wait().map_err(storage::error)? {
                return Err(VmError::Io(format!(
                    "VM supervisor exited with {status}; inspect {}/failure.json and stderr.json",
                    dir.display()
                )));
            }
            if storage::exists(&dir.join("qmp.sock")) && qmp::owned(&dir.join("qmp.sock"), &machine.uuid).is_ok() {
                std::thread::spawn(move || {
                    if let Err(e) = child.wait() {
                        eprintln!("VM supervisor wait: {e}");
                    }
                });
                return self.status_any(name);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        child.kill().map_err(storage::error)?;
        child.wait().map_err(storage::error)?;
        Err(VmError::Locked("QEMU did not expose its owned QMP socket within 15 seconds; inspect stderr.json and the recipe".into()))
    }
    pub fn stop_sandbox(&self, force: bool) -> Result<Machine, VmError> {
        self.stop_any(SANDBOX_NAME, force)
    }
    /// The sandbox VM's screen: the session desktop under `world = vm`.
    pub fn sandbox_desktop(&self) -> Result<Arc<dyn Desktop>, VmError> {
        self.machine(SANDBOX_NAME)?;
        let display = vm_rfb::RfbDisplay::connect(self.machine_dir(SANDBOX_NAME).join("rfb.sock"), self.guard(SANDBOX_NAME))
            .map_err(|e| VmError::Protocol(e.to_string()))?;
        Ok(Arc::new(display))
    }
    /// Operator cleanup of the session's sandbox VM (`aho sandbox destroy`).
    pub fn destroy_sandbox(&self) -> Result<(), VmError> {
        self.destroy(SANDBOX_NAME)
    }
    /// The fixed QEMU invocation for `machine`: only what the recipe's typed fields select, one
    /// NIC on the session network, the agent port, QMP, RFB and the serial console.
    pub(crate) fn launch(&self, machine: &Machine) -> Result<supervisor::Launch, VmError> {
        let dir = self.machine_dir(&machine.name);
        let recipe = &machine.recipe;
        let mac = self
            .network
            .attach(&nic_name(&machine.name))
            .map_err(|e| VmError::Locked(format!("the session network refused the machine's NIC: {e}")))?
            .nic
            .mac;
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut a: Vec<String> = Vec::new();
        let mut push = |xs: &[&str]| a.extend(xs.iter().map(|x| x.to_string()));
        let accel = match recipe.acceleration {
            Acceleration::Tcg => "tcg",
            Acceleration::Kvm => "kvm",
            Acceleration::Hvf => "hvf",
        };
        let (machine_type, gpu) = match recipe.architecture {
            Architecture::Aarch64 => ("virt", "virtio-gpu-pci"),
            Architecture::X86_64 => ("q35", "VGA"),
        };
        push(&[
            "-nodefaults",
            "-no-user-config",
            "-name",
            &format!("aho-{}-{}", self.config.owner_session, machine.name),
            "-uuid",
            &machine.uuid,
        ]);
        push(&["-machine", machine_type, "-accel", accel, "-cpu", if recipe.acceleration == Acceleration::Tcg { "max" } else { "host" }]);
        push(&["-m", &recipe.memory_mib.to_string(), "-smp", &recipe.cpus.to_string()]);
        if cfg!(target_os = "linux") {
            push(&["-sandbox", "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny"]);
        }
        for disk in &machine.disks {
            push(&["-drive", &format!("file={},if=virtio,format=qcow2,discard=unmap", path(&disk.path))]);
        }
        if let Some(name) = &recipe.cdrom_media {
            let media = self.catalog.resolve_media(name, &[MediaKind::Iso])?;
            push(&["-drive", &format!("file={},media=cdrom,readonly=on,if=virtio", path(&media.path))]);
        }
        if let Some(name) = &recipe.firmware_media {
            push(&["-bios", &path(&self.catalog.resolve_media(name, &[MediaKind::Firmware])?.path)]);
        }
        if let Some(name) = &recipe.kernel_media {
            push(&["-kernel", &path(&self.catalog.resolve_media(name, &[MediaKind::Kernel])?.path)]);
            if let Some(initrd) = &recipe.initrd_media {
                push(&["-initrd", &path(&self.catalog.resolve_media(initrd, &[MediaKind::Initrd])?.path)]);
            }
            if !recipe.kernel_args.is_empty() {
                push(&["-append", &recipe.kernel_args]);
            }
        }
        let mac = mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
        push(&["-netdev", &format!("stream,id=net0,server=off,reconnect=1,addr.type=unix,addr.path={}", path(&dir.join("net.sock")))]);
        push(&["-device", &format!("virtio-net-pci,netdev=net0,mac={mac}")]);
        push(&["-device", "virtio-serial-pci"]);
        push(&["-chardev", &format!("socket,id=agent,path={},server=on,wait=off", path(&dir.join("agent.sock")))]);
        push(&["-device", "virtserialport,chardev=agent,name=org.aho.agent"]);
        push(&["-qmp", &format!("unix:{},server=on,wait=off", path(&dir.join("qmp.sock")))]);
        push(&["-device", gpu, "-device", "qemu-xhci", "-device", "usb-kbd", "-device", "usb-tablet"]);
        push(&["-display", "none", "-vnc", &format!("unix:{}", path(&dir.join("rfb.sock")))]);
        push(&["-serial", "stdio"]);
        Ok(supervisor::Launch {
            binary: self.config.qemu(&recipe.architecture).to_path_buf(),
            args: a,
            machine_dir: dir,
            session_dir: self.session.clone(),
        })
    }
    fn stop_any(&self, name: &str, force: bool) -> Result<Machine, VmError> {
        let machine = self.machine(name)?;
        if machine.started_at_ms.is_none() {
            return Ok(machine);
        }
        let socket = self.machine_dir(name).join("qmp.sock");
        qmp::owned(&socket, &machine.uuid)?;
        qmp::command(&socket, if force { "quit" } else { "system_powerdown" }, json!({}))?;
        let until = Instant::now() + Duration::from_secs(if force { 5 } else { 30 });
        while Instant::now() < until {
            let machine = self.machine(name)?;
            if machine.started_at_ms.is_none() {
                return Ok(machine);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(VmError::Locked("guest has not powered off; use force=true to terminate QEMU".into()))
    }
    /// Operator cleanup is permitted after revocation; normal lab effects are not.
    #[allow(clippy::disallowed_methods, reason = "authority: vm-destroy")]
    pub fn destroy(&self, name: &str) -> Result<(), VmError> {
        self.stop_any(name, true)?;
        let _lock = storage::lock(&self.session.join(".lock"))?;
        let machine = self.machine(name)?;
        if machine.started_at_ms.is_some() {
            return Err(VmError::Locked("machine is still running".into()));
        }
        self.proxies.lock().unwrap_or_else(|p| p.into_inner()).remove(name);
        std::fs::remove_dir_all(self.machine_dir(name)).map_err(storage::error)
    }
    pub fn orphan_disks(&self, live_sessions: &[String]) -> Result<Vec<OrphanDisk>, VmError> {
        let expired = self.lease()?.expires_at_ms <= storage::now();
        if live_sessions.contains(&self.config.owner_session) && !expired {
            return Ok(Vec::new());
        }
        Ok(self
            .all()?
            .into_iter()
            .flat_map(|machine| {
                machine.disks.into_iter().map(move |disk| OrphanDisk {
                    owner_session: machine.owner_session.clone(),
                    machine: machine.name.clone(),
                    path: disk.path,
                    bytes: disk.bytes,
                    created_at_ms: disk.created_at_ms,
                    lease_expired: expired,
                })
            })
            .collect())
    }
}
/// Every guest-agent and display effect first checks the lease budget and that the machine runs.
pub(crate) fn guard_at(session: PathBuf, machine_dir: PathBuf) -> Guard {
    let path = machine_dir.join("machine.json");
    Arc::new(move || {
        let lease: LeaseLimits = storage::load(&session.join("lease.json")).map_err(|e| e.to_string())?;
        supervisor::budget(&session, &lease, storage::now()).map_err(|e| e.to_string())?;
        let machine: Machine = storage::load(&path).map_err(|e| e.to_string())?;
        if machine.state != MachineState::Running {
            return Err("VM is not running; start it before using its guest agent/display".into());
        }
        Ok(())
    })
}
pub(crate) fn validate_lease(lease: &LeaseLimits) -> Result<(), VmError> {
    if lease.id.is_empty() || lease.granted_ref.is_empty() || lease.max_machines == 0 || lease.max_cpu_s == 0 || lease.max_disk_bytes == 0 {
        return Err(VmError::Invalid("VM lease needs id, already-logged granted_ref, and positive machine/disk/CPU limits".into()));
    }
    Ok(())
}
/// A lab machine's name is also its NIC name, so peers find it as `<name>.aho`.
fn lab_name(name: &str) -> Result<(), VmError> {
    storage::name(name)?;
    if name == SANDBOX_NAME || name == "sandbox" {
        return Err(VmError::Invalid("the session's sandbox VM is not a lab machine; use aho sandbox commands".into()));
    }
    if name.starts_with('-') || name.ends_with('-') || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(VmError::Invalid(format!(
            "`{name}` must be a lowercase DNS label (letters, digits, '-'): other machines reach it as {name}.aho"
        )));
    }
    Ok(())
}
impl VmLab for VmManager {
    fn check_lease(&self) -> Result<(), VmError> {
        self.check_lease()
    }
    fn recipes(&self) -> Result<Vec<aho_vm::Recipe>, VmError> {
        self.check_lease()?;
        self.catalog.recipes()
    }
    fn list(&self) -> Result<Vec<Machine>, VmError> {
        self.check_lease()?;
        Ok(self.all()?.into_iter().filter(|m| m.role == MachineRole::Lab).collect())
    }
    fn status(&self, name: &str) -> Result<Machine, VmError> {
        self.check_lease()?;
        lab_name(name)?;
        self.status_any(name)
    }
    fn create(&self, name: &str, recipe: &str) -> Result<Machine, VmError> {
        lab_name(name)?;
        let _lock = storage::lock(&self.session.join(".lock"))?;
        self.create_inner(name, recipe, MachineRole::Lab)
    }
    fn disk_create(&self, name: &str, disk: &str, bytes: u64) -> Result<Disk, VmError> {
        lab_name(name)?;
        storage::name(disk)?;
        if bytes < 1 << 20 {
            return Err(VmError::Invalid("disk must be at least 1 MiB".into()));
        }
        let _lock = storage::lock(&self.session.join(".lock"))?;
        self.quota(bytes, 0)?;
        let mut machine = self.machine(name)?;
        if machine.started_at_ms.is_some() {
            return Err(VmError::Locked("stop the VM before adding a persistent disk".into()));
        }
        if machine.disks.iter().any(|d| d.name == disk) {
            return Err(VmError::Invalid(format!("disk {disk} already exists")));
        }
        let disk = Disk {
            name: disk.into(),
            path: self.machine_dir(name).join(format!("{disk}.qcow2")),
            bytes,
            owner_session: self.config.owner_session.clone(),
            created_at_ms: storage::now(),
        };
        machine.disks.push(disk.clone());
        storage::save(&self.machine_dir(name).join("machine.json"), &machine)?;
        self.create_disk(&disk, None)?;
        Ok(disk)
    }
    fn start(&self, name: &str) -> Result<Machine, VmError> {
        lab_name(name)?;
        self.start_any(name)
    }
    fn stop(&self, name: &str, force: bool) -> Result<Machine, VmError> {
        self.check_lease()?;
        lab_name(name)?;
        self.stop_any(name, force)
    }
    fn console_read(&self, name: &str, cursor: u64, max_bytes: usize) -> Result<ConsoleChunk, VmError> {
        self.check_lease()?;
        lab_name(name)?;
        self.machine(name)?;
        let path = self.machine_dir(name).join("console.json");
        let ring: supervisor::Scrollback = if storage::exists(&path) { storage::load(&path)? } else { supervisor::Scrollback::default() };
        let offset = cursor.saturating_sub(ring.start).min(ring.data.len() as u64) as usize;
        let end = offset.saturating_add(max_bytes.clamp(1, 1 << 20)).min(ring.data.len());
        Ok(ConsoleChunk {
            text: String::from_utf8_lossy(&ring.data[offset..end]).into_owned(),
            cursor: ring.start + end as u64,
            dropped: ring.start.saturating_sub(cursor),
        })
    }
    #[allow(clippy::disallowed_methods, reason = "authority: vm-console-write")]
    fn console_write(&self, name: &str, bytes: &[u8]) -> Result<(), VmError> {
        self.check_lease()?;
        lab_name(name)?;
        self.machine(name)?;
        if bytes.len() > 65536 {
            return Err(VmError::Invalid("console write exceeds 64 KiB".into()));
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            let mut stream =
                std::os::unix::net::UnixStream::connect(self.machine_dir(name).join("console.sock")).map_err(storage::error)?;
            stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(storage::error)?;
            stream.write_all(bytes).map_err(storage::error)
        }
        #[cfg(not(unix))]
        {
            Err(VmError::Locked("console requires Unix sockets".into()))
        }
    }
    fn desktop(&self, name: &str) -> Result<Arc<dyn Desktop>, VmError> {
        self.check_lease()?;
        lab_name(name)?;
        self.machine(name)?;
        let display = vm_rfb::RfbDisplay::connect(self.machine_dir(name).join("rfb.sock"), self.guard(name))
            .map_err(|e| VmError::Protocol(e.to_string()))?;
        Ok(Arc::new(display))
    }
}
