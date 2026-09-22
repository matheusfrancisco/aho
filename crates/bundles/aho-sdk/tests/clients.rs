//! The generated TS and Python clients against a real rpc server: this crate's `aho-sdk rpc` (the
//! aho-sdk-min product; `aho rpc` takes the same argv) with the scripted model. Each client opens
//! a session, prompts, answers the approval card for `write`, and must see both tool calls, their
//! results, and the final text (`sdk/*/test*/e2e.*` hold the client side and its checks).
//! Skipped with a printed reason when `python3` (≥ 3.11) or `node` (≥ 22.6) is absent.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn version(program: &str) -> Option<(u32, u32)> {
    let out = Command::new(program).arg("--version").output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let v = text.split_whitespace().last()?.trim_start_matches('v');
    let mut parts = v.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

struct Run {
    _dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
}

fn run_dirs() -> Run {
    let dir = tempfile::tempdir().unwrap();
    let (project, home) = (dir.path().join("project"), dir.path().join("home"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Run { _dir: dir, project, home }
}

fn check_summary(sdk: &str, out: std::process::Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the {sdk} e2e failed ({}):\n{stderr}", out.status);
    let s: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{sdk} printed no summary ({e}):\n{stderr}"));
    let tools: Vec<&str> = s["tool_calls"].as_array().unwrap().iter().map(|c| c["tool"].as_str().unwrap()).collect();
    assert_eq!(tools, ["read", "write"], "{s}");
    let outcomes: Vec<&str> = s["tool_results"].as_array().unwrap().iter().map(|r| r["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes, ["ok", "ok"], "{s}");
    assert!(s["tool_results"][0]["text"].as_str().unwrap().contains("hello from disk"), "{s}");
    assert_eq!(s["approvals"].as_array().unwrap().len(), 1, "{s}");
    assert_eq!(s["final_text"], "Read a.txt and wrote b.txt.");
    assert_eq!(s["transition"], "completed");
    assert_eq!(s["exit"], 0);
}

fn sdk_dir() -> PathBuf {
    aho_sdk::codegen::default_dir()
}

#[test]
fn the_python_sdk_drives_a_real_rpc_server() {
    match version("python3") {
        Some(v) if v >= (3, 11) => {}
        found => {
            eprintln!("skipped: the Python SDK needs python3 >= 3.11 on PATH (found {found:?})");
            return;
        }
    }
    let r = run_dirs();
    let out = Command::new("python3")
        .arg(sdk_dir().join("python/tests/e2e.py"))
        .args(["--bin", env!("CARGO_BIN_EXE_aho-sdk"), "--project"])
        .arg(&r.project)
        .env("PYTHONPATH", sdk_dir().join("python"))
        .env("HOME", &r.home)
        .output()
        .unwrap();
    check_summary("python", out);
    assert_eq!(std::fs::read_to_string(r.project.join("b.txt")).unwrap(), "written by the python sdk\n");
}

#[test]
fn the_typescript_sdk_drives_a_real_rpc_server() {
    match version("node") {
        Some(v) if v >= (22, 6) => {}
        found => {
            eprintln!("skipped: the TypeScript SDK needs node >= 22.6 on PATH for --experimental-strip-types (found {found:?})");
            return;
        }
    }
    let r = run_dirs();
    let out = Command::new("node")
        .args(["--experimental-strip-types", "--no-warnings"])
        .arg(sdk_dir().join("ts/test/e2e.ts"))
        .args(["--bin", env!("CARGO_BIN_EXE_aho-sdk"), "--project"])
        .arg(&r.project)
        .env("HOME", &r.home)
        .output()
        .unwrap();
    check_summary("typescript", out);
    assert_eq!(std::fs::read_to_string(r.project.join("b.txt")).unwrap(), "written by the ts sdk\n");
}
