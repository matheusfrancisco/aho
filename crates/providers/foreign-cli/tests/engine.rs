//! The engine without a real CLI: refusals, launch failure, and crash recovery from the parent
//! log (crash-honest receipts). The CLI format here is a minimal test transcript; the real
//! formats are tested against recorded fixtures in `subagent-claude` / `subagent-codex`.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::BoundWorld;
use aho_log::{Entry, EntryKind, LogStore, MemoryLog};
use aho_policy::{Preset, SandboxPolicy};
use aho_subagent::{Mode, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::{ConsequenceSet, Epoch, SessionId, StopReason, Value};
use foreign_cli::{
    Cli, Event, ForeignProvider, Launch, LaunchCtx, LocateCtx, Mirror, Normalized, Record, Settings, Status, file_ident, mirror,
};
use serde_json::json;

/// Transcript lines: `{"k":"prompt"}`, `{"k":"text","t":…}`, `{"k":"end"}`.
struct TestCli;

impl Cli for TestCli {
    const PROVIDER: &'static str = "subagent-test";
    const PRODUCT: &'static str = "TestCLI";
    const SHORT: &'static str = "test";
    const PERSONA: bool = false;
    fn launch(&self, _: &LaunchCtx<'_>) -> Result<Launch, String> {
        Ok(Launch { argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()], env: vec![], cli_session: None })
    }
    fn locate(&self, ctx: &LocateCtx<'_>) -> Option<PathBuf> {
        Some(ctx.cwd.join("transcript.jsonl")).filter(|p| p.is_file())
    }
    fn normalize(&self, line: &[u8]) -> Record {
        let v: Value = serde_json::from_slice(line).unwrap();
        let k = v["k"].as_str().unwrap().to_string();
        let out = match k.as_str() {
            "prompt" => Normalized::Events(vec![Event::UserPrompt]),
            "text" => Normalized::Events(vec![Event::AssistantText { text: v["t"].as_str().unwrap().into() }]),
            "end" => Normalized::Events(vec![Event::TurnEnd { stop: StopReason::EndTurn, last_message: None }]),
            _ => Normalized::Unknown,
        };
        Record { kind: k, out }
    }
}

fn settings(socket: &str) -> Settings {
    Settings { socket: socket.into(), poll_ms: 20, blocked_after_ms: 60_000, interrupt_grace_ms: 500, ..Settings::default() }
}

fn policy(preset: Preset, world: BoundWorld, ws: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy::from_preset(preset, world, ws.display().to_string(), ws.display().to_string(), Epoch(0))
}

