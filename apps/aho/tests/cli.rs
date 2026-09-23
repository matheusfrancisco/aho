//! The built binary, end to end: argv parsing and exit codes, a scripted-model run, the `--json`
//! wrapper contract, plan → apply writes, and `session verify` catching a tampered log.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    std::fs::write(root.join("a.txt"), "hello from disk\n").unwrap();
    std::fs::write(
        root.join("script.json"),
        r#"{"steps": [
            {"text": "Reading it.", "tool_calls": [{"id": "c1", "name": "read", "args": {"path": "a.txt"}}]},
            {"expect_tool_result_contains": "hello from disk", "text": "a.txt says: hello from disk"}
        ]}"#,
    )
    .unwrap();
    Project { _dir: dir, root, home }
}

fn aho(p: &Project, args: &[&str], stdin: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.args(args).current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().to_string();
        if k.starts_with("AHO_") || k.ends_with("_API_KEY") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn sessions(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root.join(".aho/sessions"))
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default()
}

#[test]
fn argv_errors_exit_1_and_help_exits_0() {
    let p = project();
    assert_eq!(aho(&p, &["--help"], "").status.code(), Some(0));
    assert_eq!(aho(&p, &["frobnicate"], "").status.code(), Some(1));
    let both = aho(&p, &["rewind", "c1", "--chat", "--code"], "");
    assert_eq!(both.status.code(), Some(1), "--chat and --code conflict");
    let bad_set = aho(&p, &["--set", "no-equals", "tree"], "");
    assert_eq!(bad_set.status.code(), Some(1));
    assert!(text(&bad_set.stderr).contains("KEY=VALUE"));
}

#[test]
fn scripted_run_reads_a_file_and_the_log_verifies() {
    let p = project();
    let out = aho(&p, &["run", "--script", "script.json", "-p", "read a.txt"], "");
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {}", text(&out.stderr));
    assert!(stdout.contains("● read a.txt"), "{stdout}");
    assert!(stdout.contains("a.txt says: hello from disk"), "{stdout}");
    let logs = sessions(&p.root);
    assert_eq!(logs.len(), 1);
    let id = logs[0].file_stem().unwrap().to_string_lossy().to_string();
    let verify = aho(&p, &["session", "verify", &id[..10]], "");
    assert_eq!(verify.status.code(), Some(0), "{}", text(&verify.stdout));
    assert!(text(&verify.stdout).contains("replay: 2 model request(s)"), "{}", text(&verify.stdout));
}

