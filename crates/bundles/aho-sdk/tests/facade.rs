//! The Rust facade against a real session: the aho-sdk-min composition, the scripted model, the
//! host world, and surface-rpc answering every call.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::time::Duration;

use aho_config::Layer;
use aho_llm::StreamEvent;
use aho_log::{CapState, EntryKind, GrantScope, ToolOutcome, Transition};
use aho_sdk::{Answer, Client, Event, Open, Sdk, SdkError, Spawn};
use aho_types::*;
use serde_json::json;

struct Fx {
    _dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
}

fn fx(steps: serde_json::Value) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(project.join("a.txt"), "hello from disk\n").unwrap();
    std::fs::write(dir.path().join("script.json"), json!({ "steps": steps }).to_string()).unwrap();
    Fx { project, home, _dir: dir }
}

impl Fx {
    fn script(&self) -> PathBuf {
        self.project.parent().unwrap().join("script.json")
    }
    fn builder(&self) -> aho_sdk::SdkBuilder {
        let b = Sdk::builder().project(&self.project).home(&self.home).script(self.script());
        match std::env::var("PATH") {
            Ok(p) => b.env("PATH", &p),
            Err(_) => b,
        }
    }
}

/// Everything already received, without waiting.
fn drain(ev: &mut aho_sdk::Events) -> Vec<Event> {
    std::iter::from_fn(|| ev.try_next()).collect()
}

fn kinds(evs: &[Event]) -> Vec<&EntryKind> {
    evs.iter().filter_map(|e| if let Event::Entry(e) = e { Some(&e.kind) } else { None }).collect()
}

async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f).await.expect("finished within 30 s")
}

#[tokio::test]
async fn an_embedded_session_runs_a_tool_turn_and_streams_typed_events() {
    let f = fx(json!([
        {"tool_calls": [{"name": "read", "args": {"path": "a.txt"}}], "stop": "tool_use"},
        {"expect_tool_result_contains": "hello from disk", "text": "It says hello."}
    ]));
    let sdk = f.builder().build().unwrap();
    let s = sdk.open_session(Open::New).await.unwrap();
    let mut events = s.events().unwrap();
    assert!(matches!(s.events(), Err(SdkError::Config(_))), "one event stream per connection");
    let sub = s.subscribe(None).await.unwrap();
    assert!(sub.replayed > 0, "bootstrap entries are replayed");
    assert_eq!(within(s.prompt("read a.txt")).await.unwrap(), Transition::Completed);

    let evs = drain(&mut events);
    let ks = kinds(&evs);
    assert!(ks.iter().any(|k| matches!(k, EntryKind::PlanSealed { .. })), "the replay starts at the bootstrap");
    let call =
        ks.iter().find_map(|k| if let EntryKind::ToolCall { call, tool, .. } = k { Some((call.clone(), tool.clone())) } else { None });
    let (call, tool) = call.expect("a tool call entry");
    assert_eq!(tool, "read");
    let result = ks.iter().find_map(|k| match k {
        EntryKind::ToolResult { call: c, outcome, content, .. } if *c == call => Some((outcome.clone(), content.clone())),
        _ => None,
    });
    let (outcome, content) = result.expect("the call's result entry");
    assert_eq!(outcome, ToolOutcome::Ok);
    assert!(content.iter().filter_map(ContentBlock::as_text).any(|t| t.contains("hello from disk")));
    let last_text = ks.iter().rev().find_map(|k| match k {
        EntryKind::AssistantMessage { content, stop: StopReason::EndTurn, .. } => {
            Some(content.iter().filter_map(ContentBlock::as_text).collect::<String>())
        }
        _ => None,
    });
    assert_eq!(last_text.as_deref(), Some("It says hello."));
    assert!(evs.iter().any(|e| matches!(e, Event::Stream(StreamEvent::ToolUseStart { name, .. }) if name == "read")));
    assert!(evs.iter().any(|e| matches!(e, Event::Stream(StreamEvent::Text(t)) if t == "It says hello.")));
    let idle = evs.iter().position(|e| matches!(e, Event::Idle(i) if i.transition == Transition::Completed)).expect("idle");
    assert_eq!(idle, evs.len() - 1, "idle is the last event before the reply");

    let fold = s.fold().await.unwrap();
    assert_eq!(fold.turns, 1);
    assert_eq!(fold.caps.get("surface-rpc"), Some(&CapState::Active));
    assert_eq!(fold.policy.preset, "workspace");

    let (id, path) = (s.id(), s.log_path().unwrap().to_path_buf());
    s.close().await.unwrap();
    assert!(path.starts_with(std::fs::canonicalize(&f.project).unwrap().join(".aho/sessions")), "{}", path.display());
    // close() must release the log's writer lock immediately (a leaked Arc<Session> here once
    // kept it held forever via the approval pump); resume below would then fail to append.
    let probe = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    probe.try_lock().expect("the closed session released its writer lock");
    drop(probe);
    let resumed = sdk.open_session(Open::Resume(id)).await.unwrap();
    assert_eq!(resumed.fold().await.unwrap().turns, 1, "the log on disk resumes");
    resumed.close().await.unwrap();
}

