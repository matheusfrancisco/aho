//! A persistent watchdog owns QEMU, so leases still expire while the daemon is disconnected.
use crate::storage;
use aho_vm::{LeaseLimits, Machine, MachineState, VmError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Launch {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub machine_dir: PathBuf,
    pub session_dir: PathBuf,
}
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Scrollback {
    pub start: u64,
    pub data: Vec<u8>,
}
impl Scrollback {
    fn push(&mut self, bytes: &[u8], capacity: usize) {
        let total = self.data.len().saturating_add(bytes.len());
        if total > capacity {
            let drop = total - capacity;
            self.start = self.start.saturating_add(drop as u64);
            if drop >= self.data.len() {
                self.data.clear();
                self.data.extend_from_slice(&bytes[bytes.len().saturating_sub(capacity)..]);
                return;
            }
            self.data.drain(..drop);
        }
        self.data.extend_from_slice(bytes);
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: vm-supervisor-output")]
fn capture(mut stream: impl Read + Send + 'static, path: PathBuf, capacity: usize) {
    std::thread::spawn(move || {
        let mut ring: Scrollback = if storage::exists(&path) {
            match storage::load(&path) {
                Ok(ring) => ring,
                Err(e) => {
                    eprintln!("VM scrollback: {e}");
                    return;
                }
            }
        } else {
            Scrollback::default()
        };
        let mut bytes = [0u8; 8192];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => return,
                Ok(n) => {
                    ring.push(&bytes[..n], capacity);
                    if let Err(e) = storage::save(&path, &ring) {
                        eprintln!("VM scrollback: {e}");
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("VM console: {e}");
                    return;
                }
            }
        }
    });
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: vm-supervisor-console")]
fn console(path: &Path, stdin: std::process::ChildStdin, stop: Arc<AtomicBool>) -> Result<(), VmError> {
    use std::os::unix::net::UnixListener;
    if storage::exists(path) {
        std::fs::remove_file(path).map_err(storage::error)?;
    }
    let listener = UnixListener::bind(path).map_err(storage::error)?;
    listener.set_nonblocking(true).map_err(storage::error)?;
    let stdin = Arc::new(Mutex::new(stdin));
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // BSD sockets inherit the listener's O_NONBLOCK on accept; Linux does not. Both
                    // calls are tuning only: macOS refuses them (EINVAL) once the writer already
                    // closed, and its buffered bytes must still reach the guest.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let stdin = stdin.clone();
                    std::thread::spawn(move || {
                        let mut bytes = [0u8; 8192];
                        loop {
                            match stream.read(&mut bytes) {
                                Ok(0) => return,
                                Ok(n) => {
                                    let mut input = stdin.lock().unwrap_or_else(|p| p.into_inner());
                                    if let Err(e) = input.write_all(&bytes[..n]).and_then(|()| input.flush()) {
                                        eprintln!("VM console: {e}");
                                        return;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("VM console: {e}");
                                    return;
                                }
                            }
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    eprintln!("VM console accept: {e}");
                    return;
                }
            }
        }
    });
    Ok(())
}

pub(crate) fn budget(session: &Path, lease: &LeaseLimits, now: u64) -> Result<(), VmError> {
    let expired = || VmError::RevocationEffective { lease: lease.id.clone(), reason: "expired, revoked, or CPU budget exhausted".into() };
    if lease.revoked || now >= lease.expires_at_ms {
        return Err(expired());
    }
    let mut used = 0u64;
    for path in storage::entries(&session.join("machines"))? {
        let state = path.join("machine.json");
        if !storage::exists(&state) {
            continue;
        }
        let m: Machine = storage::load(&state)?;
        let elapsed = m.started_at_ms.map(|at| now.saturating_sub(at).saturating_mul(u64::from(m.recipe.cpus))).unwrap_or(0);
        used = used.saturating_add(m.cpu_budget_used_ms).saturating_add(elapsed);
    }
    if used >= lease.max_cpu_s.saturating_mul(1000) {
        return Err(expired());
    }
    Ok(())
}

/// Entry point for the installed `aho-vm-supervisor` binary. Never executes guest commands.
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: vm-supervisor-launch")]
pub fn supervise(launch_path: &Path) -> Result<(), VmError> {
    let launch: Launch = storage::load(launch_path)?;
    let machine_path = launch.machine_dir.join("machine.json");
    let lease_path = launch.session_dir.join("lease.json");
    let lock_path = launch.session_dir.join(".lock");
    let lease: LeaseLimits = storage::load(&lease_path)?;
    budget(&launch.session_dir, &lease, storage::now())?;
    let mut child = Command::new(&launch.binary)
        .args(&launch.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(storage::error)?;
    let stopping = Arc::new(AtomicBool::new(false));
    let result = (|| {
        let stdin = child.stdin.take().ok_or_else(|| VmError::Io("QEMU stdin pipe missing".into()))?;
        console(&launch.machine_dir.join("console.sock"), stdin, stopping.clone())?;
        capture(
            child.stdout.take().ok_or_else(|| VmError::Io("QEMU output pipe missing".into()))?,
            launch.machine_dir.join("console.json"),
            1 << 20,
        );
        capture(
            child.stderr.take().ok_or_else(|| VmError::Io("QEMU error pipe missing".into()))?,
            launch.machine_dir.join("stderr.json"),
            65536,
        );
        {
            let _lock = storage::lock(&lock_path)?;
            let mut machine: Machine = storage::load(&machine_path)?;
            machine.pid = Some(child.id());
            machine.started_at_ms = Some(storage::now());
            machine.state = MachineState::Running;
            storage::save(&machine_path, &machine)?;
        }
        loop {
            if let Some(status) = child.try_wait().map_err(storage::error)? {
                if status.success() {
                    return Ok(());
                }
                return Err(VmError::Io(format!("QEMU exited with {status}; inspect stderr.json")));
            }
            let lease: LeaseLimits = storage::load(&lease_path)?;
            budget(&launch.session_dir, &lease, storage::now())?;
            std::thread::sleep(Duration::from_millis(200));
        }
    })();
    stopping.store(true, Ordering::Release);
    match child.try_wait().map_err(storage::error)? {
        Some(_) => {}
        None => {
            child.kill().map_err(storage::error)?;
            child.wait().map_err(storage::error)?;
        }
    }
    let _lock = storage::lock(&lock_path)?;
    let mut machine: Machine = storage::load(&machine_path)?;
    if let Some(started) = machine.started_at_ms.take() {
        machine.cpu_budget_used_ms = machine
            .cpu_budget_used_ms
            .saturating_add(storage::now().saturating_sub(started).saturating_mul(u64::from(machine.recipe.cpus)));
    }
    machine.state = MachineState::Stopped;
    machine.pid = None;
    storage::save(&machine_path, &machine)?;
    if let Err(e) = &result {
        storage::save(&launch.machine_dir.join("failure.json"), &e.to_string())?;
    }
    result
}
#[cfg(not(unix))]
pub fn supervise(_: &Path) -> Result<(), VmError> {
    Err(VmError::Locked("VM supervision requires Linux or macOS".into()))
}
