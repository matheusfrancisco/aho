//! `aho invariants check|show` and `aho session verify --formal` through the built binary (13):
//! without TLC the check refuses with the install fix and records nothing; with java and
//! tla2tools.jar a failing spec exits 2, its verdict lands in the session log by Operator and
//! `show` renders the counterexample state by state; a session with `Task/*` entries is a
//! behavior of aho's own Tasks.tla.
//!
//! TLC tests need `java` and a jar (`AHO_TLA_JAR`, else `target/tmp/tla2tools.jar`, else
//! `~/.aho/tools/tla2tools.jar`) and print why they skip otherwise.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

const SPEC: &str = r#"# Counter — System Spec

## 1 Purpose

A counter that never passes its limit.

## 6 Invariants

### inv.counter.bounded

- statement: the counter never exceeds Max
- kind: safety
- mode: bounded(6)

```tla id=inv.counter.bounded
---- MODULE Counter ----
EXTENDS Naturals
CONSTANT Max
VARIABLE x
vars == <<x>>
Init == x = 0
Inc == x GUARD Max /\ x' = x + 1
Reset == x = Max /\ x' = 0
Next == Inc \/ Reset
Bounded == x <= Max
====
```

```cfg id=inv.counter.bounded
CONSTANT Max = 3
INIT Init
NEXT Next
INVARIANT Bounded
```
"#;

/// A git project with the counter spec (`guard` is `<` for a sound Inc, `<=` for a broken one)
/// and one finished scripted session running `script`.
fn project(guard: &str, script: &str) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join("spec")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    assert!(Command::new("git").args(["init", "-q", "."]).current_dir(&root).status().unwrap().success());
    std::fs::write(root.join("spec/SPEC.md"), SPEC.replace("GUARD", guard)).unwrap();
    std::fs::write(root.join("script.json"), script).unwrap();
    let p = Project { _dir: dir, root, home };
    let run = aho(&p, &["run", "--script", "script.json", "-p", "go"]);
    assert_eq!(run.status.code(), Some(0), "{}\n{}", text(&run.stdout), text(&run.stderr));
    p
}

const TEXT_ONLY: &str = r#"{"steps": [{"text": "nothing to do"}]}"#;

fn aho(p: &Project, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.args(args).current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().to_string();
        if k.starts_with("AHO_") || k.ends_with("_API_KEY") {
            cmd.env_remove(k);
        }
    }
    cmd.stdin(Stdio::null()).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn log_text(root: &Path) -> String {
    let dir = root.join(".aho/sessions");
    let path =
        std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "jsonl"));
    std::fs::read_to_string(path.unwrap()).unwrap()
}

/// Puts a real tla2tools.jar where formal-tlc looks (`~/.aho/tools`), or says why it cannot.
fn install_tlc(p: &Project) -> bool {
    let jar = [
        std::env::var_os("AHO_TLA_JAR").map(PathBuf::from),
        Some(Path::new(env!("CARGO_TARGET_TMPDIR")).join("tla2tools.jar")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aho/tools/tla2tools.jar")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file());
    let Some(jar) = jar else {
        eprintln!("skipped: no tla2tools.jar (set AHO_TLA_JAR, or run `cargo test -p formal-tlc --test real_tlc -- --ignored` once)");
        return false;
    };
    if !Command::new("java").arg("-version").output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skipped: `java` does not run on this host, and TLC needs a Java runtime");
        return false;
    }
    let tools = p.home.join(".aho/tools");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::copy(jar, tools.join("tla2tools.jar")).unwrap();
    true
}

#[test]
fn check_without_tlc_refuses_with_the_install_fix_and_records_nothing() {
    let p = project("<", TEXT_ONLY);
    let before = log_text(&p.root);
    let out = aho(&p, &["invariants", "check", "inv.counter.bounded"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{}\n{err}", text(&out.stdout));
    assert!(err.contains("tla2tools.jar") && err.contains("aho doctor formal --install tla"), "{err}");
    assert!(err.contains("no verdict was recorded"), "{err}");
    assert_eq!(log_text(&p.root), before, "a refused check appends nothing");
    let verify = aho(&p, &["session", "verify", "--formal"]);
    assert_eq!(verify.status.code(), Some(0), "without TLC the formal half is skipped, not failed: {}", text(&verify.stdout));
    assert!(text(&verify.stdout).contains("· formal: skipped"), "{}", text(&verify.stdout));
}

#[test]
fn a_failing_spec_exits_2_records_the_verdict_and_show_renders_the_trace() {
    let p = project("<=", TEXT_ONLY);
    if !install_tlc(&p) {
        return;
    }
    let out = aho(&p, &["invariants", "check", "--all"]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("Fail ✕") && stdout.contains("violated: invariant Bounded of Counter"), "{stdout}");
    assert!(stdout.contains("appended 1 entries by Operator"), "{stdout}");
    let log = log_text(&p.root);
    let checked = log.lines().rev().find(|l| l.contains("\"type\":\"invariant_checked\"")).expect("an InvariantChecked entry");
    assert!(checked.contains("\"kind\":\"operator\"") && checked.contains("\"verdict\":\"fail\""), "{checked}");

    let show = aho(&p, &["invariants", "show", "inv.counter.bounded"]);
    let shown = text(&show.stdout);
    assert_eq!(show.status.code(), Some(0), "{shown}\n{}", text(&show.stderr));
    assert!(shown.contains("Violated: invariant Bounded of Counter"), "{shown}");
    // the state per step: x climbs past Max = 3
    for (step, x) in [(1, 0), (5, 4)] {
        assert!(shown.contains(&format!("State {step}: ")) && shown.contains(&format!("x = {x}")), "{shown}");
    }

    std::fs::write(p.root.join("spec/SPEC.md"), SPEC.replace("GUARD", "<")).unwrap();
    let fixed = aho(&p, &["invariants", "check", "inv.counter.bounded"]);
    assert_eq!(fixed.status.code(), Some(0), "{}\n{}", text(&fixed.stdout), text(&fixed.stderr));
    let show = text(&aho(&p, &["invariants", "show", "inv.counter.bounded", "--json"]).stdout);
    let v: serde_json::Value = serde_json::from_str(&show).unwrap();
    assert_eq!(v["checked"]["verdict"]["verdict"], "pass", "the latest check wins: {v}");
    assert!(v["counterexample"].is_null(), "{v}");
}

#[test]
fn a_session_with_task_entries_is_a_behavior_of_every_aho_spec() {
    let script = r#"{"steps": [
        {"tool_calls": [{"id": "t1", "name": "task_seed", "args": {"tasks": [{"id": "survey", "content": "survey", "kind": "explore"}]}}], "stop": "tool_use"},
        {"tool_calls": [{"id": "t2", "name": "task_claim", "args": {}}], "stop": "tool_use"},
        {"tool_calls": [{"id": "t3", "name": "task_complete", "args": {"id": "survey", "artifact": {"findings": "done", "confidence": "high", "what_i_did_not_check": ["load"]}}}], "stop": "tool_use"},
        {"text": "done"}
    ]}"#;
    let p = project("<", script);
    assert!(log_text(&p.root).contains("\"ns\":\"Task\""), "the scripted run produced Task entries");
    if !install_tlc(&p) {
        return;
    }
    let out = aho(&p, &["session", "verify", "--formal"]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{}", text(&out.stderr));
    for m in ["Log", "Turn", "Policy", "Tasks"] {
        assert!(stdout.contains(&format!("✓ formal {m}.tla: the log is a behavior")), "{m}: {stdout}");
    }
}
