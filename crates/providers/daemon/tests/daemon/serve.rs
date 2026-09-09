#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_log::{EntryKind, JsonlLog, LogStore, ProcessIdentity, RecoveryRole, Takeover};
use aho_types::*;
use daemon::client::{Client, connect};
use daemon::recovery::persist_intent;
use daemon::wire::ServerFrame;
use daemon::{ServeExit, ServeOptions, serve};
use llm_scripted::{ScriptedLlm, Step};
use serde_json::{Value, json};

use crate::rig::{Host, boot, rig};

pub async fn connect_when_up(socket: &Path) -> Client {
    for _ in 0..500 {
        if let Ok(c) = connect(socket).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the daemon never came up on {}", socket.display());
}

pub async fn next(c: &mut Client) -> ServerFrame {
    tokio::time::timeout(Duration::from_secs(20), c.next_event())
        .await
        .expect("an event in time")
        .unwrap()
        .expect("the connection stays open")
}

fn attached(log: &JsonlLog) -> Vec<(String, bool, Takeover)> {
    log.read_from(None)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::SurfaceAttached { client_instance, has_local_history, takeover } => {
                Some((client_instance, has_local_history, takeover))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_attach_and_prompt_over_the_socket_and_a_stranger_is_read_only() {
    let r = rig();
    let sessions = r.dir.path().join("sessions");
    let socket = r.dir.path().join("d.sock");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("hello back")]));
    let host = Arc::new(Host::new(r, llm.clone(), sessions.clone()));
    let opts = ServeOptions {
        socket: socket.clone(),
        idle_exit: None,
        reload_grace: Duration::from_millis(500),
        scan_interval: Duration::from_millis(100),
    };
    let server = tokio::spawn(serve(opts.clone(), host.clone(), None));
    let mut a = connect_when_up(&socket).await;

    let err = serve(opts.clone(), host.clone(), None).await.unwrap_err();
    assert!(err.contains("another daemon already serves"), "{err}");

    let mut early = connect(&socket).await.unwrap();
    assert!(early.request("session.list", Value::Null).await.unwrap().is_error(), "methods before hello are refused");
    let bad = early.request("hello", json!({ "min": 2, "max": 3 })).await.unwrap();
    assert!(bad.is_error() && bad.message().unwrap().contains("no common protocol version"), "{bad:?}");

    let hello = a.hello("A").await.unwrap();
    assert_eq!(hello.payload["version"], json!(1));
    let opened = a.request("session.open", json!({})).await.unwrap();
    assert_eq!(opened.event, "result", "{opened:?}");
    let sid = opened.payload["session"].clone();
    let attach = a.request("session.attach", json!({ "session": sid, "has_local_history": false })).await.unwrap();
    assert_eq!(attach.payload["takeover"], json!("allowed"), "{attach:?}");
    let accepted = a.request("session.prompt", json!({ "session": sid, "text": "hi there" })).await.unwrap();
    assert_eq!(accepted.event, "accepted", "{accepted:?}");

    let (mut saw_prompt, mut saw_answer) = (false, false);
    loop {
        let f = next(&mut a).await;
        match f.event.as_str() {
            "entry" => {
                let kind = &f.payload["entry"]["kind"];
                saw_prompt |= kind["type"] == json!("user_message") && kind["text"] == json!("hi there");
                saw_answer |= kind["type"] == json!("assistant_message");
            }
            "idle" => {
                assert_eq!(f.payload["transition"]["transition"], json!("completed"), "{f:?}");
                break;
            }
            "error" => panic!("{f:?}"),
            _ => {}
        }
    }
    assert!(saw_prompt && saw_answer);
    assert_eq!(llm.remaining(), 0);

    // Another instance without local history while A is attached: read-only.
    let mut b = connect(&socket).await.unwrap();
    b.hello("B").await.unwrap();
    let refused = b.request("session.attach", json!({ "session": sid, "has_local_history": false })).await.unwrap();
    assert_eq!(refused.payload["takeover"], json!("refused"), "{refused:?}");
    assert!(refused.payload["reason"].as_str().unwrap().contains("client `A`"), "{refused:?}");
    assert_eq!(next(&mut b).await.event, "entry", "a read-only client still gets the stream");
    for (method, params) in [
        ("session.prompt", json!({ "session": sid, "text": "mine now" })),
        ("session.steer", json!({ "session": sid, "text": "mine now", "urgent": true })),
        ("session.abort", json!({ "session": sid })),
    ] {
        let r = b.request(method, params).await.unwrap();
        assert!(r.is_error() && r.message().unwrap().contains("does not drive"), "{method}: {r:?}");
    }

    let listed = a.request("session.list", Value::Null).await.unwrap();
    let row = &listed.payload["sessions"][0];
    assert_eq!(
        (row["session"].clone(), row["status"].clone(), row["owner"]["client_instance"].clone()),
        (sid.clone(), json!("idle"), json!("A"))
    );

    // A reload into a binary that does not exist interrupts nothing.
    let reload = a.request("daemon.reload", json!({ "binary": "/nonexistent/aho", "session": sid })).await.unwrap();
    assert_eq!(reload.event, "reload.failed", "{reload:?}");
    assert!(reload.message().unwrap().contains("/nonexistent/aho"), "{reload:?}");

    let log = JsonlLog::open(sessions.join(format!("{:x}.jsonl", sid.as_str().unwrap().parse::<u128>().unwrap()))).unwrap();
    let decisions = attached(&log);
    assert_eq!(decisions.len(), 2, "{decisions:?}");
    assert_eq!(decisions[0], ("A".to_string(), false, Takeover::Allowed));
    assert!(decisions[1].0 == "B" && !decisions[1].1 && matches!(decisions[1].2, Takeover::Refused(_)), "{decisions:?}");
    assert!(!log.read_from(None).iter().any(|e| matches!(e.kind, EntryKind::SessionRecoveryIntent { .. })));

    let bye = a.request("daemon.shutdown", Value::Null).await.unwrap();
    assert_eq!(bye.event, "result", "{bye:?}");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
    assert!(!socket.exists(), "the socket is removed on exit");
}

