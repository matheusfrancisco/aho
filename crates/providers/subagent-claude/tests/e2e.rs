//! End to end through real tmux with a fake `claude` (tests/fake-claude.sh) that appends recorded
//! transcript records over time: launch → tail → turn end → follow-up → close, a child blocked on
//! its own dialog, and a crash in the middle of a turn. Skips with a reason when tmux is absent.
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
use subagent_claude::{ClaudeCode, escape_cwd};

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
    config: PathBuf,
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
    let config = dir.path().join("claude-config");
    // A socket path inside the test directory: nothing is left behind in tmux's own socket dir.
    let socket = dir.path().join(format!("tmux-{name}.sock")).display().to_string();
    Fx { _dir: dir, ws, config, socket }
}

fn fake() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

fn provider(f: &Fx, log: Arc<dyn LogStore>) -> ForeignProvider<ClaudeCode> {
    let cli = ClaudeCode { binary: fake(), config_dir: f.config.clone(), export_config_dir: true };
    let s = Settings { socket: f.socket.clone(), poll_ms: 50, blocked_after_ms: 1_500, interrupt_grace_ms: 1_000, ..Settings::default() };
    ForeignProvider::new(cli, s, log).unwrap()
}

fn policy(f: &Fx) -> SandboxPolicy {
    SandboxPolicy::from_preset(Preset::Dev, BoundWorld::host(), f.ws.display().to_string(), f.ws.display().to_string(), Epoch(0))
}

fn req(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "cc".into(),
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

/// The final assistant text of a recorded turn fixture.
fn final_text(fixture: &str) -> String {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture)).unwrap();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "assistant")
        .filter_map(|v| v["message"]["content"][0]["text"].as_str().map(str::to_string))
        .next_back()
        .unwrap()
}

fn sessions(socket: &str) -> String {
    let out = std::process::Command::new("tmux").args(["-S", socket, "list-sessions", "-F", "#{session_name}"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const WAIT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread")]
async fn launch_tail_turn_end_follow_up_and_close() {
    if !have_tmux() {
        return;
    }
    let f = fx("life");
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(&f, log.clone());

    let id = p.spawn(req("inspect the build"), policy(&f)).await.unwrap();
    let (session, attach) = p.tmux_session(id).unwrap();
    assert!(attach.contains(&f.socket) && attach.contains(&session));
    assert!(sessions(&f.socket).lines().any(|l| l == session), "the CLI runs in its own tmux session");

    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
    assert_eq!(r.last_message.as_deref(), Some(final_text("turn1.jsonl").as_str()));
    assert_eq!(r.diagnostic, None);
    let transcript = p.transcript(id).unwrap();
    assert!(transcript.starts_with(f.config.join("projects").join(escape_cwd(&f.ws))), "{}", transcript.display());
    assert_eq!(p.status(id), Some(Status::Live));

    let seen: Vec<String> = entries(&*log).into_iter().map(|(n, _)| n).collect();
    let expect = ["Receipt", "Launched", "Attached", "Delivered", "ToolUse", "ToolResult", "Message", "TurnEnd", "Result"];
    assert_eq!(seen, expect, "mirrored into the parent in transcript order");
    let e = entries(&*log);
    assert_eq!(e[0].1["state"], "launching");
    assert_eq!(e[3].1["seq"], 0, "the task on the command line is confirmed by the transcript");
    assert_eq!(e[4].1["name"], "Bash");
    assert_eq!(e[4].1["id"], e[5].1["id"]);
    assert!(e[7].1["offset"].as_u64().unwrap() > e[4].1["offset"].as_u64().unwrap(), "offsets grow with the transcript");

    // A follow-up is typed into its pane and answered by a second recorded turn.
    p.send(id, "and now the follow-up".into()).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
    assert_eq!(r.last_message.as_deref(), Some(final_text("turn2.jsonl").as_str()));
    let e = entries(&*log);
    let delivered: Vec<u64> = e.iter().filter(|(n, _)| n == "Delivered").filter_map(|(_, b)| b["seq"].as_u64()).collect();
    assert_eq!(delivered, vec![0, 1]);
    let results: Vec<u64> = e.iter().filter(|(n, _)| n == "Result").filter_map(|(_, b)| b["run"].as_u64()).collect();
    assert_eq!(results, vec![1, 2]);

    p.close(id, "the test is done").await.unwrap();
    let e = entries(&*log);
    assert_eq!(e.last().unwrap().0, "Closed");
    assert!(!sessions(&f.socket).lines().any(|l| l == session), "its tmux session is gone");
    assert!(p.children().is_empty());
    assert!(p.send(id, "anyone?".into()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_waiting_on_its_own_dialog_comes_back_blocked_and_takes_the_answer() {
    if !have_tmux() {
        return;
    }
    let f = fx("dialog");
    std::fs::write(f.ws.join(".fake-dialog"), "").unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(&f, log.clone());

    let id = p.spawn(req("inspect the build"), policy(&f)).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::ToolUse, "blocked on its own dialog");
    let diag = r.diagnostic.unwrap();
    assert!(diag.contains("Do you trust the files in this folder?") && diag.contains("send_message"), "{diag}");
    assert_eq!(p.status(id), Some(Status::Launching), "no transcript yet");
    let blocked = entries(&*log).into_iter().find(|(n, _)| n == "Blocked").unwrap().1;
    assert!(blocked["screen"].as_str().unwrap().contains("1. Yes, proceed"));

    // The parent answers through the CLI's own dialog; the run continues to a normal end.
    p.send(id, "1".into()).await.unwrap();
    let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
    assert_eq!(p.status(id), Some(Status::Live));
    p.close(id, "done").await.unwrap();
}

#[test]
fn a_crash_mid_turn_resumes_from_the_saved_offset_without_duplicates() {
    if !have_tmux() {
        return;
    }
    let f = fx("crash");
    std::fs::write(f.ws.join(".fake-slow"), "").unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());

    // First process: launch, mirror the first records, then die without closing anything.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let id = rt.block_on(async {
        let p = provider(&f, log.clone());
        let id = p.spawn(req("inspect the build"), policy(&f)).await.unwrap();
        tokio::time::timeout(WAIT, async {
            while !entries(&*log).iter().any(|(n, _)| n == "ToolUse") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        id
    });
    rt.shutdown_background();

    // While aho is down the CLI finishes its turn.
    std::fs::write(f.ws.join(".fake-continue"), "").unwrap();
    std::thread::sleep(Duration::from_millis(600));

    // Second process: same parent log, fresh provider.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let p = provider(&f, log.clone());
        assert_eq!(p.status(id), Some(Status::Live));
        assert!(p.child_info(id).unwrap().running, "the run open at the crash is still open");
        let r = tokio::time::timeout(WAIT, p.wait(id)).await.unwrap().unwrap();
        assert_eq!(r.stop, StopReason::EndTurn, "{:?}", r.diagnostic);
        assert_eq!(r.last_message.as_deref(), Some(final_text("turn1.jsonl").as_str()));

        let e = entries(&*log);
        let count = |name: &str| e.iter().filter(|(n, _)| n == name).count();
        assert_eq!((count("ToolUse"), count("ToolResult"), count("Delivered"), count("TurnEnd")), (1, 1, 1, 1), "{e:?}");
        let recovered = e.iter().find(|(n, _)| n == "Recovered").unwrap();
        assert_eq!(recovered.1["state"], "resumed");
        p.close(id, "done").await.unwrap();
    });
}
