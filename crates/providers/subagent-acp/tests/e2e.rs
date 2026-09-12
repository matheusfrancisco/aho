//! subagent-acp drives surface-acp in-process: the parent is the ACP client, the child a real
//! aho Session behind the ACP agent surface, connected by an in-memory duplex. Scripted model,
//! test tools (`note`, a network `fetch` that needs approval, a cancel-aware `slow`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aho_approval::{ApprovalRequest, ChannelApproval, Decided};
use aho_log::{EntryKind, GrantScope, LogStore, MemoryLog};
use aho_policy::SandboxPolicy;
use aho_subagent::{Mode, SpawnRequest, SubagentProvider};
use aho_types::*;
use common::{ChildAho, connector};
use llm_scripted::Step;
use serde_json::json;
use subagent_acp::{AcpProvider, Launch, ProviderSettings};
use tokio::sync::mpsc;

// ───────────────────────────── the parent side ─────────────────────────────

struct Parent {
    provider: AcpProvider,
    log: Arc<dyn LogStore>,
    cards: mpsc::Receiver<ApprovalRequest>,
    dir: tempfile::TempDir,
}

fn parent(child: &Arc<ChildAho>) -> Parent {
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let (approval, cards) = ChannelApproval::new(4);
    let settings = ProviderSettings {
        agents: BTreeMap::from([("aho".to_string(), Launch::Connect(connector(child.clone())))]),
        broken: BTreeMap::new(),
        default: "aho".into(),
        max_depth: 3,
        handshake: Duration::from_secs(10),
        secrets: vec![],
    };
    Parent { provider: AcpProvider::new(settings, Arc::new(approval), log.clone()), log, cards, dir: tempfile::tempdir().unwrap() }
}

impl Parent {
    fn policy(&self, allowed: ConsequenceSet) -> SandboxPolicy {
        let d = self.dir.path().display().to_string();
        SandboxPolicy::new(allowed, aho_cap::BoundWorld::host(), d.clone(), d, Epoch(1))
    }

    fn mirrored(&self) -> Vec<(String, Value)> {
        self.log
            .read_from(None)
            .into_iter()
            .filter_map(|e| match e.kind {
                EntryKind::Custom { ns, name, body } if ns == "Subagent" => Some((name, body)),
                _ => None,
            })
            .collect()
    }

    /// Waits until the parent log has a `Subagent/<name>` entry.
    async fn until_mirrored(&self, name: &str) {
        for _ in 0..500 {
            if self.mirrored().iter().any(|(n, _)| n == name) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no Subagent/{name} in the parent log: {:#?}", self.mirrored());
    }

    async fn card(&mut self) -> ApprovalRequest {
        tokio::time::timeout(Duration::from_secs(20), self.cards.recv()).await.expect("a card within 20 s").expect("the channel is open")
    }
}

fn request(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "scout".into(),
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

async fn wait(p: &Parent, child: SessionId) -> aho_subagent::Result_ {
    tokio::time::timeout(Duration::from_secs(30), p.provider.wait(child)).await.expect("the child finishes within 30 s").unwrap()
}

// ───────────────────────────── tests ─────────────────────────────

#[tokio::test]
async fn a_prompt_streams_into_the_parent_log_and_returns_the_last_message() {
    let child = ChildAho::new(vec![
        Step::text("Looking around.").with_thinking("start with a note").with_call(Some("n1"), "note", json!({"text": "x"})),
        Step::text("All done.").expect_tool_result("noted"),
    ]);
    let p = parent(&child);
    let id = p.provider.spawn(request("survey the repo"), p.policy(ConsequenceSet::ALL)).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref(), r.diagnostic.as_deref()), (StopReason::EndTurn, Some("All done."), None));

    let m = p.mirrored();
    let names: Vec<&str> = m.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Attached", "Sent", "Note", "Message", "ToolUse", "ToolResult", "Message", "TurnEnd"], "{m:#?}");
    assert_eq!(m[0].1["agent_info"]["name"], "aho", "the child is aho's own ACP surface");
    assert_eq!(m[2].1, json!({"child": id.0.to_string(), "provider": "subagent-acp", "text": "start with a note", "thinking": true}));
    assert_eq!(m[3].1["text"], "Looking around.");
    assert_eq!((m[4].1["id"].clone(), m[4].1["name"].clone(), m[4].1["kind"].clone()), (json!("n1"), json!("note"), json!("read")));
    assert_eq!((m[5].1["is_error"].clone(), m[5].1["content"].clone()), (json!(false), json!("noted")));
    assert_eq!((m[7].1["stop"].clone(), m[7].1["last_message"].clone()), (json!("end_turn"), json!("All done.")));
    // the child session ran in the parent's workspace
    let sessions = child.sessions.lock().unwrap().clone();
    assert_eq!(sessions[0].policy_view().workspace, p.dir.path().display().to_string());
}

#[tokio::test]
async fn the_childs_permission_requests_are_decided_by_the_parents_operator() {
    let child = ChildAho::new(vec![
        Step::tool_call("fetch", json!({"url": "https://example.com"})),
        Step::text("fetched it").expect_tool_result("fetched"),
        Step::tool_call("fetch", json!({"url": "https://example.org"})),
        Step::text("ok, not fetching").expect_tool_result("rejected"),
    ]);
    let mut p = parent(&child);
    let id = p.provider.spawn(request("get the page"), p.policy(ConsequenceSet::ALL)).await.unwrap();

    let (card, reply) = p.card().await;
    assert_eq!((card.tool.as_str(), card.class), ("acp:aho/fetch", Consequence::NetworkEgress));
    assert_eq!(card.reason, aho_policy::DecisionReason::Delegation);
    assert_eq!(card.args, json!({"url": "https://example.com"}));
    assert!(card.rendered.contains("Subagent `scout`") && card.rendered.contains("fetch: https://example.com"), "{}", card.rendered);
    reply.send(Decided::allowed(GrantScope::Once)).unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::EndTurn, Some("fetched it")));

