//! world-ssh against a real sshd (see `rig`): the `aho-sandbox` conformance suite, snapshots,
//! byte-exact file transfer, process lifecycle, and the Locked paths (unknown / changed host key,
//! refused login). Every test skips with a printed reason when no sshd can be started here.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod rig;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::{BuildError, Composition, Env, Os, PolicyInput, plan};
use aho_fs::{FileVersion, Fs, FsError, FsRead, FsWrite, Snapshot, SnapshotHandle};
use aho_proc::{Child, Exit, ProcError, SpawnSpec, Subprocess, SubprocessHandle};
use aho_sandbox::SandboxHandle;
use rig::Rig;
use world_ssh::SshWorld;

fn world(rig: &Rig, ws: &str) -> SshWorld {
    SshWorld::connect(rig.config(ws)).unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn run(p: &dyn Subprocess, spec: SpawnSpec) -> (Exit, String) {
    let child = p.spawn(spec).await.unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(20), child.wait()).await.unwrap().unwrap();
    (exit, drain(child.as_ref()).await)
}

async fn drain(child: &dyn Child) -> String {
    let (mut cursor, mut out) = (0u64, Vec::new());
    loop {
        let (bytes, next, done) = child.output(cursor);
        out.extend(bytes);
        cursor = next;
        if done {
            return String::from_utf8_lossy(&out).into_owned();
        }
        child.readable(cursor).await;
    }
}

async fn sh(p: &dyn Subprocess, script: &str) -> (Exit, String) {
    run(p, SpawnSpec::new(&["/bin/sh", "-c", script])).await
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {$(
        #[tokio::test]
        async fn $case() {
            let Some(rig) = Rig::start() else { return };
            let w = world(&rig, "ws");
            let r = aho_sandbox::run_case(stringify!($case), w.fs(), w.subprocess().as_ref(), w.sandbox().as_ref()).await;
            assert!(r.is_ok(), "{}: {r:?}", stringify!($case));
        }
    )*};
}

// One named test per `aho_sandbox::CONFORMANCE` case; `the_whole_suite_passes_and_leaves_no_scratch`
// runs the list itself, so a case added there is exercised even before it gets a name here.
conformance!(
    probe_succeeds,
    bound_world_reports_its_enforcement_honestly,
    writable_roots_start_with_the_workspace_root,
    exit_status_passes_through_including_128_plus_signal,
    a_waited_read_returns_when_the_process_ends_not_when_its_descendant_does,
    killing_a_child_kills_its_whole_process_group,
    a_timeout_kills_the_whole_process_group,
    fs_fence_refuses_a_path_outside_the_workspace,
    fs_fence_refuses_a_symlink_pointing_outside,
    fs_fence_refuses_a_dangling_symlink,
    write_is_atomic_and_versioned,
    edit_refuses_a_non_unique_match_and_a_stale_version,
    glob_and_grep_honor_gitignore,
);

#[tokio::test]
async fn the_whole_suite_passes_and_leaves_no_scratch() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let results = aho_sandbox::run_conformance(w.fs(), w.subprocess().as_ref(), w.sandbox().as_ref()).await;
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
    assert_eq!(std::fs::read_dir(rig.path("ws")).unwrap().count(), 0);
}

#[tokio::test]
async fn snapshots_round_trip_in_a_remote_shadow_store_and_leave_the_users_git_alone() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let (exit, text) = sh(w.subprocess().as_ref(), "git init -q && ls .git | wc -l").await;
    assert_eq!(exit.code, 0, "{text}");
    let before = std::fs::read_dir(rig.path("ws/.git")).unwrap().count();

    let results = aho_sandbox::run_snapshot_conformance(w.fs(), w.snapshot()).await;
    for (name, r) in &results {
        assert!(r.is_ok(), "{name}: {r:?}");
    }
    assert!(w.snapshot().git_dir().starts_with(&*rig.path("shadow").to_string_lossy()), "{}", w.snapshot().git_dir());
    assert!(std::path::Path::new(w.snapshot().git_dir()).join("HEAD").exists(), "the shadow store lives on the remote side");
    assert!(!rig.path("ws/.git/index").exists(), "the user's index was never written");
    assert_eq!(std::fs::read_dir(rig.path("ws/.git")).unwrap().count(), before);
}

