//! `07` — the policy hinge. A *world* is `Fs + Subprocess + Sandbox` bound as one exclusive
//! bundle. The `enforced_by` string a world reports is printed by `aho plan`; `policy only` is
//! an honest value. `run_conformance` is the suite every world must pass (15).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use aho_cap::{BoundWorld, Handle, HandleKind, SeamId};
use aho_fs::{FileVersion, Fs, FsError, Snapshot};
use aho_proc::{Child, Exit, ProcError, SpawnSpec, Subprocess};
use aho_types::*;

pub const SEAM: SeamId = SeamId("sandbox");

pub trait Sandbox: Send + Sync {
    /// Name, whether it confines, and what enforces it — exactly what `aho plan` prints.
    fn bound(&self) -> BoundWorld;
    /// Canonical writable roots shared by the kernel profile and the in-process fs fence, so
    /// "bash can write /tmp but `write` cannot" cannot happen (10). The workspace root is first.
    fn writable_roots(&self) -> Vec<String>;
    /// Functional probe: run `true` in the world. Fail closed (10).
    fn probe(&self) -> BoxFuture<'_, Result<(), String>>;
    /// argv for an interactive operator shell in the world (`aho shell`); run it with the
    /// workspace root (`writable_roots()[0]`) as cwd.
    fn operator_shell(&self) -> Vec<String>;
}

#[derive(Clone)]
pub struct SandboxHandle(pub Arc<dyn Sandbox>);
impl Handle for SandboxHandle {
    const KIND: HandleKind = HandleKind::Custom("sandbox");
}

// ───────────────────────────── conformance (15) ─────────────────────────────

/// Cases every world passes. Confining worlds add their isolation cases in their own crate.
pub const CONFORMANCE: &[&str] = &[
    "probe_succeeds",
    "bound_world_reports_its_enforcement_honestly",
    "writable_roots_start_with_the_workspace_root",
    "exit_status_passes_through_including_128_plus_signal",
    "a_waited_read_returns_when_the_process_ends_not_when_its_descendant_does",
    "killing_a_child_kills_its_whole_process_group",
    "a_timeout_kills_the_whole_process_group",
    "fs_fence_refuses_a_path_outside_the_workspace",
    "fs_fence_refuses_a_symlink_pointing_outside",
    "fs_fence_refuses_a_dangling_symlink",
    "write_is_atomic_and_versioned",
    "edit_refuses_a_non_unique_match_and_a_stale_version",
    "glob_and_grep_honor_gitignore",
];

/// Cases for worlds that provide `Snapshot` (11).
pub const SNAPSHOT_CONFORMANCE: &[&str] = &["snapshot_restore_round_trips_the_tree"];

/// Per-case deadline; a hang is a failure, not a stuck suite.
const CASE_DEADLINE: Duration = Duration::from_secs(30);
const STEP: Duration = Duration::from_secs(8);

type CaseResult = Result<(), String>;

/// Runs `CONFORMANCE` against a world. Scratch files live in a fresh directory under the fs root
/// and are removed afterwards.
pub async fn run_conformance(fs: &dyn Fs, proc: &dyn Subprocess, sb: &dyn Sandbox) -> Vec<(&'static str, Result<(), String>)> {
    let mut out = Vec::new();
    for &name in CONFORMANCE {
        out.push((name, run_case(name, fs, proc, sb).await));
    }
    out
}

/// Runs one `CONFORMANCE` case (so a world's test root can name each case as its own test).
pub async fn run_case(name: &str, fs: &dyn Fs, proc: &dyn Subprocess, sb: &dyn Sandbox) -> Result<(), String> {
    let dir = scratch_dir("conformance");
    let r = match tokio::time::timeout(CASE_DEADLINE, case_body(name, fs, proc, sb, &dir)).await {
        Ok(r) => r,
        Err(_) => Err(format!("did not finish within {}s", CASE_DEADLINE.as_secs())),
    };
    let _ = fs.remove(&dir).await;
    r
}

