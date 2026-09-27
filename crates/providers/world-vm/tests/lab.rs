//! The VM lab and the sandbox VM world end to end against a stub QEMU, so nothing here needs a
//! hypervisor: the real supervisor owns the stub, the manager speaks real QMP to it, the real
//! guest agent answers on the agent socket, and the machine's NIC plugs into a real aho-net
//! network. Covers lifecycle, console round trip, lease enforcement, naming, quotas, the
//! sandbox VM as a world (files and processes through the agent), and orphan disks for gc.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_net::{Network, NetworkConfig};
use aho_proc::SpawnSpec;
use aho_vm::{Acceleration, Architecture, LeaseLimits, MachineState, Recipe, VmError, VmLab};
use net_userspace::UserspaceNetwork;
use world_vm::{VmConfig, VmManager, VmWorld};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// The guest agent is another crate's binary: build it next to this test's supervisor.
fn agent() -> Option<PathBuf> {
    let supervisor = PathBuf::from(env!("CARGO_BIN_EXE_aho-vm-supervisor"));
    let bin = supervisor.with_file_name("vm-guest-agent");
    let cargo = std::env::var_os("CARGO")?;
    let built = std::process::Command::new(cargo).args(["build", "-q", "-p", "vm-guest-agent"]).status().ok()?.success();
    (built && bin.is_file()).then_some(bin)
}

fn private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

struct Rig {
    base: PathBuf,
    net: UserspaceNetwork,
}