fn req(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "t".into(),
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

fn subagent_entries(log: &dyn LogStore) -> Vec<(String, Value)> {
    let Some(h) = log.head() else { return vec![] };
    log.read_branch(h)
        .unwrap()
        .into_iter()
        .filter_map(|e: Entry| match e.kind {
            EntryKind::Custom { ns, name, body } if ns == mirror::NS => Some((name, body)),
            _ => None,
        })
        .collect()
}

fn names(log: &dyn LogStore) -> Vec<String> {
    subagent_entries(log).into_iter().map(|(n, _)| n).collect()
}

fn provider_err(e: SubagentError) -> String {
    match e {
        SubagentError::Provider(m) => m,
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn refusals_happen_before_anything_is_recorded_or_started() {
    let ws = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = ForeignProvider::new(TestCli, settings("aho-test-refusals"), log.clone()).unwrap();
    let dev = policy(Preset::Dev, BoundWorld::host(), ws.path());

    let confined = BoundWorld { name: "vm".into(), confines: true, enforced_by: "QEMU".into() };
    let e = provider_err(p.spawn(req("x"), policy(Preset::Dev, confined, ws.path())).await.unwrap_err());
    assert!(e.contains("confines tools") && e.contains("nothing was started"), "{e}");

    let e = provider_err(p.spawn(req("x"), policy(Preset::Workspace, BoundWorld::host(), ws.path())).await.unwrap_err());
    assert!(e.contains("NetworkEgress") && e.contains("`workspace`"), "{e}");

    let mut fork = req("x");
    fork.fork_at = Some(aho_types::EntryId(1));
    assert!(matches!(p.spawn(fork, dev.clone()).await, Err(SubagentError::Unsupported(w)) if w.contains("forking")));
    let mut filtered = req("x");
    filtered.tool_filter = vec!["read".into()];
    assert!(matches!(p.spawn(filtered, dev.clone()).await, Err(SubagentError::Unsupported(_))));
    let mut persona = req("x");
    persona.persona = Some("be terse".into());
    assert!(matches!(p.spawn(persona, dev.clone()).await, Err(SubagentError::Unsupported("personas"))));
    let mut deep = req("x");
    deep.depth = 4;
    assert!(matches!(p.spawn(deep, dev.clone()).await, Err(SubagentError::DepthLimit(3))));
    let e = provider_err(p.spawn(req("  "), dev).await.unwrap_err());
    assert!(e.contains("empty"), "{e}");

    assert!(names(&*log).is_empty(), "a refused spawn leaves no receipt");
    assert!(p.children().is_empty());
    assert!(!p.features().fork && !p.features().tool_filter && p.features().max_depth == 3);
}

#[tokio::test]
async fn a_launch_that_fails_is_receipted_and_closed() {
    let ws = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = Settings { tmux: "/nonexistent/aho-test/tmux".into(), ..settings("aho-test-nolaunch") };
    let p = ForeignProvider::new(TestCli, s, log.clone()).unwrap();
    let e = provider_err(p.spawn(req("do it"), policy(Preset::Dev, BoundWorld::host(), ws.path())).await.unwrap_err());
    assert!(e.contains("could not be started in tmux") && e.contains("nothing is running"), "{e}");
    let entries = subagent_entries(&*log);
    assert_eq!(entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["Receipt", "Closed"]);
    assert_eq!(entries[0].1["state"], "launching");
    assert_eq!(entries[0].1["provider"], "subagent-test");
    assert!(p.children().is_empty());
}

fn seed_receipt(m: &Mirror, child: SessionId, cwd: &std::path::Path) {
    let body = json!({"label": "old", "depth": 1, "cwd": cwd.display().to_string(), "tmux_socket": "s", "tmux_session": format!("aho-test-{}", child.0), "cli_session": null, "state": "launching"});
    m.append(mirror::RECEIPT, child, body).unwrap();
    m.append(mirror::LAUNCHED, child, json!({"tmux_session": format!("aho-test-{}", child.0), "created": 1, "pane_pid": 2})).unwrap();
}

#[tokio::test]
async fn a_child_that_was_launching_at_a_crash_is_blocked_and_uncertain() {
    let ws = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let child = SessionId(77);
    seed_receipt(&Mirror::new(log.clone(), "subagent-test"), child, ws.path());
    // An entry from another provider is not ours to recover.
    Mirror::new(log.clone(), "subagent-other").append(mirror::RECEIPT, SessionId(78), json!({"label": "x"})).unwrap();

    let p = ForeignProvider::new(TestCli, settings("aho-test-uncertain"), log.clone()).unwrap();
    assert_eq!(p.status(child), Some(Status::Uncertain));
    assert_eq!(p.children().len(), 1);
    let info = p.child_info(child).unwrap();
    assert!(!info.running);
    assert_eq!(info.last_stop, Some(StopReason::Error));

    let r = tokio::time::timeout(Duration::from_secs(5), p.wait(child)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::Error);
    assert!(r.diagnostic.as_deref().unwrap().contains("whether it came up is unknown"), "{:?}", r.diagnostic);

    let entries = subagent_entries(&*log);
    let recovered = entries.iter().find(|(n, _)| n == "Recovered").unwrap();
    assert_eq!((recovered.1["state"].as_str(), recovered.1["certainty"].as_str()), (Some("blocked"), Some("uncertain")));
    let delivery = entries.iter().find(|(n, _)| n == "Delivery").unwrap();
    assert_eq!((delivery.1["seq"].as_u64(), delivery.1["delivery"].as_str()), (Some(0), Some("unknown")));

    let e = provider_err(p.send(child, "hello?".into()).await.unwrap_err());
    assert!(e.contains("unknown") && e.contains("nothing was sent"), "{e}");

    // A second boot over the same log does not report the same uncertainty twice as a new run.
    let again = ForeignProvider::new(TestCli, settings("aho-test-uncertain"), log.clone()).unwrap();
    assert_eq!(again.status(child), Some(Status::Uncertain));
    let deliveries = subagent_entries(&*log).iter().filter(|(n, _)| n == "Delivery").count();
    assert_eq!(deliveries, 1, "an unknown delivery is marked once");
}

#[tokio::test]
async fn an_attached_child_resumes_its_tail_at_the_recorded_offset() {
    let ws = tempfile::tempdir().unwrap();
    let transcript = ws.path().join("transcript.jsonl");
    let before = "{\"k\":\"prompt\"}\n{\"k\":\"text\",\"t\":\"before the crash\"}\n";
    std::fs::write(&transcript, before).unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let m = Mirror::new(log.clone(), "subagent-test");
    let child = SessionId(90);
    seed_receipt(&m, child, ws.path());
    let ident = file_ident(&transcript).map(|(d, i)| json!([d, i]));
    m.append(mirror::ATTACHED, child, json!({"transcript": transcript.display().to_string(), "ident": ident, "offset": 0})).unwrap();
    m.append(mirror::DELIVERED, child, json!({"seq": 0, "offset": 15})).unwrap();
    let off = before.len() as u64;
    m.append(mirror::MESSAGE, child, json!({"text": "before the crash", "offset": off})).unwrap();
    m.append(mirror::SENT, child, json!({"seq": 1, "starts_run": false, "chars": 3})).unwrap();
    // While aho was down, the CLI kept writing.
    std::fs::write(&transcript, format!("{before}{{\"k\":\"text\",\"t\":\"after the crash\"}}\n{{\"k\":\"end\"}}\n")).unwrap();

    let socket = format!("aho-test-resume-{}", std::process::id());
    let p = ForeignProvider::new(TestCli, settings(&socket), log.clone()).unwrap();
    assert_eq!(p.status(child), Some(Status::Live));
    assert!(p.child_info(child).unwrap().running, "the run open at the crash is still open");
    let r = tokio::time::timeout(Duration::from_secs(10), p.wait(child)).await.unwrap().unwrap();
    assert_eq!(r.stop, StopReason::EndTurn);
    assert_eq!(r.last_message.as_deref(), Some("after the crash"));

    let entries = subagent_entries(&*log);
    let texts: Vec<&str> = entries.iter().filter(|(n, _)| n == "Message").filter_map(|(_, b)| b["text"].as_str()).collect();
    assert_eq!(texts, vec!["before the crash", "after the crash"], "nothing before the offset is mirrored again");
    let recovered = entries.iter().find(|(n, _)| n == "Recovered").unwrap();
    assert_eq!(recovered.1["state"], "resumed");
    assert_eq!(recovered.1["offset"].as_u64(), Some(off));
    let unknown: Vec<u64> = entries.iter().filter(|(n, _)| n == "Delivery").filter_map(|(_, b)| b["seq"].as_u64()).collect();
    assert_eq!(unknown, vec![1], "the confirmed prompt is not marked, the unconfirmed send is");
    let result = entries.iter().find(|(n, _)| n == "Result").unwrap();
    assert_eq!(result.1["stop"], "end_turn");

    // No tmux session exists for it on this socket: gone only after two scans.
    if foreign_cli::which("tmux").is_some() {
        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some((_, b)) = subagent_entries(&*log).into_iter().find(|(n, _)| n == "Closed") {
                    return b;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(closed["reason"].as_str().unwrap().contains("missing on two scans"), "{closed}");
        assert_eq!(p.status(child), Some(Status::Closed));
    } else {
        println!("skipping the two-scan check: tmux is not on PATH");
    }
}
