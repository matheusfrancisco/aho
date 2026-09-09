//! Operator methods over the socket: `session.command`, `session.call`, `session.subscribe`,
//! `session.reopen`, `session.prompt{wait}`, and the host hooks around closing and exec.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aho_log::{EntryKind, JsonlLog, LogStore};
use aho_types::*;
use daemon::client::{Client, connect};
use daemon::{ServeExit, ServeOptions, serve};
use llm_scripted::{ScriptedLlm, Step};
use serde_json::{Value, json};

use crate::rig::{Host, rig};
use crate::serve::{connect_when_up, next};

fn opts(socket: &Path) -> ServeOptions {
    ServeOptions {
        socket: socket.to_path_buf(),
        idle_exit: None,
        reload_grace: Duration::from_millis(500),
        scan_interval: Duration::from_secs(60),
    }
}

fn log_of(sessions: &Path, sid: &Value) -> PathBuf {
    sessions.join(format!("{:x}.jsonl", sid.as_str().unwrap().parse::<u128>().unwrap()))
}

async fn owner(c: &mut Client, name: &str) -> Value {
    c.hello(name).await.unwrap();
    let sid = c.request("session.open", json!({})).await.unwrap().payload["session"].clone();
    let att = c.request("session.attach", json!({ "session": sid, "watch": false })).await.unwrap();
    assert_eq!(att.payload["owner"], json!(true), "{att:?}");
    sid
}

