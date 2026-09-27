//! Operator verbs (`aho lease`, `aho sandbox`, `aho export`) and `aho gc` on a session's
//! VM state without opening a manager: a manager binds its machines' NIC sockets, which the
//! session that runs them holds. Everything here works on the persisted records under
//! `<state>/<owner>/`, under the same lock, and controls QEMU only over its owned QMP socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_vm::{LeaseLimits, Machine, VmError};
use serde_json::json;

use crate::manager::{SANDBOX_NAME, guard_at, validate_lease};
use crate::{qmp, storage};

fn owner_dir(state_dir: &Path, owner: &str) -> Result<PathBuf, VmError> {
    storage::name(owner)?;
    Ok(state_dir.join(owner))
}

/// Records `lease` as the owner's current lease (operator authority: the grant is already in the
/// session log as `granted_ref`). A running supervisor sees it on its next budget check.
pub fn set_lease(state_dir: &Path, owner: &str, lease: &LeaseLimits) -> Result<(), VmError> {
    validate_lease(lease)?;
    let dir = storage::directory(&owner_dir(state_dir, owner)?)?;
    storage::directory(&dir.join("machines"))?;
    let _lock = storage::lock(&dir.join(".lock"))?;
    storage::save(&dir.join("lease.json"), lease)
}

/// Marks the owner's lease revoked; every further VM effect and running machine stops at the
/// next check. `Ok(false)` when the owner has no lease on disk.
pub fn revoke_lease(state_dir: &Path, owner: &str) -> Result<bool, VmError> {
    let dir = owner_dir(state_dir, owner)?;
    if !storage::exists(&dir.join("lease.json")) {
        return Ok(false);
    }
    let _lock = storage::lock(&dir.join(".lock"))?;
    let mut lease: LeaseLimits = storage::load(&dir.join("lease.json"))?;
    lease.revoked = true;
    storage::save(&dir.join("lease.json"), &lease)?;
    Ok(true)
}

/// The owner's sandbox VM as recorded, if it was ever created.
pub fn sandbox_record(state_dir: &Path, owner: &str) -> Result<Option<Machine>, VmError> {
    let path = owner_dir(state_dir, owner)?.join("machines").join(SANDBOX_NAME).join("machine.json");
    if storage::exists(&path) { storage::load(&path).map(Some) } else { Ok(None) }
}

/// Stops `machine` (QMP `quit` to the QEMU whose UUID matches its record) and removes it with
/// its disks. `Ok(false)` when there is no such machine.
#[allow(clippy::disallowed_methods, reason = "authority: vm-host-destroy")]
pub fn destroy_machine(state_dir: &Path, owner: &str, machine: &str) -> Result<bool, VmError> {
    storage::name(machine)?;
    let session = owner_dir(state_dir, owner)?;
    let dir = session.join("machines").join(machine);
    if !storage::exists(&dir.join("machine.json")) {
        return Ok(false);
    }
    let record: Machine = storage::load(&dir.join("machine.json"))?;
    if record.started_at_ms.is_some() {
        let socket = dir.join("qmp.sock");
        if qmp::owned(&socket, &record.uuid).is_ok() {
            qmp::command(&socket, "quit", json!({}))?;
        }
        // The supervisor records the stop; a supervisor that is gone leaves a stale record.
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            let now: Machine = storage::load(&dir.join("machine.json"))?;
            if now.started_at_ms.is_none() || qmp::owned(&socket, &record.uuid).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _lock = storage::lock(&session.join(".lock"))?;
    std::fs::remove_dir_all(&dir).map_err(storage::error)?;
    Ok(true)
}

/// A machine nothing may use any more: its owning session's log is gone, or its lease ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineOrphan {
    pub owner: String,
    pub machine: String,
    pub dir: PathBuf,
    pub bytes: u64,
    pub created_at_ms: u64,
    pub lease_expired: bool,
}

/// Machines of owners recorded for `sessions_dir` whose log `live` does not know, or whose lease
/// is expired or revoked. Owners of other projects are never judged here.
pub fn orphans(state_dir: &Path, sessions_dir: &Path, live: &dyn Fn(&str) -> bool) -> Vec<MachineOrphan> {
    let Ok(owners) = storage::entries(state_dir) else { return Vec::new() };
    let mine = |dir: &Path| {
        // Both sides are `<project>/<session.dir>` as aho composes it, so paths compare directly.
        storage::load::<PathBuf>(&dir.join("owner.json")).is_ok_and(|recorded| recorded == sessions_dir)
    };
    let mut out = Vec::new();
    for dir in owners.into_iter().filter(|d| mine(d)) {
        let Some(owner) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        let lease: Option<LeaseLimits> = storage::load(&dir.join("lease.json")).ok();
        let expired = lease.is_none_or(|l| l.revoked || l.expires_at_ms <= storage::now());
        if live(&owner) && !expired {
            continue;
        }
        for machine_dir in storage::entries(&dir.join("machines")).unwrap_or_default() {
            let Ok(m) = storage::load::<Machine>(&machine_dir.join("machine.json")) else { continue };
            out.push(MachineOrphan {
                owner: owner.clone(),
                machine: m.name,
                dir: machine_dir,
                bytes: m.disks.iter().map(|d| d.bytes).sum(),
                created_at_ms: m.created_at_ms,
                lease_expired: expired,
            });
        }
    }
    out
}

/// The running sandbox VM's guest agent, for operator verbs. The agent serves one host channel
/// at a time: while the session holds it, this waits for the agent and then fails.
pub fn sandbox_guest(state_dir: &Path, owner: &str) -> Result<Arc<vm_guest::Guest>, VmError> {
    let session = owner_dir(state_dir, owner)?;
    let machine = sandbox_record(state_dir, owner)?
        .ok_or_else(|| VmError::NotFound("this session has no sandbox VM yet; it is created when the session first runs".into()))?;
    let dir = session.join("machines").join(SANDBOX_NAME);
    vm_guest::Guest::connect(dir.join("agent.sock"), machine.recipe.workspace.clone(), guard_at(session, dir))
        .map_err(|e| VmError::Locked(format!("the sandbox VM's guest agent is not reachable ({e}); it serves one client, so run this through the session (`aho shell -- …`) while the session uses it")))
}