#[tokio::test]
async fn restore_deletes_new_files_prunes_their_directories_and_skips_dot_aho() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let fs = w.fs();
    fs.write("keep.txt", "k", None).await.unwrap();
    fs.write(".aho/sessions/log.jsonl", "{}", None).await.unwrap();
    let t0 = w.snapshot().tree().await.unwrap();
    fs.write("new/deep/x.txt", "x", None).await.unwrap();
    fs.write(".aho/sessions/log.jsonl", "{}\n{}", None).await.unwrap();
    let d = w.snapshot().restore(t0).await.unwrap();
    assert_eq!(d.deleted, vec!["new/deep/x.txt".to_string()]);
    assert!(d.changed.is_empty(), "{d:?}");
    assert!(!rig.path("ws/new").exists(), "emptied directories are pruned");
    assert_eq!(fs.read(".aho/sessions/log.jsonl", None).await.unwrap().text, "{}\n{}", ".aho/ is never snapshotted or restored");
    assert!(
        matches!(w.snapshot().restore(aho_types::Hash::of(b"nope")).await, Err(FsError::Invalid(m)) if m.contains("not in the shadow store"))
    );
}

#[tokio::test]
async fn bytes_survive_the_channel_exactly() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let fs = w.fs();
    // All 256 byte values, written remotely, read back through the fs seam.
    let (exit, text) = sh(w.subprocess().as_ref(), r#"perl -e 'print map { chr } 0 .. 255' > all.bin"#).await;
    assert_eq!(exit.code, 0, "{text}");
    assert_eq!(fs.read_bytes("all.bin").await.unwrap(), (0u8..=255).collect::<Vec<_>>());
    assert!(matches!(fs.read("all.bin", None).await, Err(FsError::Invalid(_))), "not UTF-8");

    let tricky = "it's a \\back\\slash, $HOME `id` $(id)\r\nünïcödé\n\0nul and no trailing newline";
    let name = "dir with space/it's \"quoted\" $x.txt";
    let d = fs.write(name, tricky, None).await.unwrap();
    assert!(d.created);
    assert_eq!(d.new_version, FileVersion::of(tricky.as_bytes()));
    assert_eq!(std::fs::read(rig.path("ws").join(name)).unwrap(), tricky.as_bytes(), "byte-exact on the remote disk");
    let r = fs.read(name, None).await.unwrap();
    assert_eq!((r.text.as_str(), r.version), (tricky, d.new_version));
    let st = fs.stat(name).await.unwrap();
    assert_eq!((st.is_dir, st.bytes, st.version), (false, tricky.len() as u64, Some(d.new_version)));
    let listed: Vec<String> = fs.list("dir with space").await.unwrap().into_iter().map(|s| s.path).collect();
    assert_eq!(listed, vec![name.to_string()]);
    let partial = fs.read(name, Some((2, 2))).await.unwrap();
    assert_eq!((partial.text.as_str(), partial.total_lines, partial.partial), ("ünïcödé\n", 3, true));
}

