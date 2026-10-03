//! End to end against a real rust-analyzer on a tiny cargo project in a tempdir. Skips (with the
//! reason printed) when rust-analyzer or cargo is not on PATH; runs whenever they are.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_fs::FsRead;
use aho_proc::SubprocessHandle;
use aho_types::*;
use serde_json::json;
use tool_lsp::{LspConfig, LspDeps, ToolLsp};
use world_host::{HostFs, HostSubprocess};

const MAIN: &str = "fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let x = add(1, 2);
    let y = add(x, 3);
    println!(\"{}\", y);
}
";

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n")
        .unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), MAIN).unwrap();
    (dir, root)
}

fn tool(root: &Path, cfg: LspConfig) -> ToolLsp {
    let deps = LspDeps { proc: SubprocessHandle(Arc::new(HostSubprocess::new(root))), fs: FsRead(Arc::new(HostFs::new(root).unwrap())) };
    ToolLsp::build(deps, cfg).unwrap()
}

fn req(tool: &str, args: Value) -> ExecRequest {
    let view = ExecView {
        call: CallId(format!("call-{tool}")),
        tool: tool.into(),
        class: Consequence::SpawnsProcess,
        args,
        policy: PolicyView {
            epoch: Epoch(0),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: "/work".into(),
            home: "/home".into(),
        },
        by: Principal::system("test"),
    };
    ExecRequest { view, cancel: CancelToken::new(), progress: None }
}

