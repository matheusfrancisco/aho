//! `aho gc` over a fixture tree built with the real pieces: JSONL logs written by `JsonlLog`, the
//! world-host shadow repository (`ShadowGit`) for checkpoint trees, real `git worktree`s, real
//! spill files and real child processes. Each test checks what is kept, what is listed, and that
//! reclaiming removes exactly the listed items.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aho_config::{Layer, Origin, Sourced, Where};
use aho_fs::Snapshot;
use aho_log::{CheckpointCause, EntryId, EntryKind, JsonlLog, LogStore, ProcessIdentity, ToolOutcome, Transition};
use aho_proc::{ChildKind, spawned};
use aho_types::{CallId, CheckpointId, Consequence, Hash, Principal, StepId, Timestamp, TurnId};
use gc::{GcReport, OrphanKind, ReclaimOpts, ScanInputs, Target, reclaim, render_report, scan};
use serde_json::json;
use world_host::ShadowGit;

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

struct Fixture {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    sessions: PathBuf,
    shadow: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    Fixture { project, sessions: root.join("proj/.aho/sessions"), shadow: root.join("shadow"), _tmp: tmp }
}

impl Fixture {
    fn inputs(&self, now: Timestamp) -> ScanInputs {
        ScanInputs::new(&self.project, &self.sessions, &self.shadow, now)
    }
    fn log(&self, label: &str) -> JsonlLog {
        JsonlLog::open(self.sessions.join(format!("{label}.jsonl"))).unwrap()
    }
}

fn later(ms: u64) -> Timestamp {
    Timestamp(Timestamp::now().0 + ms)
}

fn sys() -> Principal {
    Principal::system("session")
}

fn append(log: &JsonlLog, kind: EntryKind) -> EntryId {
    log.append(log.head(), sys(), kind).unwrap().id
}

fn meta(process: ProcessIdentity) -> EntryKind {
    EntryKind::SessionMeta { title: None, tags: vec![], last_prompt: None, process: Some(process) }
}

fn checkpoint(n: u64, tree: Hash) -> EntryKind {
    EntryKind::WorkspaceCheckpoint { id: CheckpointId(n), tree, parent: None, cause: CheckpointCause::Manual }
}

fn config(key: &str, value: &str) -> EntryKind {
    EntryKind::ConfigResolved {
        key: key.into(),
        sourced: Sourced { value: json!(value), winner: Origin::new(Layer::FileProject, Where::Const), shadowed: vec![] },
    }
}

/// A turn that ran to completion: the session is `Closed`.
fn closed_turn(log: &JsonlLog) {
    append(log, EntryKind::TurnStart { turn: TurnId(1) });
    append(log, EntryKind::StepStart { step: StepId(1) });
    append(log, EntryKind::StepEnd { step: StepId(1), transition: Transition::Completed });
    append(log, EntryKind::TurnEnd { turn: TurnId(1) });
}

fn me() -> ProcessIdentity {
    ProcessIdentity { pid: std::process::id(), start_time: Timestamp(Timestamp::now().0 - 1_000) }
}