fn refused(f: &daemon::wire::ServerFrame, needle: &str) {
    assert!(f.is_error() && f.message().unwrap().contains(needle), "wanted an error with `{needle}`: {f:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_commands_append_for_the_owner_and_a_stranger_is_read_only() {
    let r = rig();
    let sessions = r.dir.path().join("sessions");
    let socket = r.dir.path().join("d.sock");
    let host = Arc::new(Host::new(r, Arc::new(ScriptedLlm::from_steps(vec![])), sessions.clone()));
    let server = tokio::spawn(serve(opts(&socket), host.clone(), None));
    let mut a = connect_when_up(&socket).await;
    let sid = owner(&mut a, "A").await;

    let note =
        a.request("session.command", json!({ "session": sid, "kind": { "type": "note", "text": "operator was here" } })).await.unwrap();
    assert_eq!(note.event, "result", "{note:?}");
    let at = EntryId(note.payload["entry"].as_u64().unwrap());
    let echoed = a.request("session.call", json!({ "session": sid, "method": "echo", "params": { "x": 1 } })).await.unwrap();
    assert_eq!((echoed.payload["session"].clone(), echoed.payload["params"].clone()), (sid.clone(), json!({ "x": 1 })), "{echoed:?}");
    refused(&a.request("session.call", json!({ "session": sid, "method": "nope" })).await.unwrap(), "no host method `nope`");

    // Kinds only the session writes, and entries its fold would reject, append nothing.
    let before = JsonlLog::open(log_of(&sessions, &sid)).unwrap().read_from(None).len();
    refused(
        &a.request("session.command", json!({ "session": sid, "kind": { "type": "turn_start", "turn": 9 } })).await.unwrap(),
        "written by the session itself",
    );
    let older_epoch = json!({ "type": "policy_epoch", "epoch": 0, "allowed": [], "preset": "none", "approval_ref": null });
    refused(&a.request("session.command", json!({ "session": sid, "kind": older_epoch })).await.unwrap(), "would not fold");
    assert_eq!(JsonlLog::open(log_of(&sessions, &sid)).unwrap().read_from(None).len(), before);

    // A stranger (another instance, no local history) while A is attached: it may watch, not drive.
    let mut b = connect(&socket).await.unwrap();
    b.hello("B").await.unwrap();
    let att = b.request("session.attach", json!({ "session": sid, "has_local_history": false, "watch": false })).await.unwrap();
    assert_eq!(att.payload["takeover"], json!("refused"), "{att:?}");
    for (method, params) in [
        ("session.command", json!({ "session": sid, "kind": { "type": "note", "text": "not mine" } })),
        ("session.call", json!({ "session": sid, "method": "echo" })),
        ("session.reopen", json!({ "session": sid })),
        ("session.prompt", json!({ "session": sid, "text": "hi", "wait": true })),
    ] {
        refused(&b.request(method, params).await.unwrap(), "does not drive");
    }
    let watched = b.request("session.subscribe", json!({ "session": sid })).await.unwrap();
    assert_eq!(watched.event, "result", "a read-only client may still watch: {watched:?}");
    assert!(watched.payload["replayed"].as_u64().unwrap() > 0, "{watched:?}");

    // Watching needs an attach (it is what logs the decision).
    let mut c = connect(&socket).await.unwrap();
    c.hello("C").await.unwrap();
    refused(&c.request("session.subscribe", json!({ "session": sid })).await.unwrap(), "has not attached");

    let log = JsonlLog::open(log_of(&sessions, &sid)).unwrap();
    let e = log.get(at).unwrap();
    assert_eq!((e.by, e.kind), (Principal::Operator, EntryKind::Note { text: "operator was here".into() }));
    assert!(!log.read_from(None).iter().any(|e| matches!(&e.kind, EntryKind::Note { text } if text == "not mine")));
    assert_eq!(host.journal().iter().filter(|j| j.starts_with("call ")).count(), 2, "{:?}", host.journal());

    assert_eq!(a.request("daemon.shutdown", Value::Null).await.unwrap().event, "result");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_boots_again_over_the_log_and_moves_the_stream_without_gaps() {
    let r = rig();
    let sessions = r.dir.path().join("sessions");
    let socket = r.dir.path().join("d.sock");
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("after the reopen")]));
    let host = Arc::new(Host::new(r, llm.clone(), sessions.clone()));
    let server = tokio::spawn(serve(opts(&socket), host.clone(), None));
    let mut a = connect_when_up(&socket).await;
    let sid = owner(&mut a, "A").await;
    let n: u64 = sid.as_str().unwrap().parse().unwrap();

    let sub = a.request("session.subscribe", json!({ "session": sid })).await.unwrap();
    let replayed = sub.payload["replayed"].as_u64().unwrap();
    let mut ids = Vec::new();
    for _ in 0..replayed {
        let f = next(&mut a).await;
        assert_eq!(f.event, "entry", "the replay comes before the reply: {f:?}");
        ids.push(f.payload["entry"]["id"].as_u64().unwrap());
    }
    assert_eq!(sub.payload["head"], json!(ids.last().unwrap()), "{sub:?}");

    let note = a.request("session.command", json!({ "session": sid, "kind": { "type": "note", "text": "plan changed" } })).await.unwrap();
    let reopened = a.request("session.reopen", json!({ "session": sid })).await.unwrap();
    assert_eq!(reopened.event, "result", "{reopened:?}");
    let journal = host.journal();
    let tail: Vec<&str> = journal.iter().rev().take(2).rev().map(String::as_str).collect();
    assert_eq!(tail, [format!("closing {n}"), format!("boot {n}")], "{journal:?}");

    // The same owner prompts the rebooted session and the reply waits for the turn's end.
    let done = a.request("session.prompt", json!({ "session": sid, "text": "go on", "wait": true })).await.unwrap();
    assert_eq!((done.event.as_str(), done.payload["transition"]["transition"].clone()), ("result", json!("completed")), "{done:?}");
    assert_eq!(llm.remaining(), 0);

    // The stream carried on from where it was: the note, the new boot's entries, the turn; no repeats.
    let mut kinds = Vec::new();
    loop {
        let f = next(&mut a).await;
        if f.event == "entry" {
            ids.push(f.payload["entry"]["id"].as_u64().unwrap());
            kinds.push(f.payload["entry"]["kind"]["type"].as_str().unwrap().to_string());
            if kinds.last().map(String::as_str) == Some("turn_end") {
                break;
            }
        }
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "entry ids strictly increase: {ids:?}");
    let note_pos = kinds.iter().position(|k| k == "note").unwrap();
    let meta_pos = kinds.iter().position(|k| k == "session_meta").unwrap();
    assert!(note_pos < meta_pos, "the note, then the new boot: {kinds:?}");
    assert!(ids.contains(&note.payload["entry"].as_u64().unwrap()));

    assert_eq!(a.request("daemon.shutdown", Value::Null).await.unwrap().event, "result");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
    assert_eq!(host.journal().last().map(String::as_str), Some(format!("closing {n}").as_str()), "exit closes every hosted session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_runs_before_exec_and_a_failed_exec_restarts_the_host() {
    let r = rig();
    let sessions = r.dir.path().join("sessions");
    let socket = r.dir.path().join("d.sock");
    // Marked executable for others but not for its owner (us): exec fails with EACCES, which,
    // unlike ENOEXEC, `execvp` does not retry through `/bin/sh` — this test process stays.
    let broken = r.dir.path().join("broken-aho");
    std::fs::write(&broken, "not a program\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o601)).unwrap();
    let host = Arc::new(Host::new(r, Arc::new(ScriptedLlm::from_steps(vec![])), sessions.clone()));
    let server = tokio::spawn(serve(opts(&socket), host.clone(), None));
    let mut a = connect_when_up(&socket).await;
    let sid = owner(&mut a, "A").await;

    let started = a.request("daemon.reload", json!({ "binary": broken, "session": sid })).await.unwrap();
    assert_eq!(started.event, "reload.started", "{started:?}");
    assert!(started.payload["problems"].to_string().contains("driver scheduler: stopped late"), "hook problems are reported: {started:?}");
    let failed = next(&mut a).await;
    assert_eq!(failed.event, "reload.failed", "{failed:?}");
    let journal = host.journal();
    let hooks: Vec<&str> = journal.iter().filter(|j| !j.starts_with("boot")).map(String::as_str).collect();
    assert_eq!(hooks, ["before_exec", "exec_failed"], "{journal:?}");

    // Still serving with this image.
    let mut again = connect_when_up(&socket).await;
    again.hello("A2").await.unwrap();
    assert_eq!(again.request("daemon.shutdown", Value::Null).await.unwrap().event, "result");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
}