async fn status(c: &mut Client, sid: &str) -> Value {
    let listed = c.request("session.list", Value::Null).await.unwrap();
    listed.payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session"] == json!(sid))
        .map(|r| r["status"].clone())
        .unwrap_or(Value::Null)
}

async fn until_status(c: &mut Client, sid: &str, want: &str) {
    for _ in 0..500 {
        if status(c, sid).await == json!(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {sid} never became {want}: {}", status(c, sid).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_continues_reloaded_sessions_and_lists_crashed_ones_for_resume() {
    let r = rig();
    let sessions = r.dir.path().join("sessions");
    let socket = r.dir.path().join("d.sock");

    // Session 11: a reload interrupted its turn and left an intent.
    let reloaded = Arc::new(JsonlLog::open(sessions.join(format!("{:x}.jsonl", 11u128))).unwrap());
    drop(boot(&r, SessionId(11), Arc::new(ScriptedLlm::from_steps(vec![])), reloaded.clone(), Arc::new(NeverApproval)).await.unwrap());
    reloaded.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(1) }).unwrap();
    reloaded
        .append(None, Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "keep going".into(), client_key: None })
        .unwrap();
    persist_intent(&*reloaded, RecoveryRole::Headless, "reload into /builds/aho").unwrap();
    drop(reloaded);

    // Session 12: its process died inside turn 2.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = ProcessIdentity { pid: child.id(), start_time: Timestamp::now() };
    child.wait().unwrap();
    let fork_point = {
        let log = JsonlLog::open(sessions.join(format!("{:x}.jsonl", 12u128))).unwrap();
        let a = |k: EntryKind| log.append(None, Principal::system("loop"), k).unwrap().id;
        a(EntryKind::SessionMeta { title: None, tags: vec![], last_prompt: None, process: Some(dead) });
        a(EntryKind::TurnStart { turn: TurnId(1) });
        let end = a(EntryKind::TurnEnd { turn: TurnId(1) });
        a(EntryKind::TurnStart { turn: TurnId(2) });
        end
    };

    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("continuing after the reload")]));
    let host = Arc::new(Host::new(r, llm.clone(), sessions.clone()));
    let opts = ServeOptions {
        socket: socket.clone(),
        idle_exit: None,
        reload_grace: Duration::from_millis(500),
        scan_interval: Duration::from_millis(50),
    };
    let server = tokio::spawn(serve(opts, host, None));
    let mut c = connect_when_up(&socket).await;
    c.hello("ops").await.unwrap();

    until_status(&mut c, "11", "idle").await;
    assert_eq!(llm.remaining(), 0, "the continuation ran");
    let log = JsonlLog::open(sessions.join(format!("{:x}.jsonl", 11u128))).unwrap();
    let keys: Vec<String> = log
        .read_from(None)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::UserMessage { client_key: Some(k), .. } => Some(k),
            _ => None,
        })
        .collect();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("recovery:"), "{keys:?}");

    until_status(&mut c, "12", "crashed").await;
    let open = c.request("session.open", json!({ "id": "12" })).await.unwrap();
    assert!(open.is_error() && open.message().unwrap().contains("session.resume"), "{open:?}");
    let resumed = c.request("session.resume", json!({ "session": "12" })).await.unwrap();
    assert_eq!(resumed.payload["fork"], json!(fork_point.0), "{resumed:?}");
    assert_eq!(status(&mut c, "12").await, json!("idle"));

    assert_eq!(c.request("daemon.shutdown", Value::Null).await.unwrap().event, "result");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
}
