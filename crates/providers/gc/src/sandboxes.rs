//! Sandboxes: a session's namespace sandbox (`<id>.ns`), its network (`<id>.net`), and its
//! VM machines (under `vm.dir/state/<id>`). They persist on purpose while the session exists
//! (only `aho sandbox destroy` or the end of a VM lease retires them), so a sandbox is an
//! orphan only when its session's log is gone, and a machine also when its lease has ended.
//! Machines of other projects' sessions share `vm.dir`; each owner records its session
//! directory, and only this one's are judged.

use std::path::{Path, PathBuf};

use crate::logs::Sessions;
use crate::{GcReport, Orphan, OrphanKind, ScanInputs, Target, age_of};

#[allow(clippy::disallowed_methods, reason = "authority: gc-sandbox-scan")]
pub(crate) fn judge(inputs: &ScanInputs, sessions: &Sessions, report: &mut GcReport) {
    let has_log = |label: &str| sessions.by_label(label).is_some() || inputs.session_dir.join(format!("{label}.jsonl")).is_file();
    if !sessions.unreadable.is_empty() {
        report.notes.push(format!("sandboxes were not judged: {} session log(s) could not be read", sessions.unreadable.len()));
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&inputs.session_dir) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let (Some(label), Some(ext)) =
                (dir.file_stem().map(|s| s.to_string_lossy().into_owned()), dir.extension().and_then(|e| e.to_str()))
            else {
                continue;
            };
            let target = match ext {
                "ns" => Target::Namespace { dir: dir.clone() },
                "net" => Target::Network { dir: dir.clone() },
                _ => continue,
            };
            if has_log(&label) {
                continue;
            }
            report.items.push(Orphan {
                owner: label,
                kind: OrphanKind::Sandbox,
                age_ms: age_of(&dir, inputs.now),
                size: None,
                path: dir.display().to_string(),
                target,
            });
        }
    }
    let Some(state) = &inputs.vm_state else { return };
    for m in world_vm::orphans(state, &inputs.session_dir, &|owner| has_log(owner)) {
        let why = if m.lease_expired { "lease ended" } else { "session gone" };
        report.items.push(Orphan {
            owner: m.owner.clone(),
            kind: OrphanKind::Machine,
            age_ms: Some(inputs.now.0.saturating_sub(m.created_at_ms)),
            size: Some(m.bytes),
            path: format!("{} ({why})", m.dir.display()),
            target: Target::Machine { state: state.clone(), owner: m.owner, machine: m.machine },
        });
    }
}

pub(crate) fn reclaim(target: &Target) -> Result<(), String> {
    match target {
        Target::Namespace { dir } => world_ns::destroy(dir)
            .map(|_| ())
            .map_err(|e| format!("could not destroy the sandbox {} ({e}); it is still there", dir.display())),
        Target::Network { dir } => net_userspace::remove_stopped(dir)
            .map(|_| ())
            .map_err(|e| format!("could not remove the network {} ({e}); it is still there", dir.display())),
        Target::Machine { state, owner, machine } => world_vm::destroy_machine(state, owner, machine)
            .map(|_| ())
            .map_err(|e| format!("could not destroy machine {machine} of {owner} ({e}); it is still there")),
        _ => Err("not a sandbox".into()),
    }
}

/// The VM state root for `home` when `vm.dir` is unset.
pub fn default_vm_state(home: &Path) -> PathBuf {
    home.join(".aho").join("vm").join("state")
}
