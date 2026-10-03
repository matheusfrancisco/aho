//! Diagnostics appended to file writes, end to end: tool-fs performs the write, tool-lsp's
//! `post_execute` (`after_write`) runs where the session runs it (inside a spawned task on the
//! multi-thread runtime) against the `lsp-fixture` server (see `tests/fixture/lsp_fixture.rs`);
//! late notices are read through `late_notice`, the capability's `context` fn.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_fs::{Fs, FsRead, FsWrite};
use aho_proc::SubprocessHandle;
use aho_types::*;
use serde_json::json;
use tool_fs::{FsDeps, ToolFs};
use tool_lsp::{LspConfig, LspDeps, ToolLsp};
use world_host::{HostFs, HostSubprocess};

const FIXTURE: &str = env!("CARGO_BIN_EXE_lsp-fixture");

/// `*.fk` files are served by the fixture started with `flags`. The write budget is generous so a
/// loaded machine does not turn an on-time result into a late one; tests of lateness lower it.
fn config(flags: &[&str], sets: &[(&str, &str)]) -> LspConfig {
    let mut schema = Schema::default();
    for k in <LspConfig as Config>::keys() {
        schema.add(k);
    }
    let mut all = vec![("caps.tool-lsp.servers.fake.command", "fixture"), ("caps.tool-lsp.servers.fake.globs", "*.fk")];
    all.extend_from_slice(sets);
    let mut inputs = ResolveInputs::default();
    inputs.sets = all.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut cfg = LspConfig::from_loadout(&resolve(&inputs, &schema, false).unwrap()).unwrap();
    let fake = cfg.servers.iter_mut().find(|s| s.language == "fake").unwrap();
    fake.command = std::iter::once(FIXTURE).chain(flags.iter().copied()).map(String::from).collect();
    cfg.write_wait_ms = 5_000;
    cfg
}

struct Rig {
    _dir: tempfile::TempDir,
    fs: ToolFs,
    lsp: ToolLsp,
}

fn rig(cfg: LspConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = std::fs::canonicalize(dir.path()).unwrap();
    let host: Arc<dyn Fs> = Arc::new(HostFs::new(&root).unwrap());
    let fs = ToolFs::build(FsDeps { read: FsRead(host.clone()), write: FsWrite(host.clone()) }, ()).unwrap();
    let lsp =
        ToolLsp::build(LspDeps { proc: SubprocessHandle(Arc::new(HostSubprocess::new(Path::new(&root)))), fs: FsRead(host) }, cfg).unwrap();
    Rig { _dir: dir, fs, lsp }
}

fn view(tool: &str, args: Value) -> ExecView {
    ExecView {
        call: CallId(format!("call-{tool}")),
        tool: tool.into(),
        class: Consequence::WorkspaceWrite,
        args,
        policy: PolicyView {
            epoch: Epoch(0),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: "/work".into(),
            home: "/home".into(),
        },
        by: Principal::system("test"),
    }
}

fn text(content: &[ContentBlock]) -> String {
    content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n")
}

impl Rig {
    /// Runs the tool, then the post-execute; returns the tool's own text and the decision.
    async fn call(&self, tool: &str, args: Value) -> (String, PostDecision) {
        let out = self.fs.run(tool, &args).await;
        let ExecOutcome::Done { content, .. } = &out else { panic!("{tool} failed: {out:?}") };
        let own = text(content);
        let lsp = self.lsp.clone();
        let v = view(tool, args);
        let decision = tokio::spawn(async move { lsp.after_write(&v, &out) }).await.unwrap();
        (own, decision)
    }

    /// The result text the model sees after the post-execute.
    async fn seen(&self, tool: &str, args: Value) -> (String, String) {
        let (own, decision) = self.call(tool, args).await;
        match decision {
            PostDecision::Accept { content: Some(c), deferred_context } => {
                assert!(deferred_context.is_empty());
                (own, text(&c))
            }
            PostDecision::Accept { content: None, .. } => (own.clone(), own),
            other => panic!("a write is never blocked: {other:?}"),
        }
    }

