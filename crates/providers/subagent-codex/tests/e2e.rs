//! End to end through real tmux with a fake `codex` (tests/fake-codex.sh) that writes a rollout
//! from recorded records over time: launch → locate → tail → turn end → follow-up → close, and an
//! interrupt by Escape. Skips with a reason when tmux is absent.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aho_cap::BoundWorld;
use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_policy::{Preset, SandboxPolicy};
use aho_subagent::{Mode, SpawnRequest, SubagentProvider};
use aho_types::{ConsequenceSet, Epoch, StopReason, Value};
use foreign_cli::{ForeignProvider, Settings, Status, mirror};
use subagent_codex::Codex;

fn have_tmux() -> bool {
    if foreign_cli::which("tmux").is_some() {
        return true;
    }
    println!("skipping: tmux is not on PATH, so there is no PTY host to run the fake CLI in");
    false
}

struct Fx {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    home: PathBuf,
    socket: String,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux").args(["-S", &self.socket, "kill-server"]).output();
    }
}

fn fx(name: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ws = std::fs::canonicalize(ws).unwrap();
    let home = dir.path().join("codex-home");
    // A socket path inside the test directory: nothing is left behind in tmux's own socket dir.
    let socket = dir.path().join(format!("tmux-{name}.sock")).display().to_string();
    Fx { _dir: dir, ws, home, socket }
}

fn provider(f: &Fx, log: Arc<dyn LogStore>) -> ForeignProvider<Codex> {
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-codex.sh");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cli = Codex { binary: fake.display().to_string(), home: f.home.clone(), export_home: true };
    let s = Settings { socket: f.socket.clone(), poll_ms: 50, blocked_after_ms: 10_000, interrupt_grace_ms: 3_000, ..Settings::default() };
    ForeignProvider::new(cli, s, log).unwrap()
}

fn policy(f: &Fx) -> SandboxPolicy {
    SandboxPolicy::from_preset(Preset::Dev, BoundWorld::host(), f.ws.display().to_string(), f.ws.display().to_string(), Epoch(0))
}

fn req(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "cx".into(),
        prompt: prompt.into(),
        mode: Mode::Continuable,
        persona: None,
        tool_filter: vec![],
        model: None,
        decl_cut: ConsequenceSet::ALL,
        is_async: false,
        bubble: false,
        fork_at: None,
        depth: 0,
    }
}

fn entries(log: &dyn LogStore) -> Vec<(String, Value)> {
    let Some(h) = log.head() else { return vec![] };
    log.read_branch(h)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::Custom { ns, name, body } if ns == mirror::NS => Some((name, body)),
            _ => None,
        })
        .collect()
}

/// `task_complete.last_agent_message` of a recorded turn fixture.
fn last_agent_message(fixture: &str) -> String {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture)).unwrap();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["payload"]["type"] == "task_complete")
        .and_then(|v| v["payload"]["last_agent_message"].as_str().map(str::to_string))
        .unwrap()
}

const WAIT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread")]
async fn launch_locate_tail_turn_end_follow_up_and_close() {
    if !have_tmux() {
        return;
    }
    let f = fx("life");
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(&f, log.clone());

    let id = p.spawn(req("inspect the build"), policy(&f)).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
    assert_eq!(r.last_message.as_deref(), Some(last_agent_message("turn1.jsonl").as_str()));
    let rollout = p.transcript(id).unwrap();
    assert!(rollout.starts_with(f.home.join("sessions")), "{}", rollout.display());
    assert!(rollout.file_name().unwrap().to_str().unwrap().starts_with("rollout-"));

    let seen: Vec<String> = entries(&*log).into_iter().map(|(n, _)| n).collect();
    let expect = ["Receipt", "Launched", "Attached", "Delivered", "Message", "ToolUse", "ToolResult", "Message", "TurnEnd", "Result"];
    assert_eq!(seen, expect);
    let e = entries(&*log);
    assert_eq!(e[5].1["id"], e[6].1["id"], "the output answers the call");
    assert_eq!(e[2].1["cli_session"], Value::Null, "Codex picks its own thread id");

    p.send(id, "and now the follow-up".into()).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
    assert_eq!(r.last_message.as_deref(), Some(last_agent_message("turn2.jsonl").as_str()));
    let delivered: Vec<u64> = entries(&*log).iter().filter(|(n, _)| n == "Delivered").filter_map(|(_, b)| b["seq"].as_u64()).collect();
    assert_eq!(delivered, vec![0, 1]);

    p.close(id, "the test is done").await.unwrap();
    assert_eq!(entries(&*log).last().unwrap().0, "Closed");
    assert!(p.children().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_interrupts_the_turn_and_the_run_ends_aborted() {
    if !have_tmux() {
        return;
    }
    let f = fx("interrupt");
    std::fs::write(f.ws.join(".fake-hang"), "").unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(&f, log.clone());

    let id = p.spawn(req("a long task"), policy(&f)).await.unwrap();
    tokio::time::timeout(WAIT, async {
        while !entries(&*log).iter().any(|(n, _)| n == "Delivered") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(p.child_info(id).unwrap().running);
    // The pane needs a moment to switch its terminal to single-key reads.
    tokio::time::sleep(Duration::from_millis(300)).await;
    p.interrupt(id).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::Aborted, "{:?}", r.diagnostic);
    assert_eq!(r.diagnostic, None, "Codex itself recorded the interrupt, within the grace period");
    let e = entries(&*log);
    let turn_end = e.iter().find(|(n, _)| n == "TurnEnd").unwrap();
    assert_eq!(turn_end.1["stop"], "aborted");
    assert!(e.iter().any(|(n, b)| n == "Note" && b["reason"].as_str().unwrap().contains("Escape")));
    assert_eq!(p.status(id), Some(Status::Live));
    p.close(id, "done").await.unwrap();
}