#[tokio::test]
async fn edit_and_write_keep_the_mode_and_refuse_after_an_outside_change() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let (fs, p) = (w.fs(), w.subprocess());
    let v = fs.write("run.sh", "#!/bin/sh\necho one\n", None).await.unwrap().new_version;
    assert_eq!(sh(p.as_ref(), "chmod 755 run.sh").await.0.code, 0);
    let d = fs.edit("run.sh", "one", "two", false, v).await.unwrap();
    assert_eq!(d.hunks.len(), 1);
    let (exit, out) = run(p.as_ref(), SpawnSpec::new(&["./run.sh"])).await;
    assert_eq!((exit.code, out.as_str()), (0, "two\n"), "still executable after the atomic replace");

    // Someone else changes the file behind the agent's back.
    assert_eq!(sh(p.as_ref(), "printf changed > run.sh").await.0.code, 0);
    match fs.write("run.sh", "mine", Some(d.new_version)).await {
        Err(FsError::StaleVersion { actual, .. }) => assert_eq!(actual, FileVersion::of(b"changed")),
        other => panic!("expected a stale refusal, got {other:?}"),
    }
    assert!(matches!(fs.edit("run.sh", "changed", "x", false, d.new_version).await, Err(FsError::StaleVersion { .. })));
    assert_eq!(std::fs::read_to_string(rig.path("ws/run.sh")).unwrap(), "changed");
    assert!(matches!(fs.write("missing.txt", "x", Some(v)).await, Err(FsError::NotFound(_))));
    assert!(matches!(fs.read(".", None).await, Err(FsError::Invalid(_))));
    fs.remove("run.sh").await.unwrap();
    assert!(matches!(fs.stat("run.sh").await, Err(FsError::NotFound(_))));
    assert!(matches!(fs.remove(".").await, Err(FsError::Invalid(_))));
}

#[tokio::test]
async fn spawn_passes_env_cwd_stdin_and_exit_codes_and_reports_missing_programs() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let p = w.subprocess();
    w.fs().write("sub/.keep", "", None).await.unwrap();
    let spec = SpawnSpec {
        cwd: Some("sub".into()),
        env: vec![("AHO_TEST".into(), "a b 'c'".into())],
        stdin: Some(b"from stdin\n".to_vec()),
        ..SpawnSpec::new(&["/bin/sh", "-c", "pwd -P; printf '%s\\n' \"$AHO_TEST\"; cat; exit 7"])
    };
    let (exit, out) = run(p.as_ref(), spec).await;
    let root = w.root().to_string();
    assert_eq!(exit, Exit { code: 7, signal: None });
    assert_eq!(out, format!("{root}/sub\na b 'c'\nfrom stdin\n"));

    let err = p.spawn(SpawnSpec::new(&["definitely-not-a-program-aho"])).await.err();
    assert_eq!(err, Some(ProcError::NotFound("definitely-not-a-program-aho".into())));
    let spec = SpawnSpec { cwd: Some("nowhere".into()), ..SpawnSpec::new(&["true"]) };
    assert!(matches!(p.spawn(spec).await.err(), Some(ProcError::NotFound(m)) if m.contains("nowhere")));
}

