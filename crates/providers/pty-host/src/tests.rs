use super::*;
use std::time::Duration;

use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::CapState;
use aho_term::{OpenSpec, Size, Snapshot, Terminal};
use aho_types::ConsequenceSet;

fn host(root: &std::path::Path) -> HostTerminals {
    HostTerminals::new(root.to_string_lossy().into_owned())
}

fn spec(argv: &[&str], size: Size) -> OpenSpec {
    OpenSpec { argv: argv.iter().map(|s| s.to_string()).collect(), cwd: None, env: vec![("PS1".into(), "$ ".into())], size }
}

/// Waits until `pred` holds for a snapshot (transcript read from 0), or fails after 10 s.
async fn until(t: &dyn Terminal, what: &str, pred: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = t.snapshot(0);
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}\ntranscript:\n{}",
            s.screen.text,
            s.delta.text
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), t.changed(s.seq)).await;
    }
}

fn number_after(text: &str, tag: &str) -> Option<u32> {
    let rest = &text[text.rfind(tag)? + tag.len()..];
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn gone(pid: u32) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_shell_runs_input_and_the_screen_and_transcript_show_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let t = host(dir.path()).open(spec(&["/bin/sh"], Size::new(80, 24).unwrap())).await.unwrap();
    t.write(b"echo $((40 + 2))\n".to_vec()).await.unwrap();
    let s = until(t.as_ref(), "42 on its own line", |s| s.screen.text.lines().any(|l| l.trim() == "42")).await;
    assert!(s.delta.text.lines().any(|l| l == "42"), "{}", s.delta.text);
    assert_eq!(s.screen.size, Size { cols: 80, rows: 24 });
    assert!(s.exit.is_none());
    t.close().await.unwrap();
}

#[tokio::test]
async fn resize_reaches_the_program_and_the_emulator() {
    let dir = tempfile::tempdir().unwrap();
    let t = host(dir.path()).open(spec(&["/bin/sh"], Size::new(80, 24).unwrap())).await.unwrap();
    t.resize(Size::new(100, 30).unwrap()).unwrap();
    t.write(b"stty size\n".to_vec()).await.unwrap();
    let s = until(t.as_ref(), "stty size = 30 100", |s| s.delta.text.lines().any(|l| l == "30 100")).await;
    assert_eq!(s.screen.size, Size { cols: 100, rows: 30 });
    t.close().await.unwrap();
}

#[tokio::test]
async fn close_kills_background_jobs_and_double_forked_processes() {
    let dir = tempfile::tempdir().unwrap();
    let t = host(dir.path()).open(spec(&["/bin/sh"], Size::DEFAULT)).await.unwrap();
    let leader = t.identity().pid;
    t.write(b"sleep 1000 & echo BG=$!\n(sleep 1001 & echo DF=$! )\n".to_vec()).await.unwrap();
    let s =
        until(t.as_ref(), "both pids", |s| number_after(&s.delta.text, "BG=").is_some() && number_after(&s.delta.text, "DF=").is_some())
            .await;
    let (bg, df) = (number_after(&s.delta.text, "BG=").unwrap(), number_after(&s.delta.text, "DF=").unwrap());
    assert!(alive(bg) && alive(df));
    let closed = t.close().await.unwrap();
    assert!(gone(bg).await, "background job {bg} survived close");
    assert!(gone(df).await, "double-forked {df} survived close");
    assert_eq!(closed.killed.first(), Some(&leader));
    assert!(closed.killed.contains(&bg) && closed.killed.contains(&df), "{closed:?}");
    assert!(closed.exit.is_some(), "{closed:?}");
    // idempotent, and the terminal refuses input afterwards
    let again = t.close().await.unwrap();
    assert!(again.killed.is_empty() && again.note.is_some());
    assert_eq!(t.write(b"x".to_vec()).await.unwrap_err().code(), "TERM_CLOSED");
}

#[tokio::test]
async fn dropping_a_terminal_without_close_still_kills_its_tree() {
    let dir = tempfile::tempdir().unwrap();
    let t = host(dir.path()).open(spec(&["/bin/sh"], Size::DEFAULT)).await.unwrap();
    t.write(b"sleep 1002 & echo BG=$!\n".to_vec()).await.unwrap();
    let s = until(t.as_ref(), "bg pid", |s| number_after(&s.delta.text, "BG=").is_some()).await;
    let bg = number_after(&s.delta.text, "BG=").unwrap();
    drop(t);
    assert!(gone(bg).await, "background job {bg} survived drop");
}

#[tokio::test]
async fn exit_status_and_signals_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path());
    let t = h.open(spec(&["/bin/sh", "-c", "echo bye; exit 3"], Size::DEFAULT)).await.unwrap();
    let s = until(t.as_ref(), "exit", |s| s.exit.is_some()).await;
    assert_eq!(s.exit, Some(aho_term::Exit { code: 3, signal: None }));
    assert!(s.delta.text.contains("bye"));
    let t = h.open(spec(&["/bin/sh", "-c", "kill -TERM $$"], Size::DEFAULT)).await.unwrap();
    let s = until(t.as_ref(), "signal exit", |s| s.exit.is_some()).await;
    assert_eq!(s.exit, Some(aho_term::Exit { code: 128 + 15, signal: Some(15) }));
}

#[tokio::test]
async fn a_missing_program_or_directory_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path());
    let e = h.open(spec(&["/no/such/program"], Size::DEFAULT)).await.err().unwrap();
    assert_eq!(e.code(), "TERM_NOT_FOUND", "{e}");
    let mut s = spec(&["/bin/sh"], Size::DEFAULT);
    s.cwd = Some("missing-dir".into());
    assert_eq!(h.open(s).await.err().unwrap().code(), "TERM_NOT_FOUND");
    assert_eq!(h.open(spec(&[], Size::DEFAULT)).await.err().unwrap().code(), "TOOL_BAD_ARGS");
}

#[tokio::test]
async fn cwd_is_the_workspace_root_and_relative_dirs_resolve_under_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let h = host(&dir.path().canonicalize().unwrap());
    let mut s = spec(&["/bin/sh", "-c", "pwd; sleep 5"], Size::DEFAULT);
    s.cwd = Some("sub".into());
    let t = h.open(s).await.unwrap();
    let want = dir.path().canonicalize().unwrap().join("sub").display().to_string();
    until(t.as_ref(), "pwd", |s| s.delta.text.lines().any(|l| l == want)).await;
    t.close().await.unwrap();
}

fn plan_for(world: Option<&str>) -> aho_cap::Plan {
    let ws = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<world_host::HostWorld>().provide::<PtyHost>();
    let mut schema = Schema::default();
    for k in comp.config_keys().into_iter().chain(aho_cap::core_keys()) {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![("workspace.root".into(), ws.path().to_string_lossy().into_owned())];
    if let Some(w) = world {
        inputs.sets.push(("seams.world".into(), w.into()));
    }
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let policy = PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    if world == Some("host") {
        let (reg, broken) = comp.start(&p, &l);
        assert!(broken.is_empty(), "{broken:?}");
        assert!(reg.handle::<TerminalsHandle>().is_some());
    }
    p
}

#[test]
fn active_under_world_host_and_locked_with_a_fix_otherwise() {
    let p = plan_for(Some("host"));
    assert!(matches!(p.row("pty-host").unwrap().state, CapState::Active), "{:?}", p.row("pty-host"));
    let p = plan_for(None);
    let row = p.row("pty-host").unwrap();
    assert!(matches!(row.state, CapState::Locked { .. }), "{row:?}");
    assert_eq!(row.missing[0].fix.as_deref(), Some("aho set seams.world host"));
}