fn done(o: ExecOutcome) -> (Value, String) {
    match o {
        ExecOutcome::Done { value, content, .. } => (value, content.iter().filter_map(|b| b.as_text()).collect()),
        other => panic!("expected a result, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_analyzer_answers_definition_references_hover_symbols_rename_and_diagnostics() {
    for bin in ["rust-analyzer", "cargo"] {
        if !on_path(bin) {
            println!("SKIPPED: `{bin}` is not on PATH, so the rust-analyzer integration test cannot run.");
            return;
        }
    }
    let (_dir, root) = project();
    let mut cfg = LspConfig::default();
    // Bounded waits for indexing: generous, but a hung server still fails the test.
    cfg.ready_timeout_ms = 180_000;
    cfg.diagnostics_wait_ms = 90_000;
    cfg.request_timeout_ms = 60_000;
    let t = tool(&root, cfg);

    // Definition of the call on line 6 (`    let x = add(1, 2);`, `add` at column 13).
    let (v, text) = done(t.run("lsp_definition", &req("lsp_definition", json!({"path": "src/main.rs", "line": 6, "column": 13}))).await);
    assert_eq!(v["total"], json!(1), "{text}");
    assert_eq!(v["locations"][0]["path"], json!("src/main.rs"), "{text}");
    assert_eq!(v["locations"][0]["line"], json!(1), "{text}");
    assert_eq!(v["locations"][0]["column"], json!(4), "{text}");
    assert!(text.contains("src/main.rs:1:4  fn add(a: i32, b: i32) -> i32 {"), "{text}");

    // The same by symbol name alone (workspace/symbol) and by path + symbol.
    let (v, text) = done(t.run("lsp_definition", &req("lsp_definition", json!({"symbol": "add"}))).await);
    assert_eq!(v["locations"][0]["line"], json!(1), "{text}");
    let (v, text) = done(t.run("lsp_definition", &req("lsp_definition", json!({"path": "src/main.rs", "line": 7, "symbol": "add"}))).await);
    assert_eq!(v["locations"][0]["line"], json!(1), "{text}");

    // References: the declaration and the two calls.
    let (v, text) = done(t.run("lsp_references", &req("lsp_references", json!({"path": "src/main.rs", "symbol": "add"}))).await);
    assert_eq!(v["total"], json!(3), "{text}");
    let lines: Vec<i64> = v["locations"].as_array().unwrap().iter().map(|l| l["line"].as_i64().unwrap()).collect();
    assert_eq!(lines.iter().copied().collect::<std::collections::BTreeSet<_>>(), [1, 6, 7].into_iter().collect(), "{text}");
    let (v, text) = done(
        t.run(
            "lsp_references",
            &req("lsp_references", json!({"path": "src/main.rs", "line": 6, "column": 13, "include_declaration": false})),
        )
        .await,
    );
    assert_eq!(v["total"], json!(2), "{text}");

    // Hover carries the signature.
    let (v, text) = done(t.run("lsp_hover", &req("lsp_hover", json!({"path": "src/main.rs", "line": 7, "column": 13}))).await);
    assert!(v["contents"].as_str().unwrap().contains("fn add(a: i32, b: i32) -> i32"), "{text}");

    // Outline and workspace search.
    let (v, text) = done(t.run("lsp_symbols", &req("lsp_symbols", json!({"path": "src/main.rs"}))).await);
    let names: Vec<&str> = v["symbols"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"add") && names.contains(&"main"), "{text}");
    assert!(text.contains("function add  src/main.rs:1:4"), "{text}");
    let (v, text) = done(t.run("lsp_symbols", &req("lsp_symbols", json!({"query": "add"}))).await);
    assert!(v["symbols"].as_array().unwrap().iter().any(|s| s["name"] == json!("add") && s["line"] == json!(1)), "{text}");

    // Rename preview: diffs for all three sites, and the file on disk is untouched.
    let (v, text) = done(
        t.run("lsp_rename_preview", &req("lsp_rename_preview", json!({"path": "src/main.rs", "symbol": "add", "new_name": "sum"}))).await,
    );
    assert_eq!(v["written"], json!(false));
    assert!(text.contains("-fn add(a: i32, b: i32) -> i32 {") && text.contains("+fn sum(a: i32, b: i32) -> i32 {"), "{text}");
    assert!(text.contains("+    let y = sum(x, 3);"), "{text}");
    assert_eq!(std::fs::read_to_string(root.join("src/main.rs")).unwrap(), MAIN);

    // A clean file has no errors; an introduced type error is reported on its line.
    let (v, text) = done(t.run("lsp_diagnostics", &req("lsp_diagnostics", json!({"path": "src/main.rs"}))).await);
    assert!(!v["diagnostics"].as_array().unwrap().iter().any(|d| d["severity"] == json!("error")), "{text}");
    let broken = MAIN.replace("    println!(\"{}\", y);", "    let s: i32 = \"not a number\";\n    println!(\"{} {}\", y, s);");
    std::fs::write(root.join("src/main.rs"), &broken).unwrap();
    let (v, text) = done(t.run("lsp_diagnostics", &req("lsp_diagnostics", json!({"path": "src/main.rs"}))).await);
    let errors: Vec<&Value> = v["diagnostics"].as_array().unwrap().iter().filter(|d| d["severity"] == json!("error")).collect();
    assert!(
        errors.iter().any(|d| d["line"] == json!(8) && d["code"] == json!("E0308") && d["message"].as_str().unwrap().contains("i32")),
        "expected a type error on line 8:\n{text}"
    );

    // Positions still track the file after the change (the server got the new text).
    let (v, text) = done(t.run("lsp_hover", &req("lsp_hover", json!({"path": "src/main.rs", "line": 8, "symbol": "s"}))).await);
    assert!(v["contents"].as_str().unwrap().contains("i32"), "{text}");

    // One server was running for (rust, root); shutdown stops it.
    let running = t.pool().running("rust").await;
    assert_eq!(running.len(), 1);
    let pid = running[0].client.child().identity().pid;
    drop(running);
    assert_eq!(t.shutdown().await, 1);
    assert!(t.pool().running("rust").await.is_empty());
    let alive =
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().unwrap().success();
    assert!(!alive, "the server process {pid} outlived shutdown");
}

#[tokio::test]
async fn a_configured_server_missing_from_path_fails_with_the_fix() {
    let (_dir, root) = project();
    let mut cfg = LspConfig::default();
    cfg.servers[0].command = vec!["aho-test-no-such-language-server".into()];
    let t = tool(&root, cfg);
    let out = t.run("lsp_hover", &req("lsp_hover", json!({"path": "src/main.rs", "line": 1, "column": 4}))).await;
    let ExecOutcome::Err { code, reason } = out else { panic!("expected an error, got {out:?}") };
    assert_eq!(code, "LSP_SERVER_EXITED");
    assert!(reason.contains("`aho-test-no-such-language-server`") && reason.contains("not on this world's PATH"), "{reason}");
    assert!(reason.contains("caps.tool-lsp.servers.rust.command"), "{reason}");
}

#[tokio::test]
async fn files_without_a_configured_language_and_bad_positions_are_refused_before_any_server_starts() {
    let (_dir, root) = project();
    std::fs::write(root.join("notes.txt"), "hello\n").unwrap();
    let t = tool(&root, LspConfig::default());
    let out = t.run("lsp_hover", &req("lsp_hover", json!({"path": "notes.txt", "line": 1}))).await;
    assert!(matches!(&out, ExecOutcome::Err { code, reason } if code == "LSP_NO_SERVER" && reason.contains("*.txt")), "{out:?}");
    let out = t.run("lsp_hover", &req("lsp_hover", json!({"path": "../outside.rs", "line": 1}))).await;
    assert!(matches!(&out, ExecOutcome::Err { code, .. } if code == "TOOL_BAD_ARGS"), "{out:?}");
    let out = t.run("lsp_definition", &req("lsp_definition", json!({"path": "src/main.rs", "line": 0}))).await;
    assert!(matches!(&out, ExecOutcome::Err { code, reason } if code == "TOOL_BAD_ARGS" && reason.contains("1-based")), "{out:?}");
    let out = t.run("lsp_rename_preview", &req("lsp_rename_preview", json!({"path": "src/main.rs", "line": 1}))).await;
    assert!(matches!(&out, ExecOutcome::Err { code, reason } if code == "TOOL_BAD_ARGS" && reason.contains("new_name")), "{out:?}");
    assert!(t.pool().running("rust").await.is_empty());
}

#[test]
fn the_manifest_is_locked_without_any_known_server_and_declares_only_spawn_classes() {
    let m = ToolLsp::MANIFEST;
    assert_eq!(m.handles, &[HandleKind::Subprocess, HandleKind::FsRead]);
    let ceiling = aho_cap::ceiling(m.handles);
    assert!(m.tools.iter().all(|t| ceiling.contains(t.class) && t.class == Consequence::SpawnsProcess));
    assert!(
        m.requires
            .iter()
            .any(|r| matches!(r, Requirement::AnyOf(rs) if rs.iter().any(|b| matches!(b, Requirement::Binary("rust-analyzer")))))
    );
    assert_eq!(aho_cap::binaries_needed(&[m]), tool_lsp::KNOWN_SERVERS.iter().copied().collect());
}

/// The write path against the real server: rust-analyzer's own analysis (pulled) puts the type
/// error in the write result; the `cargo check` it runs on `didSave` reports the borrow error its
/// analysis cannot, as a late notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_carries_rust_analyzer_and_cargo_check_errors() {
    for bin in ["rust-analyzer", "cargo"] {
        if !on_path(bin) {
            println!("SKIPPED: `{bin}` is not on PATH, so the rust-analyzer write test cannot run.");
            return;
        }
    }
    let (_dir, root) = project();
    let mut cfg = LspConfig::default();
    cfg.ready_timeout_ms = 180_000;
    cfg.write_wait_ms = 60_000;
    cfg.late_wait_ms = 180_000;
    let t = tool(&root, cfg);
    // Indexing finishes before the write, as it would in a session that already used the server.
    done(t.run("lsp_diagnostics", &req("lsp_diagnostics", json!({"path": "src/main.rs"}))).await);

    let host: Arc<dyn aho_fs::Fs> = Arc::new(HostFs::new(&root).unwrap());
    let fs = tool_fs::ToolFs::build(tool_fs::FsDeps { read: FsRead(host.clone()), write: aho_fs::FsWrite(host) }, ()).unwrap();
    let args = json!({
        "path": "src/main.rs",
        "content": MAIN.replace(
            "    println!(\"{}\", y);\n}\n",
            "    let s: i32 = \"not a number\";\n    println!(\"{} {}\", y, s);\n}\n\nfn moved() {\n    let v = vec![1];\n    let w = v;\n    println!(\"{:?} {:?}\", v, w);\n}\n",
        ),
    });
    fs.run("read", &json!({"path": "src/main.rs"})).await;
    let out = fs.run("write", &args).await;
    let view = req("write", args).view;
    let lsp = t.clone();
    let decision = tokio::spawn(async move { lsp.after_write(&view, &out) }).await.unwrap();
    let PostDecision::Accept { content: Some(content), .. } = decision else { panic!("no diagnostics were appended: {decision:?}") };
    let seen: String = content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n");
    println!("{seen}");
    let rows: Vec<&str> = seen.lines().filter(|l| l.starts_with("src/main.rs:")).collect();
    assert!(rows.iter().any(|r| r.starts_with("src/main.rs:8:") && r.contains("error") && r.contains("E0308")), "{seen}");
    let moved = |text: &str| text.lines().any(|r| r.starts_with("src/main.rs:15:") && r.contains("error") && r.contains("E0382"));
    if !moved(&seen) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let late = loop {
            if let Some(n) = t.late_notice() {
                break n;
            }
            assert!(std::time::Instant::now() < deadline, "cargo check's E0382 never arrived");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        println!("{late}");
        assert!(moved(&late), "{late}");
    }
    t.shutdown().await;
}