#[tokio::test]
async fn cancel_kills_the_remote_group_and_interactive_stdin_stays_open() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let p = w.subprocess();

    let child = p.spawn_interactive(SpawnSpec::new(&["cat"])).await.unwrap();
    child.write_stdin(b"ping\n".to_vec()).await.unwrap();
    let mut cursor = 0;
    let mut text = String::new();
    while !text.contains("ping\n") {
        tokio::time::timeout(Duration::from_secs(10), child.readable(cursor)).await.unwrap();
        let (bytes, next, _) = child.output(cursor);
        text.push_str(&String::from_utf8_lossy(&bytes));
        cursor = next;
    }
    let pid = child.identity().pid;
    child.kill().await.unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    assert_eq!(exit.signal, Some(15), "{exit:?}");
    let (alive, _) = sh(p.as_ref(), &format!("kill -0 {pid} 2>/dev/null")).await;
    assert_ne!(alive.code, 0, "the remote leader {pid} is gone");

    // Dropping a running child kills it too.
    let child = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "echo $$; exec sleep 30"])).await.unwrap();
    let pid = child.identity().pid;
    drop(child);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (alive, _) = sh(p.as_ref(), &format!("kill -0 {pid} 2>/dev/null")).await;
        if alive.code != 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "a dropped child {pid} kept running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn glob_and_grep_follow_nested_ignores_links_includes_and_file_bases() {
    let Some(rig) = Rig::start() else { return };
    let w = world(&rig, "ws");
    let fs = w.fs();
    for (f, t) in [
        (".gitignore", "*.log\nbuild/\n"),
        ("a.rs", "fn main() { needle(); }\n"),
        ("x.log", "needle\n"),
        ("build/out.rs", "needle\n"),
        ("src/.gitignore", "!keep.log\n"),
        ("src/keep.log", "needle here\n"),
        ("src/lib.rs", "// no match\n"),
        ("src/.ignore", "gen.rs\n"),
        ("src/gen.rs", "needle\n"),
    ] {
        fs.write(f, t, None).await.unwrap();
    }
    assert_eq!(sh(w.subprocess().as_ref(), "ln -s a.rs alias.rs && ln -s / outside && ln -s src srclink").await.0.code, 0);

    let files = fs.glob("**/*.rs", None).await.unwrap();
    assert_eq!(files, vec!["a.rs".to_string(), "alias.rs".into(), "src/lib.rs".into()]);
    let hits: Vec<(String, u32)> = fs.grep("needle", None, None).await.unwrap().into_iter().map(|h| (h.path, h.line)).collect();
    assert_eq!(hits, vec![("a.rs".to_string(), 1), ("alias.rs".into(), 1), ("src/keep.log".into(), 1)]);
    let only_rs: Vec<String> = fs.grep("needle", None, Some("*.rs")).await.unwrap().into_iter().map(|h| h.path).collect();
    assert_eq!(only_rs, vec!["a.rs".to_string(), "alias.rs".into()]);
    let one: Vec<String> = fs.grep("needle", Some("src/keep.log"), None).await.unwrap().into_iter().map(|h| h.text).collect();
    assert_eq!(one, vec!["needle here".to_string()]);
    assert_eq!(
        fs.glob("*.rs", Some("srclink")).await.unwrap(),
        vec!["src/lib.rs".to_string()],
        "an in-root link resolves to canonical paths"
    );
    assert!(matches!(fs.glob("*", Some("a.rs")).await, Err(FsError::Invalid(_))));
    assert!(matches!(fs.glob("*", Some("outside")).await, Err(FsError::OutsideWorkspace(_))));
    assert!(matches!(fs.grep("(", None, None).await, Err(FsError::Invalid(_))));
}

#[tokio::test]
async fn an_unknown_host_key_locks_the_world_with_a_keyscan_fix() {
    let Some(rig) = Rig::start() else { return };
    std::fs::write(rig.path("empty_known_hosts"), "").unwrap();
    let mut cfg = rig.config("ws");
    cfg.target.known_hosts = Some(rig.path("empty_known_hosts"));
    let err = match SshWorld::connect(cfg) {
        Err(BuildError::Probe(m)) => m,
        Err(e) => panic!("wrong error {e}"),
        Ok(_) => panic!("connected to a host whose key is not known"),
    };
    assert!(err.starts_with("Locked:"), "{err}");
    assert!(err.contains(&format!("ssh-keyscan -p {} 127.0.0.1 >> {}", rig.port, rig.path("empty_known_hosts").display())), "{err}");
    assert!(err.contains("StrictHostKeyChecking"), "{err}");
    assert!(!rig.path("ws").exists(), "nothing ran on the remote");
    assert_eq!(std::fs::read_to_string(rig.path("empty_known_hosts")).unwrap(), "", "the key was not silently accepted");

    // A different key recorded for the host: changed, not unknown.
    let other = rig.path("other");
    let st = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&other).status().unwrap();
    assert!(st.success());
    let pubkey = std::fs::read_to_string(rig.path("other.pub")).unwrap();
    std::fs::write(rig.path("wrong_known_hosts"), format!("[127.0.0.1]:{} {pubkey}", rig.port)).unwrap();
    let mut cfg = rig.config("ws");
    cfg.target.known_hosts = Some(rig.path("wrong_known_hosts"));
    match SshWorld::connect(cfg) {
        Err(BuildError::Probe(m)) => assert!(m.starts_with("Locked:") && m.contains("ssh-keygen -R"), "{m}"),
        Err(e) => panic!("wrong error {e}"),
        Ok(_) => panic!("connected despite a changed host key"),
    }
}

