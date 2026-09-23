//! A changed plan reaches the session log exactly once (03, 05): an operator verb (`aho set`)
//! records it as its own command, and a resume under a plan the log did not seal records it as
//! `resume`; a resume under the sealed plan records nothing.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("script.json"), r#"{"steps": [{"text": "noted."}]}"#).unwrap();
    let script = root.join("script.json");
    std::fs::write(
        root.join("aho.toml"),
        format!("[model]\nprovider = \"scripted\"\nname = \"script\"\n[provider.scripted]\nscript = \"{}\"\n", script.display()),
    )
    .unwrap();
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
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "aho {args:?}\nstdout: {}\nstderr: {}", text(&out.stdout), text(&out.stderr));
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn log_path(root: &Path) -> PathBuf {
    let logs: Vec<PathBuf> = std::fs::read_dir(root.join(".aho/sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    logs[0].clone()
}

fn entries(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn of_type<'a>(es: &'a [Value], t: &str) -> Vec<&'a Value> {
    es.iter().filter(|e| e["kind"]["type"] == t).collect()
}

/// The entries a command appended: from its `command_run` (by name) to the end.
fn command<'a>(es: &'a [Value], name: &str) -> Vec<&'a Value> {
    let runs: Vec<usize> =
        es.iter().enumerate().filter(|(_, e)| e["kind"]["type"] == "command_run" && e["kind"]["name"] == name).map(|(i, _)| i).collect();
    assert_eq!(runs.len(), 1, "one `{name}` command in the log");
    let tail: Vec<&Value> = es[runs[0]..].iter().collect();
    let done = tail.iter().position(|e| e["kind"]["type"] == "command_done").expect("the command is closed");
    tail[..=done].to_vec()
}

#[test]
fn a_changed_plan_is_recorded_once_by_the_command_or_on_resume() {
    let p = project();
    aho(&p, &["run", "-p", "first"]);
    let log = log_path(&p.root);
    let id = log.file_stem().unwrap().to_string_lossy().to_string();
    let sealed = of_type(&entries(&log), "plan_sealed").len();

    // Edited by hand: nothing recorded it, so the resume does.
    let toml = std::fs::read_to_string(p.root.join("aho.toml")).unwrap();
    std::fs::write(p.root.join("aho.toml"), format!("{toml}[policy]\npreset = \"read-only\"\n")).unwrap();
    aho(&p, &["run", "--session", &id, "-p", "second"]);
    let es = entries(&log);
    let resume = command(&es, "resume");
    let run_id = resume[0]["id"].clone();
    assert_eq!(resume[0]["by"]["kind"], "operator");
    let kinds: Vec<&str> = resume.iter().map(|e| e["kind"]["type"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"plan_sealed"), "{kinds:?}");
    let preset = resume.iter().find(|e| e["kind"]["type"] == "config_resolved" && e["kind"]["key"] == "policy.preset").expect("the preset");
    assert_eq!(preset["kind"]["sourced"]["value"], "read-only");
    let epochs: Vec<&&Value> = resume.iter().filter(|e| e["kind"]["type"] == "policy_epoch").collect();
    assert_eq!(epochs.len(), 1, "{resume:?}");
    assert_eq!((epochs[0]["kind"]["preset"].clone(), epochs[0]["kind"]["approval_ref"].clone()), (Value::from("read-only"), run_id));
    assert!(resume.iter().all(|e| e["by"]["kind"] == "operator"));
    assert_eq!(of_type(&es, "plan_sealed").len(), sealed + 1);

    // Same plan again: the resume appends nothing of its own.
    aho(&p, &["run", "--session", &id, "-p", "third"]);
    assert_eq!(of_type(&entries(&log), "command_run").len(), 1);

    // An operator verb records its own change in the session, and the next resume has nothing to add.
    let set = aho(&p, &["-y", "set", "policy.preset", "workspace"]);
    assert!(text(&set.stdout).contains("appended Policy/Epoch{"), "{}", text(&set.stdout));
    let es = entries(&log);
    let by_set = command(&es, "set");
    let epoch = by_set.iter().find(|e| e["kind"]["type"] == "policy_epoch").expect("the widening back");
    assert_eq!(
        (epoch["kind"]["preset"].clone(), epoch["kind"]["approval_ref"].clone()),
        (Value::from("workspace"), by_set[0]["id"].clone())
    );
    assert_eq!(by_set[0]["kind"]["args"], serde_json::json!(["policy.preset", "workspace"]));
    aho(&p, &["run", "--session", &id, "-p", "fourth"]);
    let es = entries(&log);
    assert_eq!(of_type(&es, "command_run").len(), 2, "no resume command after `aho set` recorded the plan");
    assert_eq!(aho(&p, &["session", "verify", &id]).status.code(), Some(0));
}