impl Rig {
    fn new(tag: &str) -> Option<Rig> {
        if std::process::Command::new("python3").arg("-V").output().is_err() {
            eprintln!("skipped: the stub QEMU needs python3");
            return None;
        }
        let Some(agent) = agent() else {
            eprintln!("skipped: vm-guest-agent could not be built");
            return None;
        };
        // Short: machines listen on unix sockets inside it (`sun_path` is 104 bytes on macOS,
        // whose per-user temp dir alone takes half of that).
        let tmp = std::env::temp_dir();
        let root = if tmp.as_os_str().len() > 24 { PathBuf::from("/tmp") } else { tmp };
        let base = root.join(format!("aho-vm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        private(&base.join("net"));
        // The same values for every test in this process (they run in parallel): the stub gives
        // each machine its own guest workspace, `$AHO_STUB_ROOT/aho-<owner>-<machine>`.
        unsafe {
            std::env::set_var("AHO_STUB_AGENT", &agent);
            std::env::set_var("AHO_STUB_ROOT", stub_root());
        }
        let net =
            UserspaceNetwork::open(base.join("net"), NetworkConfig::with_prefix(format!("vm-{tag}"), if tag == "lab" { 91 } else { 92 }))
                .unwrap();
        Some(Rig { base, net })
    }
    fn config(&self, lease: LeaseLimits) -> VmConfig {
        VmConfig {
            owner_session: "5e55".into(),
            lease,
            state_dir: self.base.join("vm/state"),
            catalog_dir: self.base.join("vm"),
            qemu_img: fixture("qemu-img-stub.sh"),
            qemu_aarch64: fixture("qemu-stub.py"),
            qemu_x86_64: fixture("qemu-stub.py"),
            supervisor: env!("CARGO_BIN_EXE_aho-vm-supervisor").into(),
            sessions_dir: None,
        }
    }
    fn manager(&self, lease: LeaseLimits) -> Cleanup {
        Cleanup(Arc::new(VmManager::open(self.config(lease), Arc::new(self.net.clone())).unwrap()))
    }
}

/// Destroys every machine (and so its supervisor and stub) however the test ends.
struct Cleanup(Arc<VmManager>);
impl std::ops::Deref for Cleanup {
    type Target = Arc<VmManager>;
    fn deref(&self) -> &Arc<VmManager> {
        &self.0
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for machine in self.0.all().unwrap_or_default() {
            let _ = self.0.destroy(&machine.name);
        }
    }
}

fn stub_root() -> PathBuf {
    let tmp = std::env::temp_dir();
    let root = if tmp.as_os_str().len() > 24 { PathBuf::from("/tmp") } else { tmp };
    root.join(format!("aho-vm-guests-{}", std::process::id()))
}

/// The stub guest's workspace for `machine` (the path its recipe must name).
fn guest_workspace(machine: &str) -> PathBuf {
    stub_root().join(format!("aho-5e55-{machine}"))
}

fn lease(machines: u32) -> LeaseLimits {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    LeaseLimits {
        id: "7".into(),
        granted_ref: "42".into(),
        expires_at_ms: now + 3_600_000,
        max_machines: machines,
        max_disk_bytes: 64 << 20,
        max_cpu_s: 3600,
        revoked: false,
    }
}

fn recipe(name: &str, machine: &str) -> Recipe {
    Recipe {
        name: name.into(),
        architecture: Architecture::X86_64,
        acceleration: Acceleration::Tcg,
        memory_mib: 256,
        cpus: 1,
        disk_bytes: 16 << 20,
        base_media: None,
        cdrom_media: None,
        kernel_media: None,
        initrd_media: None,
        firmware_media: None,
        kernel_args: String::new(),
        workspace: guest_workspace(machine).to_string_lossy().into_owned(),
    }
}

fn console_until(m: &VmManager, name: &str, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let text = m.console_read(name, 0, 1 << 16).unwrap().text;
        if text.contains(needle) {
            return text;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "console never showed {needle:?}: {text:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_lab_machine_boots_under_its_lease_talks_on_its_console_and_stops() {
    let Some(rig) = Rig::new("lab") else { return };
    let m = rig.manager(lease(2));
    m.catalog().put_recipe(&recipe("tiny", "vm-db")).unwrap();

    // Names are DNS labels on the session network, and the sandbox is never a lab machine.
    for bad in ["sandbox", "_sandbox", "Bad_Name"] {
        assert!(matches!(m.create(bad, "tiny"), Err(VmError::Invalid(_))), "{bad}");
    }
    let made = m.create("vm-db", "tiny").unwrap();
    assert_eq!(made.state, MachineState::Stopped);
    assert!(std::fs::read_to_string(&made.disks[0].path).unwrap().starts_with("qcow2-stub bytes=16777216"));

    let running = m.start("vm-db").unwrap();
    assert_eq!(running.state, MachineState::Running, "QMP reports the machine running");
    console_until(&m, "vm-db", "stub boot");
    m.console_write("vm-db", b"hello guest\n").unwrap();
    console_until(&m, "vm-db", "console: hello guest");
    let nics = rig.net.status().unwrap();
    assert!(nics.nics.iter().any(|n| n.name == "vm-db"), "the machine's NIC is on the session network as vm-db.aho");

    // Quota: the second machine fits, a third does not (the lease allows two).
    m.create("vm-cache", "tiny").unwrap();
    assert!(matches!(m.create("vm-web", "tiny"), Err(VmError::Quota(_))));

    let stopped = m.stop("vm-db", false).unwrap();
    assert_eq!(stopped.state, MachineState::Stopped);
    assert!(stopped.cpu_budget_used_ms > 0, "running time is charged to the lease");
    assert_eq!(m.list().unwrap().len(), 2);

    // Orphans: while the owner lives nothing is orphaned; once it is gone, its disks are.
    assert!(m.orphan_disks(&["5e55".into()]).unwrap().is_empty());
    assert_eq!(m.orphan_disks(&[]).unwrap().len(), 2);

    // A revoked lease stops every further effect before it happens.
    m.revoke_lease().unwrap();
    for e in [m.start("vm-db").unwrap_err(), m.list().unwrap_err()] {
        assert_eq!(e.code(), "RevocationEffective", "{e}");
    }
    // Operator cleanup still works after revocation.
    m.destroy("vm-cache").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_vm_is_the_world_files_and_processes_go_through_its_guest_agent() {
    let Some(rig) = Rig::new("world") else { return };
    let m = rig.manager(lease(1));
    m.catalog().put_recipe(&recipe("sandbox-recipe", "_sandbox")).unwrap();
    let world = tokio::task::spawn_blocking({
        let m = m.0.clone();
        move || VmWorld::open(m, "sandbox-recipe", Duration::from_secs(20))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(rig.net.status().unwrap().nics.iter().any(|n| n.name == "sandbox"), "the sandbox VM is sandbox.aho");

    let fs = world.fs();
    fs.write("notes.txt", "from the host side\n", None).await.unwrap();
    assert_eq!(fs.read("notes.txt", None).await.unwrap().text, "from the host side\n");
    let guest_file = guest_workspace("_sandbox").join("notes.txt");
    assert!(guest_file.is_file(), "the write landed in the guest's workspace");

    let sp = world.subprocess();
    let child = sp.spawn(SpawnSpec::new(&["/bin/sh", "-c", "cat notes.txt; exit 3"])).await.unwrap();
    let exit = child.wait().await.unwrap();
    let (out, _, _) = child.output(0);
    assert_eq!((exit.code, String::from_utf8_lossy(&out).into_owned()), (3, "from the host side\n".to_string()));

    // The sandbox counts against the lease and cannot be taken over as a lab machine.
    assert!(matches!(m.create("vm-extra", "sandbox-recipe"), Err(VmError::Quota(_))));
    assert!(m.status("_sandbox").is_err());

    drop(world);
    let reopened = tokio::task::spawn_blocking({
        let m = m.0.clone();
        move || VmWorld::existing(m)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reopened.fs().read("notes.txt", None).await.unwrap().text, "from the host side\n", "the sandbox persisted");
    m.stop_sandbox(true).unwrap();
    m.destroy_sandbox().unwrap();
}