/// An identity whose process has exited.
fn dead() -> ProcessIdentity {
    let mut c = Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    ProcessIdentity { pid, start_time: Timestamp::now() }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgsign=false", "-c", "user.name=aho", "-c", "user.email=aho@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn has_object(repo: &Path, oid: &str) -> bool {
    Command::new("git").arg("--git-dir").arg(repo).args(["cat-file", "-e", oid]).status().unwrap().success()
}

fn kinds(r: &GcReport, kind: OrphanKind) -> Vec<String> {
    r.items.iter().filter(|o| o.kind == kind).map(|o| o.path.clone()).collect()
}

#[tokio::test]
async fn unreachable_trees_are_listed_and_reclaim_leaves_every_referenced_tree_whole() {
    let f = fixture();
    let sg = ShadowGit::new(&f.project, &f.shadow);
    let write = |p: &str, s: &str| {
        let p = f.project.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    write("a.txt", "one");
    write("shared.txt", "the same in every checkpoint");
    let t1 = sg.tree().await.unwrap();
    write("a.txt", "two");
    write("d/only.txt", "only the second checkpoint has this");
    let t2 = sg.tree().await.unwrap();
    std::fs::remove_dir_all(f.project.join("d")).unwrap();
    write("a.txt", "three");
    let t3 = sg.tree().await.unwrap();
    let repo = sg.git_dir().to_path_buf();

    // t1 only on a branch the session rewound away from, t3 on the head, t2 nowhere
    let log = f.log("a1");
    let start = append(&log, meta(dead()));
    append(&log, checkpoint(1, t1));
    log.fork(start).unwrap();
    append(&log, checkpoint(2, t3));

    let fresh = scan(&f.inputs(Timestamp::now()));
    assert!(kinds(&fresh, OrphanKind::Snapshot).is_empty(), "{}", render_report(&fresh));
    assert!(fresh.kept.iter().any(|k| k.kind == OrphanKind::Snapshot && k.why.contains("grace")), "{}", render_report(&fresh));

    let report = scan(&f.inputs(later(2 * HOUR)));
    let trees: Vec<&str> = report
        .items
        .iter()
        .filter_map(|o| match &o.target {
            Target::Snapshot { tree, .. } => Some(tree.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(trees, vec![t2.to_hex().as_str()], "{}", render_report(&report));
    let item = &report.items[0];
    assert_eq!(item.owner, "-");
    assert!(item.size.unwrap() > 0 && item.age_ms.unwrap() >= 2 * HOUR, "{item:?}");

    let out = reclaim(&report, &ReclaimOpts::default()).unwrap();
    assert_eq!((out.removed.len(), out.skipped.len(), out.failed.len()), (1, 0, 0), "{out:?}");
    assert!(out.freed > 0);
    assert!(!has_object(&repo, &t2.to_hex()), "the orphan tree is gone");
    for t in [t1, t3] {
        let listing = git(&f.project, &["--git-dir", repo.to_str().unwrap(), "ls-tree", "-r", &t.to_hex()]);
        for oid in listing.lines().filter_map(|l| l.split_whitespace().nth(2)) {
            assert!(has_object(&repo, oid), "{oid} of a referenced tree was removed");
        }
    }
    // the rewound-away checkpoint still restores
    sg.restore(t1).await.unwrap();
    assert_eq!(std::fs::read_to_string(f.project.join("a.txt")).unwrap(), "one");
    assert!(kinds(&scan(&f.inputs(later(2 * HOUR))), OrphanKind::Snapshot).is_empty());
}

#[test]
fn worktrees_of_gone_or_long_closed_sessions_are_listed_live_and_dirty_ones_kept() {
    let f = fixture();
    git(&f.project, &["init", "--quiet"]);
    std::fs::write(f.project.join("README"), "hi").unwrap();
    git(&f.project, &["add", "README"]);
    git(&f.project, &["commit", "--quiet", "-m", "init"]);
    let wt = |name: &str| {
        let p = f.project.join(".aho/worktrees").join(name);
        git(&f.project, &["worktree", "add", "--quiet", "-b", &format!("aho/{name}"), p.to_str().unwrap()]);
        p
    };
    let (done, busy, dirty, stray) = (wt("done"), wt("busy"), wt("dirty"), wt("stray"));
    std::fs::write(dirty.join("notes.txt"), "not committed").unwrap();
    let session = |label: &str, name: &str, owner: ProcessIdentity, close: bool| {
        let log = f.log(label);
        append(&log, config("workspace.layout", "worktree"));
        append(&log, config("workspace.worktree", name));
        append(&log, meta(owner));
        if close {
            closed_turn(&log);
        } else {
            append(&log, EntryKind::TurnStart { turn: TurnId(1) });
        }
    };
    session("s-done", "done", dead(), true);
    session("s-busy", "busy", me(), false);
    session("s-dirty", "dirty", dead(), true);

    let soon = scan(&f.inputs(later(HOUR)));
    assert!(kinds(&soon, OrphanKind::Worktree).is_empty(), "within the retention: {}", render_report(&soon));

    let report = scan(&f.inputs(later(8 * DAY)));
    let listed = kinds(&report, OrphanKind::Worktree);
    // rows sort by kind, owner, path: the unowned one first
    assert_eq!(listed, vec![stray.display().to_string(), done.display().to_string()], "{}", render_report(&report));
    let owners: Vec<&str> = report.items.iter().filter(|o| o.kind == OrphanKind::Worktree).map(|o| o.owner.as_str()).collect();
    assert_eq!(owners, vec!["-", "s-done"]);
    assert!(
        report.kept.iter().any(|k| k.path == dirty.display().to_string() && k.why.contains("uncommitted")),
        "{}",
        render_report(&report)
    );

    let out = reclaim(&report, &ReclaimOpts::default()).unwrap();
    assert_eq!((out.removed.len(), out.failed.len()), (2, 0), "{out:?}");
    assert!(!done.exists() && !stray.exists());
    assert!(busy.join("README").exists() && dirty.join("notes.txt").exists(), "live and dirty worktrees are untouched");
    assert!(git(&f.project, &["branch", "--list", "aho/done"]).contains("aho/done"), "the branch and its commits stay");
    assert!(!git(&f.project, &["worktree", "list"]).contains("/done"), "git no longer knows the removed worktree");
}

fn spill(dir: &Path, text: &str) -> Hash {
    let h = Hash::of(text.as_bytes());
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{}.txt", h.to_hex())), text).unwrap();
    h
}

fn result_with_spill(log: &JsonlLog, call: &str, h: Hash) {
    append(log, EntryKind::ToolCall { call: CallId(call.into()), tool: "bash".into(), args: json!({}), class: Consequence::SpawnsProcess });
    append(
        log,
        EntryKind::ToolResult {
            call: CallId(call.into()),
            outcome: ToolOutcome::Ok,
            value: json!(null),
            content: vec![],
            spill_ref: Some(h),
            meta: None,
            late: false,
        },
    );
}

#[test]
fn spill_files_no_log_names_are_listed_and_a_new_reference_saves_one() {
    let f = fixture();
    let own = f.sessions.join("e1.spill");
    let named = spill(&own, "named by e1");
    let by_child = spill(&own, "named by e1's child");
    let loose = spill(&own, "named by nobody");
    let late = spill(&own, "named only after the scan");
    std::fs::write(own.join("notes.bin"), "not a spill").unwrap();
    let orphaned_dir = spill(&f.sessions.join("gone.spill"), "its log was deleted");
    let live = spill(&f.sessions.join("live.spill"), "a running session wrote it");

    let e1 = f.log("e1");
    append(&e1, meta(dead()));
    result_with_spill(&e1, "c1", named);
    let child = JsonlLog::open(f.sessions.join("e1/7.jsonl")).unwrap();
    result_with_spill(&child, "c2", by_child);
    append(&f.log("live"), meta(me()));

    let report = scan(&f.inputs(later(2 * HOUR)));
    let file = |dir: &str, h: Hash| f.sessions.join(dir).join(format!("{}.txt", h.to_hex())).display().to_string();
    let mut expect = vec![file("e1.spill", late), file("e1.spill", loose), file("gone.spill", orphaned_dir)];
    expect.sort();
    let mut listed = kinds(&report, OrphanKind::Spill);
    listed.sort();
    assert_eq!(listed, expect, "{}", render_report(&report));
    let gone_size = "its log was deleted".len() as u64;
    assert!(report.items.iter().any(|o| o.owner == "gone" && o.size == Some(gone_size)), "{}", render_report(&report));

    // between the scan and the reclaim a result names `late`: it is no longer an orphan
    result_with_spill(&e1, "c3", late);
    let out = reclaim(&report, &ReclaimOpts::default()).unwrap();
    assert_eq!((out.removed.len(), out.skipped.len(), out.failed.len()), (2, 1, 0), "{out:?}");
    assert!(out.skipped[0].0.path.contains(&late.to_hex()));
    for (h, dir, kept) in [
        (named, "e1.spill", true),
        (by_child, "e1.spill", true),
        (late, "e1.spill", true),
        (live, "live.spill", true),
        (loose, "e1.spill", false),
        (orphaned_dir, "gone.spill", false),
    ] {
        assert_eq!(Path::new(&file(dir, h)).exists(), kept, "{dir}/{}", h.to_hex());
    }
    assert!(own.join("notes.bin").exists(), "files that are not spills are never touched");
}

#[test]
fn children_of_a_dead_owner_are_reaped_only_after_two_scans() {
    let f = fixture();
    let mut orphan = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let mut guarded = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let identity = |c: &std::process::Child| ProcessIdentity { pid: c.id(), start_time: Timestamp::now() };
    let (orphan_id, guarded_id) = (identity(&orphan), identity(&guarded));
    let ops = Principal::system(aho_proc::RECEIPT_BY);
    let crashed = f.log("crashed");
    append(&crashed, meta(dead()));
    crashed.append(crashed.head(), ops.clone(), spawned(orphan_id, ChildKind::Job, "sleep 30")).unwrap();
    // a tool may append Custom entries through `value.events`; its "receipt" names no child of ours
    crashed.append(crashed.head(), Principal::system("bash"), spawned(guarded_id, ChildKind::Job, "forged")).unwrap();
    let running = f.log("running");
    append(&running, meta(me()));
    running.append(running.head(), ops, spawned(guarded_id, ChildKind::Viewer, "viewer")).unwrap();

    // without a state file every scan is a first scan
    let mut no_state = f.inputs(Timestamp::now());
    no_state.state_file = None;
    for _ in 0..2 {
        let r = scan(&no_state);
        assert!(kinds(&r, OrphanKind::Child).is_empty());
        assert_eq!(r.pending.len(), 1, "{}", render_report(&r));
    }

    let first = scan(&f.inputs(Timestamp::now()));
    assert!(kinds(&first, OrphanKind::Child).is_empty(), "missing once is not gone: {}", render_report(&first));
    assert_eq!((first.pending[0].owner.as_str(), first.pending[0].children), ("crashed", 1));
    assert!(f.sessions.join(gc::STATE_FILE).is_file(), "the scan state persists");

    let second = scan(&f.inputs(Timestamp::now()));
    let children: Vec<&gc::Orphan> = second.items.iter().filter(|o| o.kind == OrphanKind::Child).collect();
    assert_eq!(children.len(), 1, "{}", render_report(&second));
    assert_eq!(children[0].owner, "crashed");
    assert!(matches!(children[0].target, Target::Child { child, .. } if child == orphan_id));
    assert!(render_report(&second).contains(&format!("pid {} job sleep 30", orphan_id.pid)));

    let out = reclaim(&second, &ReclaimOpts { term_grace_ms: 2_000 }).unwrap();
    assert_eq!((out.removed.len(), out.failed.len()), (1, 0), "{out:?}");
    let status = orphan.wait().unwrap();
    assert!(!status.success(), "the orphan was stopped by a signal: {status:?}");
    assert!(guarded.try_wait().unwrap().is_none(), "a live owner's child keeps running");
    guarded.kill().unwrap();
    guarded.wait().unwrap();
}

#[test]
fn a_dry_run_is_a_table_and_removes_nothing() {
    let f = fixture();
    let orphan = spill(&f.sessions.join("old.spill"), "nobody names this");
    let path = f.sessions.join("old.spill").join(format!("{}.txt", orphan.to_hex()));
    let inputs = f.inputs(later(3 * DAY));
    let text = gc::run(&inputs, true).unwrap();
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(head, ["OWNER", "KIND", "AGE", "SIZE", "PATH"], "{text}");
    let row: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(row, ["old", "spill", "3d", "17", "B", path.to_str().unwrap()], "{text}");
    assert!(path.exists(), "a dry run removes nothing");
    let text = gc::run(&inputs, false).unwrap();
    assert!(text.contains("reclaimed 1 item(s), 17 B freed"), "{text}");
    assert!(!path.exists());
    assert!(gc::run(&inputs, true).unwrap().starts_with("nothing to reclaim"));
}

#[test]
fn an_unreadable_log_stops_every_judgement_it_could_affect() {
    let f = fixture();
    let orphan = spill(&f.sessions.join("x.spill"), "maybe named by the broken log");
    let log = f.log("ok");
    append(&log, meta(dead()));
    std::fs::write(f.sessions.join("broken.jsonl"), "{not json\n").unwrap();
    let report = scan(&f.inputs(later(3 * DAY)));
    assert!(report.items.is_empty(), "{}", render_report(&report));
    assert!(report.notes.iter().any(|n| n.contains("broken.jsonl")), "{:?}", report.notes);
    assert!(report.notes.iter().any(|n| n.starts_with("spill files were not judged")), "{:?}", report.notes);
    assert!(f.sessions.join("x.spill").join(format!("{}.txt", orphan.to_hex())).exists());
}