#[test]
fn json_mode_keeps_the_wrapper_contract() {
    let p = project();
    let out = aho(&p, &["run", "--json", "--script", "script.json", "-p", "read a.txt"], "");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "stderr stays empty: {}", text(&out.stderr));
    let types: Vec<String> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .map(|v| v["kind"]["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(types.first().map(String::as_str), Some("config_resolved"), "the bootstrap comes first");
    for t in ["plan_sealed", "tool_call", "tool_result", "assistant_message", "turn_end"] {
        assert!(types.iter().any(|x| x == t), "{t} missing from {types:?}");
    }
}

#[test]
fn set_plans_asks_and_writes_the_most_specific_file() {
    let p = project();
    let declined = aho(&p, &["set", "policy.preset", "read-only"], "n\n");
    assert_eq!(declined.status.code(), Some(0));
    assert!(text(&declined.stdout).contains("~ preset  workspace -> read-only"), "{}", text(&declined.stdout));
    assert!(text(&declined.stdout).contains("nothing written."));
    assert!(!p.home.join(".aho/config.toml").exists());

    std::fs::write(p.root.join("aho.toml"), "# project settings\n").unwrap();
    let applied = aho(&p, &["set", "policy.preset", "read-only", "--yes"], "");
    assert_eq!(applied.status.code(), Some(0));
    let written = std::fs::read_to_string(p.root.join("aho.toml")).unwrap();
    assert!(written.contains("# project settings") && written.contains("preset = \"read-only\""), "comments are kept: {written}");
    let tree = aho(&p, &["tree"], "");
    assert!(text(&tree.stdout).contains("(preset: read-only, world: host"), "{}", text(&tree.stdout));

    let typo = aho(&p, &["set", "polcy.preset", "dev", "--yes"], "");
    assert_eq!(typo.status.code(), Some(1));
    assert!(text(&typo.stderr).contains("not a setting"));
}

#[test]
fn plan_failures_exit_2() {
    let p = project();
    let strict = aho(&p, &["plan", "--strict", "--set", "nope.key=1"], "");
    assert_eq!(strict.status.code(), Some(2), "{}", text(&strict.stderr));
    assert_eq!(aho(&p, &["lock"], "").status.code(), Some(0));
    let drift = aho(&p, &["run", "--lock", "--set", "tools.grace_ms=5", "--script", "script.json", "-p", "x"], "");
    assert_eq!(drift.status.code(), Some(2), "{}", text(&drift.stderr));
    assert!(sessions(&p.root).is_empty(), "a refused run opens no log");
}

#[test]
fn verify_catches_a_tampered_log() {
    let p = project();
    assert_eq!(aho(&p, &["run", "--script", "script.json", "-p", "read a.txt"], "").status.code(), Some(0));
    let log = sessions(&p.root).remove(0);
    let id = log.file_stem().unwrap().to_string_lossy().to_string();
    let original = std::fs::read_to_string(&log).unwrap();
    std::fs::write(&log, original.replace("hello from disk", "hello from DISK")).unwrap();
    let out = aho(&p, &["session", "verify", &id], "");
    assert_eq!(out.status.code(), Some(2), "{}\n{}", text(&out.stdout), text(&out.stderr));
}

/// 07: `aho shell -- <cmd>` runs one operator command in the session's world and logs it. It
/// used to lock the session policy twice in one expression and hang forever.
#[test]
fn an_operator_command_runs_in_the_session_world_and_is_logged() {
    let p = project();
    let script = p.root.join("script.json");
    std::fs::write(
        p.root.join("aho.toml"),
        format!("[model]\nprovider = \"scripted\"\nname = \"s\"\n[provider.scripted]\nscript = \"{}\"\n", script.display()),
    )
    .unwrap();
    assert_eq!(aho(&p, &["run", "-p", "read a.txt"], "").status.code(), Some(0));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.args(["shell", "--", "echo", "operator-ran"]).current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1");
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("`aho shell -- echo` did not finish within 60 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("operator-ran") && stdout.contains("logged to session"), "{stdout}");
    let log = std::fs::read_to_string(sessions(&p.root).remove(0)).unwrap();
    assert!(log.contains(r#""tool":"shell""#) && log.contains("echo operator-ran"), "the command is a logged Tool/Call");
}

/// 12: while `aho serve` runs, it owns the project's sessions — `aho run` is its client, and a
/// second process may not open a session the daemon holds. `--stop` winds it down.
#[test]
fn a_running_daemon_serves_aho_run_and_keeps_its_sessions_to_itself() {
    let p = project();
    let script = p.root.join("script.json");
    std::fs::write(
        p.root.join("aho.toml"),
        format!("[model]\nprovider = \"scripted\"\nname = \"s\"\n[provider.scripted]\nscript = \"{}\"\n", script.display()),
    )
    .unwrap();
    let up = aho(&p, &["serve", "--detach", "--idle-exit", "60"], "");
    assert_eq!(up.status.code(), Some(0), "{}\n{}", text(&up.stdout), text(&up.stderr));
    let stop = || aho(&p, &["serve", "--stop"], "");

    let run = aho(&p, &["run", "-p", "read a.txt"], "");
    let (out, err) = (text(&run.stdout), text(&run.stderr));
    let held = sessions(&p.root);
    let refused = held.first().map(|l| {
        let id = l.file_stem().unwrap().to_string_lossy().to_string();
        aho(&p, &["shell", "--session", &id, "--", "true"], "")
    });
    let stopped = stop();
    assert_eq!(run.status.code(), Some(0), "stdout: {out}\nstderr: {err}");
    assert!(err.contains("through the daemon"), "{err}");
    assert!(out.contains("a.txt says: hello from disk"), "{out}");
    assert_eq!(held.len(), 1, "the daemon wrote exactly one log");
    let refused = refused.unwrap();
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stdout));
    assert!(text(&refused.stderr).contains("open in the daemon"), "{}", text(&refused.stderr));
    assert_eq!(stopped.status.code(), Some(0), "{}", text(&stopped.stderr));
    let gone = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        !p.root.join(".aho/aho.sock").exists()
    });
    assert!(gone, "the daemon removes its socket when it stops");
}

/// `aho serve` says "ctrl-c stops": SIGINT must shut the daemon down cleanly (its shutdown
/// request is served by the same daemon, which must keep running while it answers).
#[test]
fn ctrl_c_stops_a_foreground_daemon() {
    let p = project();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.arg("serve").current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1");
    let mut daemon = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let socket = p.root.join(".aho/aho.sock");
    let up = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        socket.exists()
    });
    assert!(up, "the daemon never listened");
    let sent = Command::new("kill").args(["-INT", &daemon.id().to_string()]).status().unwrap();
    assert!(sent.success());
    let exit = (0..100).find_map(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        daemon.try_wait().unwrap()
    });
    let Some(exit) = exit else {
        daemon.kill().unwrap();
        panic!("the daemon ignored ctrl-c for 10s");
    };
    let err = text(&daemon.wait_with_output().unwrap().stderr);
    assert_eq!(exit.code(), Some(0), "{err}");
    assert!(err.contains("stopped (shut down on request)"), "{err}");
    assert!(!socket.exists(), "the daemon removes its socket when it stops");
}

#[test]
fn discovered_skills_are_opt_in_and_disable_overrides_enable() {
    let p = project();
    let skill = p.root.join(".aho/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: review\ndescription: Review changes\n---\nRead the diff.\n").unwrap();
    std::fs::write(p.root.join("aho.toml"), "").unwrap();
    let state = |args: &[&str]| {
        let out = aho(&p, args, "");
        assert!(out.status.success(), "{}", text(&out.stderr));
        let nodes: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        nodes[0]["state"]["state"].clone()
    };
    assert_eq!(state(&["tree", "skill.review", "--json"]), serde_json::json!("unlocked"));
    let enabled = aho(&p, &["enable", "skill.review", "--yes"], "");
    assert!(enabled.status.success(), "{}", text(&enabled.stderr));
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(p.root.join("aho.toml")).unwrap()).unwrap();
    assert_eq!(config["skills"]["review"]["enabled"].as_bool(), Some(true));
    assert_eq!(state(&["tree", "skill.review", "--json"]), serde_json::json!("active"));
    let disabled = state(&["--disable", "skill.review", "tree", "skill.review", "--json"]);
    assert_eq!(disabled, serde_json::json!("disabled"));
}