    p.provider.send(id, "and the other one".into()).await.unwrap();
    let (_card, reply) = p.card().await;
    reply.send(Decided::rejected()).unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::EndTurn, Some("ok, not fetching")));
    let blocked: Vec<Value> = p.mirrored().into_iter().filter(|(n, _)| n == "Blocked").map(|(_, b)| b).collect();
    assert_eq!(blocked.len(), 1, "only the refusal is a block");
    assert_eq!(blocked[0]["reason"], "the operator rejected it");
    let results: Vec<Value> = p.mirrored().into_iter().filter(|(n, _)| n == "ToolResult").map(|(_, b)| b["is_error"].clone()).collect();
    assert_eq!(results, [json!(false), json!(true)]);
    assert_eq!(child.llm.remaining(), 0);
}

#[tokio::test]
async fn requests_outside_the_child_policy_are_refused_without_asking() {
    let child = ChildAho::new(vec![
        Step::tool_call("fetch", json!({"url": "https://example.com"})),
        Step::text("blocked, moving on").expect_tool_result("rejected"),
    ]);
    let mut p = parent(&child);
    // parent ⊓ declaration without NetworkEgress
    let narrow = ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite, Consequence::TalksToHuman]);
    let id = p.provider.spawn(request("get the page"), p.policy(narrow)).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::EndTurn, Some("blocked, moving on")));
    assert!(p.cards.try_recv().is_err(), "the operator was never asked");
    let blocked: Vec<Value> = p.mirrored().into_iter().filter(|(n, _)| n == "Blocked").map(|(_, b)| b).collect();
    assert!(blocked[0]["reason"].as_str().unwrap().contains("NetworkEgress"), "{blocked:?}");
}

#[tokio::test]
async fn an_async_child_without_bubble_cannot_reach_the_operator() {
    let child = ChildAho::new(vec![
        Step::tool_call("fetch", json!({"url": "https://example.com"})),
        Step::text("no channel").expect_tool_result("rejected"),
    ]);
    let mut p = parent(&child);
    let mut req = request("get the page");
    req.is_async = true;
    let id = p.provider.spawn(req, p.policy(ConsequenceSet::ALL)).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!(r.last_message.as_deref(), Some("no channel"));
    assert!(p.cards.try_recv().is_err(), "an async, unbubbled child has no approval channel");
}

#[tokio::test]
async fn interrupting_mid_turn_cancels_the_childs_turn_and_the_child_carries_on() {
    let child = ChildAho::new(vec![
        Step::tool_call("slow", json!({})),
        Step::text("resumed"),
        // the cancelled call's late result may land inside the next turn and earn one more request (16)
        Step::text("resumed"),
    ]);
    let p = parent(&child);
    let id = p.provider.spawn(request("wait a while"), p.policy(ConsequenceSet::ALL)).await.unwrap();
    p.until_mirrored("ToolUse").await;
    let busy = format!("{:?}", p.provider.send(id, "more".into()).await.unwrap_err());
    assert!(busy.contains("one prompt at a time"), "{busy}");
    assert!(p.provider.child_info(id).unwrap().running);
    p.provider.interrupt(id).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::Aborted, None));
    assert!(r.diagnostic.unwrap().contains("parent interrupted"));
    let last = p.provider.child_info(id).unwrap();
    assert_eq!((last.running, last.last_stop), (false, Some(StopReason::Aborted)));
    assert_eq!(child.llm.remaining(), 2, "the child's model was not called again");
    let over: Vec<Value> = p.mirrored().into_iter().filter(|(n, _)| n == "ToolResult").map(|(_, b)| b).collect();
    assert_eq!(over[0]["is_error"], true, "the interrupted call is mirrored as over: {over:?}");

    p.provider.send(id, "go on".into()).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::EndTurn, Some("resumed")));
}

#[tokio::test]
async fn interrupting_while_a_permission_request_waits_answers_it_cancelled() {
    let child = ChildAho::new(vec![Step::tool_call("fetch", json!({"url": "https://example.com"})), Step::text("carrying on")]);
    let mut p = parent(&child);
    let id = p.provider.spawn(request("get the page"), p.policy(ConsequenceSet::ALL)).await.unwrap();
    let (_card, reply) = p.card().await;
    p.provider.interrupt(id).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::Aborted, None));
    assert!(reply.is_closed(), "the operator's card was withdrawn");
    let sessions = child.sessions.lock().unwrap().clone();
    let refused = sessions[0].log().read_from(None).into_iter().any(|e| {
        matches!(&e.kind, EntryKind::ToolResult { content, .. } if content.iter().any(|b| b.as_text().is_some_and(|t| t.contains("cancelled before anyone answered"))))
    });
    assert!(refused, "the child's call was refused as cancelled, not run");

    p.provider.send(id, "try something else".into()).await.unwrap();
    let r = wait(&p, id).await;
    assert_eq!((r.stop, r.last_message.as_deref()), (StopReason::EndTurn, Some("carrying on")));
}