    /// The first late notice within `within`, polling as the session's request loop would.
    async fn late(&self, within: Duration) -> Option<String> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(n) = self.lsp.late_notice() {
                return Some(n);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn unchanged() -> PostDecision {
    PostDecision::Accept { content: None, deferred_context: Vec::new() }
}

fn rows<'a>(text: &'a str, path: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.starts_with(&format!("{path}:"))).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_that_introduces_errors_reports_them_and_fixing_them_leaves_the_result_alone() {
    let r = rig(config(&[], &[]));
    let (own, seen) = r.seen("write", json!({"path": "src/a.fk", "content": "ok\nlet x = WARN;\n  y ERROR\nINFO\n"})).await;
    assert!(seen.starts_with(&own), "the tool's own text comes first:\n{seen}");
    assert_eq!(
        rows(&seen, "src/a.fk"),
        ["src/a.fk:3:5 error: found ERROR (fixture)", "src/a.fk:2:9 warning: found WARN (fixture)"],
        "errors first, infos dropped:\n{seen}"
    );

    // An edit that removes the error: only the remaining warning is reported, nothing stale.
    let (_, seen) = r.seen("edit", json!({"path": "src/a.fk", "old_string": "  y ERROR", "new_string": "  y fine"})).await;
    assert_eq!(rows(&seen, "src/a.fk"), ["src/a.fk:2:9 warning: found WARN (fixture)"], "{seen}");
    let (_, decision) =
        r.call("str_replace_editor", json!({"command": "str_replace", "path": "src/a.fk", "old_str": "WARN", "new_str": "1"})).await;
    assert_eq!(decision, unchanged());
    assert_eq!(r.late(Duration::from_millis(500)).await, None, "everything was in the results");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_file_an_uncovered_extension_and_the_switch_off_leave_results_unchanged() {
    let r = rig(config(&[], &[]));
    assert_eq!(r.call("write", json!({"path": "clean.fk", "content": "all good\n"})).await.1, unchanged());
    assert_eq!(r.lsp.pool().running("fake").await.len(), 1, "the clean write did ask the server");

    let r = rig(config(&[], &[]));
    assert_eq!(r.call("write", json!({"path": "notes.txt", "content": "ERROR\n"})).await.1, unchanged());
    assert!(r.lsp.pool().running("fake").await.is_empty(), "no server is started for a file none covers");

    let r = rig(config(&[], &[("caps.tool-lsp.diagnostics_on_write", "false")]));
    assert_eq!(r.call("write", json!({"path": "b.fk", "content": "ERROR\n"})).await.1, unchanged());
    assert!(r.lsp.pool().running("fake").await.is_empty(), "switched off: no server is started");
    assert_eq!(r.late(Duration::from_millis(300)).await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_server_leaves_the_result_unchanged() {
    let mut cfg = config(&[], &[]);
    cfg.servers.iter_mut().find(|s| s.language == "fake").unwrap().command = vec!["aho-test-no-such-language-server".into()];
    let r = rig(cfg);
    let started = Instant::now();
    assert_eq!(r.call("write", json!({"path": "a.fk", "content": "ERROR\n"})).await.1, unchanged());
    assert!(started.elapsed() < Duration::from_millis(3_000), "a dead server does not cost the whole wait");
    assert_eq!(r.late(Duration::from_millis(300)).await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_on_save_check_never_holds_the_write_and_its_findings_arrive_as_a_late_notice() {
    let r = rig(config(&["--pull", "--check=2000"], &[]));
    let started = Instant::now();
    let (_, seen) = r.seen("write", json!({"path": "m.fk", "content": "x BORROW\nWARN\n"})).await;
    assert!(started.elapsed() < Duration::from_millis(1_500), "the write waited for the check");
    assert_eq!(rows(&seen, "m.fk"), ["m.fk:2:1 warning: found WARN (fixture)"], "{seen}");
    let late = r.late(Duration::from_secs(10)).await.expect("the check's error arrives as a late notice");
    assert_eq!(rows(&late, "m.fk"), ["m.fk:1:3 error: found BORROW (fixture)", "m.fk:2:1 warning: found WARN (fixture)"], "{late}");
    assert_eq!(r.late(Duration::from_millis(300)).await, None, "delivered once");

    let r = rig(config(&["--check=300"], &[("caps.tool-lsp.check_on_save", "false")]));
    let (_, seen) = r.seen("write", json!({"path": "m.fk", "content": "BORROW\nERROR\n"})).await;
    assert_eq!(rows(&seen, "m.fk"), ["m.fk:2:1 error: found ERROR (fixture)"], "{seen}");
    assert_eq!(r.late(Duration::from_millis(1_000)).await, None, "no didSave, no check");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_missing_the_write_deadline_arrive_late_and_a_newer_write_drops_stale_ones() {
    let mut cfg = config(&["--slow=1500"], &[]);
    cfg.write_wait_ms = 300;
    let r = rig(cfg);
    let started = Instant::now();
    assert_eq!(r.call("write", json!({"path": "s.fk", "content": "ERROR\n"})).await.1, unchanged());
    assert!(started.elapsed() < Duration::from_millis(1_200), "the write waited past its budget");
    let late = r.late(Duration::from_secs(10)).await.expect("the cold server's findings arrive late");
    assert_eq!(rows(&late, "s.fk"), ["s.fk:1:1 error: found ERROR (fixture)"], "{late}");

    // The check for the first content is still running when the file is fixed: its finding is stale.
    let r = rig(config(&["--check=500"], &[]));
    r.seen("write", json!({"path": "t.fk", "content": "BORROW\n"})).await;
    let (_, decision) = r.call("write", json!({"path": "t.fk", "content": "fine\n"})).await;
    assert_eq!(decision, unchanged());
    assert_eq!(r.late(Duration::from_secs(3)).await, None, "nothing about content that no longer exists");
}