/// Runs `SNAPSHOT_CONFORMANCE`. Tree operations cover the whole workspace, so run this on a
/// scratch workspace.
pub async fn run_snapshot_conformance(fs: &dyn Fs, snap: &dyn Snapshot) -> Vec<(&'static str, Result<(), String>)> {
    let dir = scratch_dir("snapshot");
    let r = match tokio::time::timeout(CASE_DEADLINE, snapshot_round_trip(fs, snap, &dir)).await {
        Ok(r) => r,
        Err(_) => Err(format!("did not finish within {}s", CASE_DEADLINE.as_secs())),
    };
    let _ = fs.remove(&dir).await;
    vec![(SNAPSHOT_CONFORMANCE[0], r)]
}

fn scratch_dir(tag: &str) -> String {
    let t = Timestamp::now().0;
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!(".aho-{tag}-{t}-{n}")
}

async fn case_body(name: &str, fs: &dyn Fs, proc: &dyn Subprocess, sb: &dyn Sandbox, dir: &str) -> CaseResult {
    match name {
        "probe_succeeds" => sb.probe().await.map_err(|e| format!("probe failed: {e}")),
        "bound_world_reports_its_enforcement_honestly" => {
            let b = sb.bound();
            if b.name.is_empty() {
                return Err("world name is empty".into());
            }
            match (b.confines, b.enforced_by.as_str()) {
                (false, "policy only") => Ok(()),
                (false, other) => Err(format!("a world that does not confine must say `policy only`, not `{other}`")),
                (true, "policy only") | (true, "") => Err("a confining world must name what enforces it".into()),
                (true, _) => Ok(()),
            }
        }
        "writable_roots_start_with_the_workspace_root" => {
            let roots = sb.writable_roots();
            let root = fs.root();
            match roots.first() {
                Some(r) if r.trim_end_matches('/') == root.trim_end_matches('/') => Ok(()),
                other => Err(format!("first writable root is {other:?}, fs root is {root:?}")),
            }
        }
        "exit_status_passes_through_including_128_plus_signal" => {
            let (exit, _) = run(proc, SpawnSpec::new(&["/bin/sh", "-c", "exit 3"])).await?;
            expect_eq("plain exit", exit, Exit { code: 3, signal: None })?;
            let (exit, _) = run(proc, SpawnSpec::new(&["/bin/sh", "-c", "kill -TERM $$"])).await?;
            expect_eq("signalled exit", exit, Exit { code: 128 + 15, signal: Some(15) })
        }
        "a_waited_read_returns_when_the_process_ends_not_when_its_descendant_does" => {
            let child = spawn(proc, SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!"])).await?;
            let res = tokio::time::timeout(STEP, async {
                let exit = child.wait().await.map_err(|e| e.to_string())?;
                let text = drain(child.as_ref()).await;
                Ok::<_, String>((exit, text))
            })
            .await;
            let (exit, text) = match res {
                Ok(r) => r?,
                Err(_) => {
                    let _ = child.kill().await;
                    return Err("wait/read blocked on the backgrounded descendant".into());
                }
            };
            let pid = first_pid(&text)?;
            let _ = run(proc, SpawnSpec::new(&["kill", "-KILL", &pid.to_string()])).await;
            expect_eq("exit", exit, Exit { code: 0, signal: None })
        }
        "killing_a_child_kills_its_whole_process_group" => {
            let child = spawn(proc, SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!; sleep 30"])).await?;
            let pid = read_pid(child.as_ref()).await?;
            child.kill().await.map_err(|e| format!("kill failed: {e}"))?;
            let exit = tokio::time::timeout(STEP, child.wait()).await.map_err(|_| "the killed leader did not exit".to_string())?;
            let exit = exit.map_err(|e| e.to_string())?;
            if exit.signal.is_none() {
                return Err(format!("the leader exited without a signal: {exit:?}"));
            }
            gone(proc, pid).await
        }
        "a_timeout_kills_the_whole_process_group" => {
            let spec = SpawnSpec { timeout_ms: Some(300), ..SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!; sleep 30"]) };
            let child = spawn(proc, spec).await?;
            let pid = read_pid(child.as_ref()).await?;
            match tokio::time::timeout(STEP, child.wait()).await {
                Ok(Err(ProcError::Timeout)) => gone(proc, pid).await,
                Ok(other) => Err(format!("expected ProcError::Timeout, got {other:?}")),
                Err(_) => Err("the timeout never fired".into()),
            }
        }
        "fs_fence_refuses_a_path_outside_the_workspace" => {
            expect_fs("read ../", fs.read("../aho-outside", None).await, is_outside)?;
            expect_fs("write ../", fs.write("../aho-conformance-escape", "x", None).await, is_outside)?;
            expect_fs("write a/../../", fs.write(&format!("{dir}/../../aho-escape"), "x", None).await, is_outside)?;
            expect_fs("read /etc/hosts", fs.read("/etc/hosts", None).await, is_outside)
        }
        "fs_fence_refuses_a_symlink_pointing_outside" => {
            fs.write(&format!("{dir}/links/.keep"), "", None).await.map_err(|e| e.to_string())?;
            sh_ok(proc, &format!("ln -s / {dir}/links/out")).await?;
            expect_fs("read through link", fs.read(&format!("{dir}/links/out/etc/hosts"), None).await, is_outside)?;
            expect_fs("write through link", fs.write(&format!("{dir}/links/out/tmp/aho-escape"), "x", None).await, is_outside)?;
            expect_fs("stat the link", fs.stat(&format!("{dir}/links/out")).await, is_outside)
        }
        "fs_fence_refuses_a_dangling_symlink" => {
            fs.write(&format!("{dir}/dangle/.keep"), "", None).await.map_err(|e| e.to_string())?;
            sh_ok(proc, &format!("ln -s missing-target {dir}/dangle/link")).await?;
            let p = format!("{dir}/dangle/link");
            expect_fs("read", fs.read(&p, None).await, is_dangling)?;
            expect_fs("write", fs.write(&p, "x", None).await, is_dangling)?;
            match fs.stat(&format!("{dir}/dangle/missing-target")).await {
                Err(FsError::NotFound(_)) => Ok(()),
                other => Err(format!("writing through the dangling link created its target: {other:?}")),
            }
        }
        "write_is_atomic_and_versioned" => {
            let p = format!("{dir}/atomic/deep/a.txt");
            let d = fs.write(&p, "one\n", None).await.map_err(|e| e.to_string())?;
            if !d.created || d.new_version != FileVersion::of(b"one\n") {
                return Err(format!("create reported {d:?}"));
            }
            let r = fs.read(&p, None).await.map_err(|e| e.to_string())?;
            expect_eq("read after write", (r.text.as_str(), r.version), ("one\n", d.new_version))?;
            let stale = fs.write(&p, "two\n", Some(FileVersion::of(b"something else"))).await;
            expect_fs("stale write", stale, |e| matches!(e, FsError::StaleVersion { .. }))?;
            let d2 = fs.write(&p, "two\n", Some(d.new_version)).await.map_err(|e| e.to_string())?;
            if d2.created || d2.hunks.is_empty() {
                return Err(format!("update reported {d2:?}"));
            }
            let names: Vec<String> =
                fs.list(&format!("{dir}/atomic/deep")).await.map_err(|e| e.to_string())?.into_iter().map(|s| s.path).collect();
            expect_eq("directory after writes (no temp files left)", names, vec![p.clone()])
        }
        "edit_refuses_a_non_unique_match_and_a_stale_version" => {
            let p = format!("{dir}/edit.txt");
            let v = fs.write(&p, "x and x\n", None).await.map_err(|e| e.to_string())?.new_version;
            expect_fs("ambiguous edit", fs.edit(&p, "x", "y", false, v).await, |e| matches!(e, FsError::NotUnique { count: 2, .. }))?;
            expect_fs("stale edit", fs.edit(&p, "and", "or", false, FileVersion::of(b"old")).await, |e| {
                matches!(e, FsError::StaleVersion { .. })
            })?;
            expect_fs("missing text", fs.edit(&p, "zzz", "y", false, v).await, |e| matches!(e, FsError::NoMatch(_)))?;
            fs.edit(&p, "x", "y", true, v).await.map_err(|e| e.to_string())?;
            let r = fs.read(&p, None).await.map_err(|e| e.to_string())?;
            expect_eq("replace_all result", r.text, "y and y\n".to_string())
        }
        "glob_and_grep_honor_gitignore" => {
            let base = format!("{dir}/ign");
            for (f, t) in
                [(".gitignore", "ignored.txt\n"), ("ignored.txt", "needle\n"), ("kept.txt", "needle\n"), ("sub/deep.txt", "hay\nneedle\n")]
            {
                fs.write(&format!("{base}/{f}"), t, None).await.map_err(|e| e.to_string())?;
            }
            let hits = fs.grep("needle", Some(&base), None).await.map_err(|e| e.to_string())?;
            let got: Vec<(String, u32)> = hits.into_iter().map(|h| (h.path, h.line)).collect();
            expect_eq("grep", got, vec![(format!("{base}/kept.txt"), 1), (format!("{base}/sub/deep.txt"), 2)])?;
            let files = fs.glob("**/*.txt", Some(&base)).await.map_err(|e| e.to_string())?;
            expect_eq("glob", files, vec![format!("{base}/kept.txt"), format!("{base}/sub/deep.txt")])
        }
        other => Err(format!("unknown conformance case `{other}`")),
    }
}

async fn snapshot_round_trip(fs: &dyn Fs, snap: &dyn Snapshot, dir: &str) -> CaseResult {
    let p = |f: &str| format!("{dir}/{f}");
    let io = |e: FsError| e.to_string();
    fs.write(&p(".gitignore"), "*.log\n", None).await.map_err(io)?;
    let va = fs.write(&p("a.txt"), "v1\n", None).await.map_err(io)?.new_version;
    fs.write(&p("b.txt"), "keep\n", None).await.map_err(io)?;
    let t1 = snap.tree().await.map_err(io)?;
    if snap.tree().await.map_err(io)? != t1 {
        return Err("an unchanged workspace produced a different tree".into());
    }

    fs.write(&p("a.txt"), "v2\n", Some(va)).await.map_err(io)?;
    fs.write(&p("c.txt"), "new\n", None).await.map_err(io)?;
    fs.remove(&p("b.txt")).await.map_err(io)?;
    fs.write(&p("build.log"), "ignored\n", None).await.map_err(io)?;
    let t2 = snap.tree().await.map_err(io)?;
    if t2 == t1 {
        return Err("a changed workspace produced the same tree".into());
    }
    let d = snap.diff(t1, t2).await.map_err(io)?;
    expect_eq("diff(t1, t2)", (d.changed, d.deleted), (vec![p("a.txt"), p("c.txt")], vec![p("b.txt")]))?;

    let back = snap.restore(t1).await.map_err(io)?;
    expect_eq("restore(t1)", (back.changed, back.deleted), (vec![p("a.txt"), p("b.txt")], vec![p("c.txt")]))?;
    expect_eq("a.txt after restore", fs.read(&p("a.txt"), None).await.map_err(io)?.text, "v1\n".to_string())?;
    expect_eq("b.txt after restore", fs.read(&p("b.txt"), None).await.map_err(io)?.text, "keep\n".to_string())?;
    expect_fs("c.txt after restore", fs.stat(&p("c.txt")).await, |e| matches!(e, FsError::NotFound(_)))?;
    expect_eq("ignored file untouched", fs.read(&p("build.log"), None).await.map_err(io)?.text, "ignored\n".to_string())?;
    if snap.tree().await.map_err(io)? != t1 {
        return Err("the restored workspace does not hash to the target tree".into());
    }

    let fwd = snap.restore(t2).await.map_err(io)?;
    expect_eq("restore(t2)", (fwd.changed, fwd.deleted), (vec![p("a.txt"), p("c.txt")], vec![p("b.txt")]))?;
    expect_eq("a.txt after redo", fs.read(&p("a.txt"), None).await.map_err(io)?.text, "v2\n".to_string())?;
    let bad = snap.restore(Hash::of(b"no such tree")).await;
    expect_fs("restore of an unknown tree", bad.map(|_| ()), |_| true)
}

// ───────────────────────────── helpers ─────────────────────────────

fn is_outside(e: &FsError) -> bool {
    matches!(e, FsError::OutsideWorkspace(_))
}
fn is_dangling(e: &FsError) -> bool {
    matches!(e, FsError::DanglingSymlink(_))
}

fn expect_eq<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> CaseResult {
    if got == want { Ok(()) } else { Err(format!("{what}: got {got:?}, want {want:?}")) }
}

fn expect_fs<T: std::fmt::Debug>(what: &str, r: Result<T, FsError>, ok: impl Fn(&FsError) -> bool) -> CaseResult {
    match r {
        Err(e) if ok(&e) => Ok(()),
        Err(e) => Err(format!("{what}: wrong error {e:?}")),
        Ok(v) => Err(format!("{what}: expected a refusal, got {v:?}")),
    }
}

async fn spawn(proc: &dyn Subprocess, spec: SpawnSpec) -> Result<Arc<dyn Child>, String> {
    proc.spawn(spec).await.map_err(|e| format!("spawn failed: {e}"))
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

async fn run(proc: &dyn Subprocess, spec: SpawnSpec) -> Result<(Exit, String), String> {
    let child = spawn(proc, spec).await?;
    let r = tokio::time::timeout(STEP, async {
        let exit = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((exit, drain(child.as_ref()).await))
    })
    .await;
    match r {
        Ok(r) => r,
        Err(_) => {
            let _ = child.kill().await;
            Err("command did not finish".into())
        }
    }
}

async fn sh_ok(proc: &dyn Subprocess, script: &str) -> CaseResult {
    let (exit, text) = run(proc, SpawnSpec::new(&["/bin/sh", "-c", script])).await?;
    if exit.code == 0 { Ok(()) } else { Err(format!("`{script}` exited {}: {text}", exit.code)) }
}

fn first_pid(text: &str) -> Result<u32, String> {
    text.lines().next().and_then(|l| l.trim().parse().ok()).ok_or_else(|| format!("no pid in output {text:?}"))
}

async fn read_pid(child: &dyn Child) -> Result<u32, String> {
    let deadline = tokio::time::Instant::now() + STEP;
    let (mut cursor, mut out) = (0u64, Vec::new());
    loop {
        let (bytes, next, done) = child.output(cursor);
        out.extend(bytes);
        cursor = next;
        let text = String::from_utf8_lossy(&out);
        if text.contains('\n') {
            return first_pid(&text);
        }
        if done || tokio::time::Instant::now() > deadline {
            let _ = child.kill().await;
            return Err(format!("the child never printed its grandchild's pid: {text:?}"));
        }
        let _ = tokio::time::timeout_at(deadline, child.readable(cursor)).await;
    }
}

/// The grandchild `pid` stops existing (reaping by init may lag the kill a little).
async fn gone(proc: &dyn Subprocess, pid: u32) -> CaseResult {
    let deadline = tokio::time::Instant::now() + STEP;
    loop {
        let (exit, _) = run(proc, SpawnSpec::new(&["/bin/sh", "-c", &format!("kill -0 {pid} 2>/dev/null")])).await?;
        if exit.code != 0 {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            let _ = run(proc, SpawnSpec::new(&["kill", "-KILL", &pid.to_string()])).await;
            return Err(format!("backgrounded grandchild {pid} survived the group kill"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
