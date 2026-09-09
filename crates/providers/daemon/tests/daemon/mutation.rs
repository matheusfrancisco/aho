//! Real socket transactions and the shared schedule/workflow admission path. No sleeps decide
//! whether the race occurred: each driver future is polled while the transaction owns admission.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use aho_approval::Approval;
use aho_control::PromptSink;
use aho_log::{EntryKind, LogStore};
use aho_session::Session;
use aho_types::*;
use daemon::client::Client;
use daemon::{ServeExit, ServeOptions, SessionHost, serve};
use llm_scripted::{ScriptedLlm, Step};
use serde_json::{Value, json};
use session_drive::SessionSink;

use crate::rig::{Rig, boot, rig};
use crate::serve::connect_when_up;

struct RebootHost {
    rig: Rig,
    steps: Vec<Step>,
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl RebootHost {
    fn latest(&self) -> Arc<Session> {
        self.sessions.lock().unwrap().last().unwrap().clone()
    }
}

impl SessionHost for RebootHost {
    fn sessions_dir(&self) -> PathBuf {
        self.rig.dir.path().join("sessions")
    }
    fn boot(&self, id: SessionId, log: Arc<dyn LogStore>, approval: Arc<dyn Approval>) -> BoxFuture<'_, Result<Arc<Session>, String>> {
        Box::pin(async move {
            let session = boot(&self.rig, id, Arc::new(ScriptedLlm::from_steps(self.steps.clone())), log, approval).await?;
            self.sessions.lock().unwrap().push(session.clone());
            Ok(session)
        })
    }
}

async fn start(steps: Vec<Step>) -> (Arc<RebootHost>, Client, Value, tokio::task::JoinHandle<Result<ServeExit, String>>) {
    let r = rig();
    let socket = r.dir.path().join("mutation.sock");
    let host = Arc::new(RebootHost { rig: r, steps, sessions: Mutex::new(Vec::new()) });
    let options = ServeOptions {
        socket: socket.clone(),
        idle_exit: None,
        reload_grace: Duration::from_secs(1),
        scan_interval: Duration::from_secs(60),
    };
    let server = tokio::spawn(serve(options, host.clone(), None));
    let mut client = connect_when_up(&socket).await;
    client.hello("mutation-owner").await.unwrap();
    let sid = client.request("session.open", json!({})).await.unwrap().payload["session"].clone();
    let attached = client.request("session.attach", json!({"session": sid, "watch": false})).await.unwrap();
    assert_eq!(attached.payload["owner"], true);
    (host, client, sid, server)
}

async fn stop(mut client: Client, server: tokio::task::JoinHandle<Result<ServeExit, String>>) {
    assert_eq!(client.request("daemon.shutdown", json!({})).await.unwrap().event, "result");
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap(), Ok(ServeExit::Shutdown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_and_workflow_cannot_enter_between_command_and_reopen() {
    let (host, mut client, sid, server) = start(vec![Step::text("new plan")]).await;
    let old = host.latest();
    let applied = client
        .request(
            "session.command",
            json!({
                "session": sid,
                "kind": {"type": "policy_epoch", "epoch": 2, "allowed": [], "preset": "read-only", "approval_ref": null}
            }),
        )
        .await
        .unwrap();
    assert_eq!(applied.event, "result", "{applied:?}");
    let at = EntryId(applied.payload["entry"].as_u64().unwrap());
    for by in ["schedule", "workflow"] {
        let sink = SessionSink::new(old.clone());
        let mut attempt = sink.prompt(by, "must wait for replacement".into(), format!("{by}:blocked"));
        let blocked = std::future::poll_fn(|cx| Poll::Ready(attempt.as_mut().poll(cx).is_pending())).await;
        assert!(blocked, "{by} entered the old plan between command and reopen");
        drop(attempt);
    }
    assert!(!old.log().read_from(Some(at)).iter().any(|e| matches!(e.kind, EntryKind::TurnStart { .. })));
    let foreground = client.request("session.prompt", json!({"session": sid, "text": "old", "wait": true})).await.unwrap();
    assert!(foreground.is_error(), "foreground turns share the reservation: {foreground:?}");
    let reopened = client.request("session.reopen", json!({"session": sid})).await.unwrap();
    assert_eq!(reopened.event, "result", "{reopened:?}");
    let sink = SessionSink::new(host.latest());
    let report = sink.prompt("schedule", "new".into(), "schedule:after".into()).await.unwrap();
    assert!(report.ran);
    assert_eq!(report.final_text, "new plan");
    let entries = old.log().read_from(Some(at));
    let boot = entries.iter().position(|e| matches!(e.kind, EntryKind::SessionMeta { .. })).unwrap();
    let turn = entries.iter().position(|e| matches!(e.kind, EntryKind::TurnStart { .. })).unwrap();
    assert!(boot < turn, "replacement boot must precede any driver turn");
    stop(client, server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_scripted_model_after_reopen_never_reuses_logged_call_ids() {
    let (host, mut client, sid, server) = start(vec![
        Step::tool_call("missing-tool", json!({})).with_call(Some("explicit"), "missing-too", json!({})),
        Step::text("finished"),
    ])
    .await;
    for reopen in [false, true] {
        if reopen {
            let response = client.request("session.reopen", json!({"session": sid})).await.unwrap();
            assert_eq!(response.event, "result", "{response:?}");
        }
        let response = client.request("session.prompt", json!({"session": sid, "text": "run", "wait": true})).await.unwrap();
        assert_eq!(response.event, "result", "{response:?}");
        assert_eq!(response.payload["transition"]["transition"], "completed", "{response:?}");
    }
    let entries = host.latest().log().read_from(None);
    let calls: Vec<_> = entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolCall { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 4, "both turns must execute both scripted calls");
    let unique: std::collections::BTreeSet<_> = calls.iter().collect();
    assert_eq!(unique.len(), 4, "reopening reused call ids: {calls:?}");
    let folded = host.latest().fold().unwrap();
    assert!(folded.open_calls.is_empty(), "every call has its own terminal result");
    stop(client, server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_releases_clean_reservations_and_closes_dirty_ones() {
    let (host, mut client, sid, server) = start(vec![Step::text("recovered")]).await;
    assert_eq!(client.request("session.mutation", json!({"session": sid})).await.unwrap().event, "result");
    assert_eq!(client.request("session.detach", json!({"session": sid})).await.unwrap().event, "result");
    // Opening is serialized behind cleanup; no timeout-based retry is needed.
    assert_eq!(client.request("session.open", json!({"id": sid})).await.unwrap().event, "result");
    assert_eq!(client.request("session.attach", json!({"session": sid, "watch": false})).await.unwrap().event, "result");
    assert_eq!(client.request("session.mutation", json!({"session": sid})).await.unwrap().event, "result");
    let note =
        client.request("session.command", json!({"session": sid, "kind": {"type": "note", "text": "interrupted change"}})).await.unwrap();
    assert_eq!(note.event, "result");
    assert_eq!(client.request("session.detach", json!({"session": sid})).await.unwrap().event, "result");
    assert_eq!(client.request("session.open", json!({"id": sid})).await.unwrap().event, "result");
    assert_eq!(client.request("session.attach", json!({"session": sid, "watch": false})).await.unwrap().event, "result");
    let result = client.request("session.prompt", json!({"session": sid, "text": "after interruption", "wait": true})).await.unwrap();
    assert_eq!(result.event, "result", "{result:?}");
    assert!(host.latest().fold().unwrap().turn.is_none());
    stop(client, server).await;
}
