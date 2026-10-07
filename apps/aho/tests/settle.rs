//! The built binary with background jobs: `aho run -p` waits for the jobs it started and runs
//! their wake turn before it exits, and gives up at `jobs.settle_timeout_s` (cancelled, reported,
//! non-zero exit); `aho lease grant --jobs` records an operator jobs lease.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

/// A project whose model script starts `command` as a background bash job, then answers.
fn project(command: &str, last: &str) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let script = serde_json::json!({"steps": [
        {"text": "Starting it.", "tool_calls": [{"id": "c1", "name": "bash", "args": {"command": command, "background": true}}]},
        {"expect_tool_result_contains": "background as job", "text": "It runs in the background."},
        {"expect_last_user_contains": "built-ok", "text": last}
    ]});
    std::fs::write(root.join("script.json"), script.to_string()).unwrap();
    Project { _dir: dir, root, home }
}

fn aho(p: &Project, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.args(args).current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().to_string();
        if k.starts_with("AHO_") || k.ends_with("_API_KEY") {
            cmd.env_remove(k);
        }
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn session_id(p: &Project) -> String {
    let log = std::fs::read_dir(p.root.join(".aho/sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    log.file_stem().unwrap().to_string_lossy().to_string()
}

#[test]
fn print_mode_waits_for_its_job_and_answers_with_the_wake_turn() {
    let p = project("sleep 1; echo built-ok", "The build printed built-ok.");
    let out = aho(&p, &["run", "--script", "script.json", "--set", "jobs.wake=never", "-p", "build it in the background"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let (bg, last) = (stdout.find("It runs in the background.").unwrap(), stdout.find("The build printed built-ok.").unwrap());
    assert!(bg < last, "the wake turn's answer comes last, even with `jobs.wake = never`: {stdout}");
    let verify = aho(&p, &["session", "verify", &session_id(&p)[..10]]);
    assert_eq!(verify.status.code(), Some(0), "{}", text(&verify.stdout));
    assert!(text(&verify.stdout).contains("replay: 3 model request(s)"), "{}", text(&verify.stdout));
}

#[test]
fn print_mode_cancels_and_reports_jobs_still_running_at_the_settle_timeout() {
    let p = project("sleep 30; echo built-ok", "never reached");
    let started = Instant::now();
    let out = aho(&p, &["run", "--script", "script.json", "--set", "jobs.settle_timeout_s=1", "-p", "build it in the background"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(20), "it gave up at the timeout, not after the job");
    assert_ne!(out.status.code(), Some(0), "an answer that leaves a job out is not a success: {stderr}");
    assert!(stdout.contains("It runs in the background.") && !stdout.contains("never reached"), "{stdout}");
    assert!(
        stderr.contains("`jobs.settle_timeout_s` (1s) passed: 1 running job(s) were cancelled (op 1 (bash))"),
        "the operator is told which job was cut off: {stderr}"
    );
}

#[test]
fn lease_grant_jobs_records_a_jobs_lease_beside_the_vm_form() {
    // The command only sleeps so the job cannot finish before its detach lands: an instant
    // `echo` races the op table and usually returns its real output instead of the job notice.
    let p = project("sleep 1; echo built-ok", "The build printed built-ok.");
    assert_eq!(aho(&p, &["run", "--script", "script.json", "-p", "go"]).status.code(), Some(0));
    let mixed = aho(&p, &["lease", "grant", "--jobs", "3", "--machines", "2"]);
    assert_eq!(mixed.status.code(), Some(1), "a jobs lease takes no VM terms: {}", text(&mixed.stderr));

    let granted = aho(&p, &["lease", "grant", "--jobs", "3", "--expires", "1h"]);
    assert_eq!(granted.status.code(), Some(0), "{}", text(&granted.stderr));
    assert!(text(&granted.stdout).contains("up to 3 background job(s) at once, for 1h"), "{}", text(&granted.stdout));
    let status = aho(&p, &["lease", "status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&text(&status.stdout)).unwrap();
    assert_eq!((v["jobs_lease"]["max"].clone(), v["jobs_lease"]["live"].clone()), (serde_json::json!(3), serde_json::json!(true)), "{v}");
    assert!(v["lease"].is_null(), "no VM lease was granted: {v}");

    let revoked = aho(&p, &["lease", "revoke", "--jobs"]);
    assert_eq!(revoked.status.code(), Some(0), "{}", text(&revoked.stderr));
    let status = aho(&p, &["lease", "status"]);
    assert!(text(&status.stdout).contains("holds no live jobs lease, so `jobs.max` (8) applies"), "{}", text(&status.stdout));
    let verify = aho(&p, &["session", "verify", &session_id(&p)[..10]]);
    assert_eq!(verify.status.code(), Some(0), "{}", text(&verify.stdout));
}