#[tokio::test]
async fn a_refused_login_is_locked_with_the_identity_fix() {
    let Some(rig) = Rig::start() else { return };
    let stranger = rig.path("stranger");
    let st = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&stranger).status().unwrap();
    assert!(st.success());
    let mut cfg = rig.config("ws");
    cfg.target.identity = Some(stranger);
    match SshWorld::connect(cfg) {
        Err(BuildError::Probe(m)) => assert!(m.starts_with("Locked:") && m.contains("world.ssh.identity"), "{m}"),
        Err(e) => panic!("wrong error {e}"),
        Ok(_) => panic!("logged in with a key that is not authorized"),
    }
}

#[tokio::test]
async fn starts_through_a_composition_and_the_master_goes_away_with_the_world() {
    let Some(rig) = Rig::start() else { return };
    let comp = Composition::new("t").provide::<SshWorld>();
    let mut schema = aho_config::Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let cfg = rig.config("ws");
    let sets = [
        ("seams.world", "ssh".to_string()),
        ("world.ssh.host", cfg.target.host.clone()),
        ("world.ssh.port", rig.port.to_string()),
        ("world.ssh.identity", rig.path("client").to_string_lossy().into_owned()),
        ("world.ssh.config", rig.path("ssh_config").to_string_lossy().into_owned()),
        ("world.ssh.known_hosts", rig.path("known_hosts").to_string_lossy().into_owned()),
        ("world.ssh.workspace", cfg.workspace.clone()),
        ("world.ssh.shadow_dir", cfg.shadow_dir.clone()),
        ("world.ssh.control_dir", rig.path("c").to_string_lossy().into_owned()),
    ];
    let mut inputs = aho_config::ResolveInputs::default();
    inputs.sets = sets.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let l = aho_config::resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("ssh".into());
    let policy = PolicyInput { preset: "workspace".into(), allowed: aho_types::ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    assert_eq!(p.world.enforced_by, world_ssh::ENFORCED_BY);
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.handle::<FsRead>().is_some() && reg.handle::<FsWrite>().is_some() && reg.handle::<SubprocessHandle>().is_some());
    let egress = reg.handle::<aho_egress::EgressHandle>().unwrap();
    assert_eq!(egress.0.enforced_by(), "remote host");
    let snap = reg.handle::<SnapshotHandle>().unwrap();
    let sb = reg.handle::<SandboxHandle>().unwrap();
    assert_eq!(sb.0.bound(), p.world, "the sandbox reports exactly what the plan prints");
    let root = std::fs::canonicalize(rig.path("ws")).unwrap().to_string_lossy().into_owned();
    assert_eq!(sb.0.writable_roots(), vec![root.clone()]);
    sb.0.probe().await.unwrap();
    snap.0.tree().await.unwrap();

    // `aho shell`: a pty on the remote, in the workspace, running the remote login shell.
    let shell = sb.0.operator_shell();
    let mut child = tokio::process::Command::new(&shell[0])
        .args(&shell[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stdin, b"pwd -P\nexit\n").await.unwrap();
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    // A login shell may decorate the line with prompt/title escapes; `pwd -P` still ends it with the root.
    assert!(text.contains(&format!("{root}\n")), "the operator shell did not land in {root}: {text:?}");
    drop(stdin);

    let sockets = || std::fs::read_dir(rig.path("c")).unwrap().count();
    assert_eq!(sockets(), 1, "one ControlMaster for the world");
    drop((reg, snap, sb, egress));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sockets() != 0 {
        assert!(std::time::Instant::now() < deadline, "the ControlMaster outlived the world");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn concurrent_operations_share_one_master() {
    let Some(rig) = Rig::start() else { return };
    let w = Arc::new(world(&rig, "ws"));
    let mut tasks = Vec::new();
    for i in 0..16 {
        let w = w.clone();
        tasks.push(tokio::spawn(async move {
            let name = format!("f{i}.txt");
            w.fs().write(&name, &format!("{i}\n"), None).await.unwrap();
            let (exit, out) = run(w.subprocess().as_ref(), SpawnSpec::new(&["cat", &name])).await;
            assert_eq!((exit.code, out), (0, format!("{i}\n")));
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(w.fs().glob("f*.txt", None).await.unwrap().len(), 16);
    assert_eq!(std::fs::read_dir(rig.path("c")).unwrap().count(), 1);
}
