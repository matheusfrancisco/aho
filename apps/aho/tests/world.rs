//! A world that cannot open: its own error reaches `aho run` (which refuses), `aho status`,
//! `aho plan`, `aho why` and `aho shell --`, instead of only "no Subprocess provider is bound".
//! The world is `ssh` with an `ssh` client on PATH that fails the way a broken one does.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// What the fake ssh client prints; only the world can be the source of this text.
const X: &str = "fake-ssh: the moon has no route (7f3a)";

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

/// A project on the ssh world under a scripted model whose one step needs no tools; `world`
/// and `extra` are appended to its aho.toml.
fn project(world: &str, extra: &str) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let (root, home, bin) = (dir.path().join("project"), dir.path().join("home"), dir.path().join("bin"));
    for d in [&root, &home, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, format!("#!/bin/sh\necho '{X}' >&2\nexit 255\n")).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = root.join("script.json");
    std::fs::write(&script, r#"{"steps": [{"text": "hello without tools"}]}"#).unwrap();
    std::fs::write(
        root.join("aho.toml"),
        format!(
            "[model]\nprovider = \"scripted\"\nname = \"s\"\n[provider.scripted]\nscript = \"{}\"\n[seams]\nworld = \"ssh\"\n{world}\n{extra}\n",
            script.display()
        ),
    )
    .unwrap();
    Project { _dir: dir, root, home, bin }
}

/// The ssh world fully configured: it is Active in the plan and fails only when it opens.
const CONFIGURED: &str = "[world.ssh]\nhost = \"moon.invalid\"\nworkspace = \"/w\"\ncontrol_dir = \"~/s\"\n";

fn aho(p: &Project, args: &[&str]) -> Output {
    let path = format!("{}:{}", p.bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aho"));
    cmd.args(args).current_dir(&p.root).env("HOME", &p.home).env("NO_COLOR", "1").env("PATH", path);
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

fn sessions(root: &Path) -> usize {
    std::fs::read_dir(root.join(".aho/sessions"))
        .map(|rd| rd.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "jsonl")).count())
        .unwrap_or(0)
}

#[test]
fn a_world_that_fails_to_open_is_named_with_its_own_error_and_aho_run_refuses() {
    let p = project(CONFIGURED, "");
    let run = aho(&p, &["run", "-p", "hi"]);
    let err = text(&run.stderr);
    assert_eq!(run.status.code(), Some(2), "stdout: {}\nstderr: {err}", text(&run.stdout));
    assert!(err.contains("the session was not started: the world `world-ssh` is Broken") && err.contains(X), "{err}");
    assert!(err.contains("tool-fs has no world: missing handle FsRead"), "{err}");
    assert!(err.contains("tool-bash has no world: missing handle Subprocess"), "{err}");
    assert!(err.contains("fix: `aho set world.ssh.host <host>`") && err.contains("`aho why world-ssh`"), "{err}");
    assert!(!text(&run.stdout).contains("hello without tools"), "no turn ran");
    assert_eq!(sessions(&p.root), 1, "the refused boot is recorded");

    let status = text(&aho(&p, &["status"]).stdout);
    for id in ["world-ssh", "tool-fs"] {
        assert!(status.lines().any(|l| l.contains(&format!("✕ {id}")) && l.contains(X)), "{id}:\n{status}");
    }
    for id in ["world-ssh", "tool-fs"] {
        let why = text(&aho(&p, &["why", id]).stdout);
        assert!(why.contains("Broken") && why.lines().any(|l| l.trim_start().starts_with("error") && l.contains(X)), "{why}");
    }
    let plan = text(&aho(&p, &["plan"]).stdout);
    assert!(plan.lines().any(|l| l.contains("world-ssh") && l.contains("Broken")) && plan.contains(X), "{plan}");

    let shell = aho(&p, &["shell", "--", "true"]);
    let err = text(&shell.stderr);
    assert_eq!(shell.status.code(), Some(1), "{err}");
    assert!(err.contains("no Subprocess provider is bound: world-ssh is Broken") && err.contains(X), "{err}");
}

#[test]
fn a_world_the_plan_locks_refuses_only_while_file_or_shell_tools_are_on() {
    // `world.ssh.host` unset: the world is Locked before anything opens, so no log is written.
    let p = project("", "");
    let run = aho(&p, &["run", "-p", "hi"]);
    let err = text(&run.stderr);
    assert_eq!(run.status.code(), Some(2), "{err}");
    assert!(err.contains("the world `world-ssh` is Locked: Setting(world.ssh.host Set)"), "{err}");
    assert!(err.contains("fix: `aho set world.ssh.host <value>`"), "{err}");
    assert_eq!(sessions(&p.root), 0, "a world Locked by the plan refuses before any log exists");

    let only_bash = aho(&p, &["--disable", "tool-fs", "run", "-p", "hi"]);
    let err = text(&only_bash.stderr);
    assert_eq!(only_bash.status.code(), Some(2), "{err}");
    assert!(err.contains("tool-bash has no world") && !err.contains("tool-fs"), "a disabled tool-fs is not a reason: {err}");

    let off = project("", "[caps.tool-fs]\nenabled = false\n[caps.tool-bash]\nenabled = false\n");
    let run = aho(&off, &["run", "-p", "hi"]);
    assert_eq!(run.status.code(), Some(0), "stdout: {}\nstderr: {}", text(&run.stdout), text(&run.stderr));
    assert!(text(&run.stdout).contains("hello without tools"), "{}", text(&run.stdout));
}
