//! Sandboxes under `aho gc`: a session's sandbox, network and VM machines persist
//! while its log exists, and become orphans when the log is gone or (for machines) the lease
//! ended. Another project's machines in the shared VM directory are never judged here.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::Path;

use aho_vm::{Acceleration, Architecture, LeaseLimits, Machine, MachineRole, MachineState, Recipe};
use gc::{OrphanKind, ReclaimOpts, ScanInputs, Target, reclaim, scan};

fn json(path: &Path, value: &impl serde::Serialize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn machine(owner: &str) -> Machine {
    Machine {
        name: "vm-db".into(),
        owner_session: owner.into(),
        role: MachineRole::Lab,
        recipe: Recipe {
            name: "r".into(),
            architecture: Architecture::X86_64,
            acceleration: Acceleration::Tcg,
            memory_mib: 256,
            cpus: 1,
            disk_bytes: 1 << 20,
            base_media: None,
            cdrom_media: None,
            kernel_media: None,
            initrd_media: None,
            firmware_media: None,
            kernel_args: String::new(),
            workspace: "/workspace".into(),
        },
        lease_id: "1".into(),
        state: MachineState::Stopped,
        created_at_ms: 0,
        started_at_ms: None,
        cpu_budget_used_ms: 0,
        uuid: "u".into(),
        pid: None,
        disks: vec![aho_vm::Disk {
            name: "root".into(),
            path: "/nonexistent".into(),
            bytes: 1 << 20,
            owner_session: owner.into(),
            created_at_ms: 0,
        }],
        mac: None,
    }
}

fn owner(state: &Path, name: &str, sessions: &Path, lease_ends_in_ms: i64) {
    let now = aho_types::Timestamp::now().0 as i64;
    json(&state.join(name).join("owner.json"), &sessions);
    json(
        &state.join(name).join("lease.json"),
        &LeaseLimits {
            id: "1".into(),
            granted_ref: "2".into(),
            expires_at_ms: (now + lease_ends_in_ms) as u64,
            max_machines: 2,
            max_disk_bytes: 1 << 30,
            max_cpu_s: 60,
            revoked: false,
        },
    );
    json(&state.join(name).join("machines/vm-db/machine.json"), &machine(name));
}

#[test]
fn sandboxes_outlive_turns_but_not_their_session_and_machines_end_with_their_lease() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let project = root.join("proj");
    let sessions = project.join(".aho/sessions");
    let state = root.join("vm/state");
    std::fs::create_dir_all(&sessions).unwrap();
    // Session aa is alive (its log exists); bb's log is gone.
    std::fs::write(sessions.join("aa.jsonl"), "").unwrap();
    for d in ["aa.ns", "aa.net", "bb.ns", "bb.net"] {
        std::fs::create_dir_all(sessions.join(d)).unwrap();
    }
    owner(&state, "aa", &sessions, 3_600_000); // alive, lease live: kept
    owner(&state, "a2", &sessions, -1); // log gone too, but the point is: lease ended
    std::fs::write(sessions.join("a2.jsonl"), "").unwrap(); // a2's session exists, its lease does not
    owner(&state, "bb", &sessions, 3_600_000); // session gone: orphan
    owner(&state, "cc", &root.join("other/.aho/sessions"), -1); // another project's: never judged

    let mut inputs = ScanInputs::new(&project, &sessions, root.join("shadow"), aho_types::Timestamp::now());
    inputs.vm_state = Some(state.clone());
    let report = scan(&inputs);
    let listed: Vec<(OrphanKind, String)> = report.items.iter().map(|o| (o.kind, o.owner.clone())).collect();
    assert_eq!(
        listed,
        vec![
            (OrphanKind::Sandbox, "bb".into()),
            (OrphanKind::Sandbox, "bb".into()),
            (OrphanKind::Machine, "a2".into()),
            (OrphanKind::Machine, "bb".into()),
        ],
        "{:#?}",
        report.items
    );
    assert!(report.items.iter().any(|o| o.path.contains("lease ended")));

    let out = reclaim(&report, &ReclaimOpts::default()).unwrap();
    // The namespace sandbox is Linux-only; elsewhere its destroy refuses and it stays listed as failed.
    let removed: Vec<&Target> = out.removed.iter().map(|o| &o.target).collect();
    assert!(removed.iter().any(|t| matches!(t, Target::Network { dir } if dir.ends_with("bb.net"))));
    assert!(removed.iter().filter(|t| matches!(t, Target::Machine { .. })).count() == 2, "{:#?} {:#?}", out.removed, out.failed);
    assert!(!sessions.join("bb.net").exists() && !state.join("bb/machines/vm-db").exists() && !state.join("a2/machines/vm-db").exists());
    assert!(
        sessions.join("aa.net").exists() && state.join("aa/machines/vm-db").exists() && state.join("cc/machines/vm-db").exists(),
        "the living and the foreign are untouched"
    );
}