async fn until_card(events: &mut aho_sdk::Events) -> aho_sdk::ApprovalCard {
    loop {
        match events.next().await.expect("the connection stays open until the card") {
            Event::Approval(c) => return *c,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn approval_cards_arrive_typed_and_approve_answers_them_once() {
    let f = fx(json!([
        {"tool_calls": [{"name": "write", "args": {"path": "b.txt", "content": "yes\n"}}], "stop": "tool_use"},
        {"expect_tool_result_contains": "b.txt", "text": "written"}
    ]));
    let sdk = f.builder().set("policy.rules", "[\"ask write(*)\"]").build().unwrap();
    let s = sdk.open_session(Open::InMemory).await.unwrap();
    let mut events = s.events().unwrap();
    let (t, card) = within(async {
        tokio::join!(s.prompt("write it"), async {
            let card = until_card(&mut events).await;
            assert!(!Path::new(&f.project.join("b.txt")).exists(), "nothing is written before the answer");
            assert_eq!(s.approve(card.id, Answer::Allow(GrantScope::Once)).await.unwrap(), aho_sdk::Answered::default());
            card
        })
    })
    .await;
    assert_eq!(t.unwrap(), Transition::Completed);
    assert_eq!((card.tool.as_str(), card.cap.as_deref(), card.class), ("write", Some("tool-fs"), Consequence::WorkspaceWrite));
    assert_eq!(card.reason, "Rule");
    assert_eq!(card.enforced_by, "policy only");
    assert_eq!(std::fs::read_to_string(f.project.join("b.txt")).unwrap(), "yes\n", "the world is rooted at the SDK's project");
    match s.approve(card.id, Answer::Allow(GrantScope::Once)).await {
        Err(SdkError::Rpc { method, message }) => {
            assert_eq!(method, "approval.answer");
            assert!(message.contains("answered or never asked"), "{message}");
        }
        other => panic!("a second answer must fail, got {other:?}"),
    }
    s.close().await.unwrap();
}

#[tokio::test]
async fn a_rejected_card_fails_the_call_and_the_turn_goes_on() {
    let f = fx(json!([
        {"tool_calls": [{"name": "write", "args": {"path": "b.txt", "content": "no\n"}}], "stop": "tool_use"},
        {"text": "understood"}
    ]));
    let sdk = f.builder().set("policy.rules", "[\"ask write(*)\"]").build().unwrap();
    let s = sdk.open_session(Open::InMemory).await.unwrap();
    let mut events = s.events().unwrap();
    s.subscribe(None).await.unwrap();
    let (t, ()) = within(async {
        tokio::join!(s.prompt("write it"), async {
            let card = until_card(&mut events).await;
            s.approve(card.id, Answer::Reject).await.unwrap();
        })
    })
    .await;
    assert_eq!(t.unwrap(), Transition::Completed);
    let evs = drain(&mut events);
    let outcome =
        kinds(&evs).into_iter().find_map(|k| if let EntryKind::ToolResult { outcome, .. } = k { Some(outcome.clone()) } else { None });
    assert!(matches!(outcome, Some(ToolOutcome::Err { .. })), "{outcome:?}");
    assert!(!f.project.join("b.txt").exists());
    s.close().await.unwrap();
}

#[tokio::test]
async fn the_embedders_project_is_the_workspace_root_unless_an_operator_layer_says_otherwise() {
    let f = fx(json!([]));
    let sdk = f.builder().build().unwrap();
    let root = &sdk.planned().loadout.config["workspace.root"];
    assert_eq!(root.value, json!(std::fs::canonicalize(&f.project).unwrap().display().to_string()));
    assert_eq!(root.winner.layer, Layer::Bundle(aho_sdk::NAME.into()));
    assert_eq!(sdk.planned().loadout.config["policy.preset"].winner.layer, Layer::Bundle(aho_sdk::NAME.into()));
    for id in ["surface-rpc", "tool-fs", "world-host", "llm-scripted"] {
        assert_eq!(sdk.planned().plan.row(id).map(|r| r.state.clone()), Some(CapState::Active), "{id}");
    }

    std::fs::create_dir_all(f.project.join("sub")).unwrap();
    std::fs::write(f.project.join("aho.toml"), "[workspace]\nroot = \"sub\"\n").unwrap();
    let sdk = f.builder().build().unwrap();
    let root = &sdk.planned().loadout.config["workspace.root"];
    assert_eq!((root.value.clone(), root.winner.layer.clone()), (json!("sub"), Layer::FileProject));
}

#[tokio::test]
async fn builder_errors_say_what_to_pass() {
    let f = fx(json!([]));
    let e = Sdk::builder().project(&f.project).build().err().expect("no home");
    assert!(e.to_string().contains(".home(dir)"), "{e}");
    let e = Sdk::builder().project(f.project.join("missing")).home(&f.home).build().err().expect("missing project");
    assert!(e.to_string().contains("cannot be opened"), "{e}");
    let e = f.builder().set("policy.preset", "yolo").build().err().expect("unknown preset");
    assert!(matches!(e, SdkError::Compose(_)) && e.to_string().contains("names no preset"), "{e}");
    let e = Sdk::builder().project(&f.project).home(&f.home).build().unwrap().open_session(Open::New).await.err().expect("no model");
    assert!(e.to_string().contains("no model route"), "{e}");
    let e = f.builder().build().unwrap().open_session(Open::Resume(SessionId(0xdead))).await.err().expect("no such session");
    assert!(e.to_string().contains("no session dead"), "{e}");
}

#[tokio::test]
async fn the_rust_client_drives_a_spawned_rpc_server() {
    let f = fx(json!([
        {"tool_calls": [{"name": "read", "args": {"path": "a.txt"}}], "stop": "tool_use"},
        {"text": "read it"}
    ]));
    let mut spec = Spawn::new(env!("CARGO_BIN_EXE_aho-sdk"))
        .set("model.provider", "scripted")
        .set("provider.scripted.script", &f.script().display().to_string());
    spec.project = Some(f.project.clone());
    spec.env.push(("HOME".into(), f.home.clone().into_os_string()));
    let c = Client::spawn(&spec, aho_sdk::hello("rust-test")).await.unwrap();
    assert_eq!(c.hello_ok().version, surface_rpc::VERSION_MAX);
    assert!(c.hello_ok().capabilities.iter().any(|m| m == "caps.set"));
    let mut events = c.events().unwrap();
    c.subscribe(None).await.unwrap();
    assert_eq!(within(c.prompt("read")).await.unwrap(), Transition::Completed);
    let evs = drain(&mut events);
    assert!(kinds(&evs).iter().any(|k| matches!(k, EntryKind::ToolResult { outcome: ToolOutcome::Ok, .. })));
    match c.request("session.teleport", json!({})).await {
        Err(SdkError::Rpc { message, .. }) => assert!(message.contains("unknown method"), "{message}"),
        other => panic!("{other:?}"),
    }
    let status = within(c.close()).await.unwrap().expect("a spawned server has an exit status");
    assert!(status.success(), "{status:?}");

    let dead = Spawn::new(f.project.join("no-such-binary"));
    let e = Client::spawn(&dead, aho_sdk::hello("x")).await.err().expect("missing binary");
    assert!(e.to_string().contains("could not start"), "{e}");
}
