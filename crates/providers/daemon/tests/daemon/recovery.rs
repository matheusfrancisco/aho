use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_log::{
    CancelReason, CustomFolds, EntryKind, JsonlLog, LogStore, OpKind, OpStatus, RecoveryRole, ToolOutcome, Transition, derive_messages,
    fold, request_hash as request_hash_of,
};
use aho_types::*;
use daemon::recovery::{CONTINUATION, close_open_turn, consume, persist_intent, unconsumed};
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;

use crate::rig::{boot, rig};

fn open(path: &std::path::Path) -> Arc<dyn LogStore> {
    Arc::new(JsonlLog::open(path).unwrap())
}

#[test]
fn the_continuation_forbids_asking_and_summarizing() {
    assert!(CONTINUATION.contains("do not ask what to do next"));
    assert!(CONTINUATION.contains("do not summarize the reload"));
}

#[tokio::test]
async fn a_reload_intent_is_consumed_exactly_once() {
    let r = rig();
    let path = r.dir.path().join(format!("sessions/{:x}.jsonl", 9u128));
    let sid = SessionId(9);

    // The old process: a turn is running a side-effecting call when the reload is requested.
    {
        let log = open(&path);
        let s = boot(&r, sid, Arc::new(ScriptedLlm::from_steps(vec![])), log.clone(), Arc::new(NeverApproval)).await.unwrap();
        drop(s);
        let a = |k: EntryKind| log.append(None, Principal::system("loop"), k).unwrap();
        a(EntryKind::TurnStart { turn: TurnId(1) });
        a(EntryKind::UserMessage { form: Form::Prompt, text: "deploy it".into(), client_key: None });
        a(EntryKind::AssistantMessage {
            content: vec![ContentBlock::ToolUse { id: CallId("c1".into()), name: "bash".into(), args: json!({"command": "./deploy"}) }],
            usage: Usage::default(),
            stop: StopReason::ToolUse,
        });
        a(EntryKind::ToolCall {
            call: CallId("c1".into()),
            tool: "bash".into(),
            args: json!({"command": "./deploy"}),
            class: Consequence::SpawnsProcess,
        });
        a(EntryKind::OpStarted { op: OpId(1), call: CallId("c1".into()), kind: OpKind::Process, rerunnable: false, external_effect: true });
        persist_intent(&*log, RecoveryRole::Initiator, "reload into /builds/aho").unwrap();
    }

    // The new image: close the cut turn, boot, run the continuation. (`expect_user` cannot pin the
    // prompt here: a freshly booted session logs a policy `Snapshot` user message at its first
    // step, so the prompt is not the last user message. The request itself is checked below.)
    let llm = Arc::new(ScriptedLlm::from_steps(vec![Step::text("picking the deploy back up")]));
    {
        let log = open(&path);
        assert_eq!(unconsumed(&*log).len(), 1);
        assert!(close_open_turn(&*log, CancelReason::Reload).unwrap());
        let s = boot(&r, sid, llm.clone(), log.clone(), Arc::new(NeverApproval)).await.unwrap();
        assert_eq!(consume(&s, &["job 7: cargo build (call c9)".into()]).await.unwrap(), 1);
        assert_eq!(llm.remaining(), 0);
        assert!(unconsumed(&*log).is_empty());

        let entries = log.read_branch(log.head().unwrap()).unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                &e.kind,
                EntryKind::OpDone { op: OpId(1), status: OpStatus::Cancelled { reason: CancelReason::Reload } }
            ))
        );
        assert!(entries.iter().any(|e| matches!(&e.kind,
            EntryKind::ToolResult { call, outcome: ToolOutcome::Cancelled { reason: CancelReason::Reload }, content, .. }
                if call.0 == "c1" && content.iter().filter_map(|b| b.as_text()).any(|t| t.contains("may or may not have happened")))));
        assert!(entries.iter().any(|e| matches!(&e.kind, EntryKind::StepEnd { transition: Transition::Reload, .. })));
        assert!(
            entries.iter().any(|e| matches!(&e.kind,
            EntryKind::UserMessage { text, client_key: Some(k), .. } if k.starts_with("recovery:") && text.contains("job 7: cargo build")))
        );

        // The model request of the continuation turn carried the continuation, and the turn completed.
        let at = entries.iter().rposition(|e| matches!(e.kind, EntryKind::RequestHeader { .. })).unwrap();
        let EntryKind::RequestHeader { request_hash, tools_visible, .. } = &entries[at].kind else { unreachable!() };
        let messages = derive_messages(&fold(&entries[..at], &CustomFolds::new()).unwrap());
        assert_eq!(request_hash_of(&messages, tools_visible), *request_hash, "the derived messages are the ones sent");
        assert!(messages.iter().any(|m| m.role == Role::User && m.text().contains("do not ask what to do next")));
        assert!(entries[at..].iter().any(|e| matches!(&e.kind, EntryKind::AssistantMessage { content, .. }
            if content.iter().any(|b| b.as_text() == Some("picking the deploy back up")))));
        assert!(
            matches!(
                entries.iter().rev().find_map(|e| match &e.kind {
                    EntryKind::StepEnd { transition, .. } => Some(transition.clone()),
                    _ => None,
                }),
                Some(Transition::Completed)
            ),
            "the continuation turn completed"
        );
    }

    // A second start (say the process died right after the continuation): nothing runs again.
    let log = open(&path);
    assert!(!close_open_turn(&*log, CancelReason::Reload).unwrap());
    let s = boot(&r, sid, llm.clone(), log.clone(), Arc::new(NeverApproval)).await.unwrap();
    assert_eq!(consume(&s, &[]).await.unwrap(), 0);
    assert_eq!(llm.remaining(), 0);
    let state = s.fold().unwrap();
    assert!(state.turn.is_none() && state.open_calls.is_empty() && state.open_ops.is_empty());
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    fold(&entries, &CustomFolds::new()).unwrap();
    let continuations = entries
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::UserMessage { client_key: Some(k), .. } if k.starts_with("recovery:")))
        .count();
    assert_eq!(continuations, 1);
}
