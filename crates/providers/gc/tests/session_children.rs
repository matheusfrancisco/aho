//! A real session's job outlives the process that started it. The session's operations table writes the
//! `Child/Spawned` receipt when `bash` starts the job; the owner is killed; `aho gc` holds the
//! child back on the first scan that finds the owner gone, lists it on the second, and reaps it.
//!
//! The owner is this test binary run again as a child process (`AHO_GC_OWNER_DIR` set): it boots a
//! session over a JSONL log in the fixture, a scripted model starts `sleep` through `bash`, and it
//! waits there until it is killed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{EntryKind, JsonlLog, LogStore, ProcessIdentity};
use aho_session::{Boot, Session};
use aho_types::{Epoch, SessionId, Timestamp};
use gc::{Liveness, OrphanKind, ReclaimOpts, ScanInputs, Target, child_liveness, reclaim, render_report, scan};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

const OWNER_ENV: &str = "AHO_GC_OWNER_DIR";
const TEST: &str = "a_session_job_is_reaped_only_after_two_scans_find_its_owner_dead";
const READY: &str = "AHO_GC_OWNER_READY ";

fn sessions(root: &Path) -> PathBuf {
    root.join("proj/.aho/sessions")
}

/// The owner process: a session whose model starts a job, then nothing until this process is killed.
fn own(root: &Path) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let project = root.join("proj");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(sessions(root)).unwrap();
        Command::new("git").arg("init").arg("-q").current_dir(&project).status().unwrap();
        let comp = Composition::new("gc-owner").provide::<world_host::HostWorld>().consume::<tool_bash::ToolBash>();
        let mut schema = Schema::default();
        for k in comp.config_keys() {
            schema.add(k);
        }
        let text = format!(
            "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\n\
             shadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n",
            project.display(),
            root.join("shadow").display()
        );
        let mut inputs = ResolveInputs::default();
        inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
        let loadout = resolve(&inputs, &schema, false).unwrap();
        let mut env = Env::empty(Os::current());
        env.binaries.insert("git".into());
        let allowed = aho_policy::preset("workspace").unwrap().cut(&aho_cap::BoundWorld::host());
        let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
        let llm = Arc::new(ScriptedLlm::from_steps(vec![
            Step::tool_call("bash", json!({"command": "sleep 60"})).with_stop(ScriptedStop::ToolUse),
            Step::text("the job runs in the background").with_stop(ScriptedStop::EndTurn),
            Step::text("done"),
        ]));
        let mut seed = Registry::default();
        seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm)));
        let path = sessions(root).join("owner.jsonl");
        let log: Arc<dyn LogStore> = Arc::new(JsonlLog::open(&path).unwrap());
        let s = Session::boot(Boot {
            id: SessionId(0x0c),
            composition: &comp,
            plan: &plan,
            loadout: &loadout,
            bootstrap: vec![EntryKind::PolicyEpoch {
                epoch: Epoch(1),
                allowed: plan.allowed,
                preset: plan.preset.clone(),
                approval_ref: None,
            }],
            log: log.clone(),
            approval: Arc::new(NeverApproval),
            operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
            workspace: project.display().to_string(),
            home: home.display().to_string(),
            spill_dir: home.join("spill"),
            seed,
        })
        .await
        .unwrap();
        let turn = s.clone();
        tokio::spawn(async move {
            if let Err(e) = turn.prompt("start the job", None).await {
                eprintln!("owner turn failed: {e}");
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let pid = loop {
            if let Some(r) = log.read_from(None).iter().find_map(aho_proc::child_receipt) {
                break r.child.pid;
            }
            assert!(Instant::now() < deadline, "no Child/Spawned receipt was written for the bash job");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // on its own line: libtest has already written `test <name> ... ` without a newline
        println!("\n{READY}{pid}");
        std::future::pending::<()>().await;
    });
}

#[test]
fn a_session_job_is_reaped_only_after_two_scans_find_its_owner_dead() {
    if let Some(dir) = std::env::var_os(OWNER_ENV) {
        own(Path::new(&dir));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    std::fs::create_dir_all(root.join("proj")).unwrap();
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env(OWNER_ENV, &root)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = owner.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(pid) = line.split_once(READY).and_then(|(_, p)| p.trim().parse::<u32>().ok()) {
                let _ = tx.send(pid);
            }
        }
    });
    let ready = rx.recv_timeout(Duration::from_secs(90));
    let owner_pid = owner.id();
    owner.kill().unwrap();
    owner.wait().unwrap();
    let job_pid = ready.expect("the owner session never reported its job");

    // the receipt is in the owner's log, by system:ops, under the owner's Session/Meta
    let log = JsonlLog::open(sessions(&root).join("owner.jsonl")).unwrap();
    let entries = log.read_from(None);
    let receipts: Vec<_> = entries.iter().filter_map(aho_proc::child_receipt).collect();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    let job: ProcessIdentity = receipts[0].child;
    assert_eq!((job.pid, receipts[0].kind.as_str(), receipts[0].label.as_str()), (job_pid, "job", "bash"));
    let meta = entries.iter().rev().find_map(|e| match &e.kind {
        EntryKind::SessionMeta { process: Some(p), .. } => Some(*p),
        _ => None,
    });
    assert_eq!(meta.map(|p| p.pid), Some(owner_pid), "the receipt's owner is the killed session process");
    assert_eq!(child_liveness(&job), Liveness::Alive, "the job outlived its owner");

    let inputs = || ScanInputs::new(root.join("proj"), sessions(&root), root.join("shadow"), Timestamp::now());
    let first = scan(&inputs());
    assert!(first.items.iter().all(|o| o.kind != OrphanKind::Child), "missing once is not gone: {}", render_report(&first));
    assert_eq!(first.pending.iter().map(|p| (p.owner.as_str(), p.children)).collect::<Vec<_>>(), [("owner", 1)]);

    let second = scan(&inputs());
    let children: Vec<&gc::Orphan> = second.items.iter().filter(|o| o.kind == OrphanKind::Child).collect();
    assert_eq!(children.len(), 1, "{}", render_report(&second));
    assert!(matches!(children[0].target, Target::Child { child, .. } if child == job));
    assert!(render_report(&second).contains(&format!("pid {job_pid} job bash")), "{}", render_report(&second));

    let out = reclaim(&second, &ReclaimOpts { term_grace_ms: 2_000 }).unwrap();
    assert_eq!((out.removed.len(), out.failed.len()), (1, 0), "{out:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while child_liveness(&job) != Liveness::Gone {
        assert!(Instant::now() < deadline, "the reaped job still runs");
        std::thread::sleep(Duration::from_millis(50));
    }
}
