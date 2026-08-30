//! Log tests (split out for the file-size ratchet, doc 12).

mod core_tests {
    use crate::*;
    use serde_json::json;

    fn call(id: &str) -> CallId {
        CallId(id.into())
    }

    fn turn(log: &dyn LogStore, n: u64, body: impl FnOnce(&dyn LogStore)) {
        log.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(n) }).unwrap();
        body(log);
        log.append(None, Principal::system("loop"), EntryKind::TurnEnd { turn: TurnId(n) }).unwrap();
    }

    fn user(log: &dyn LogStore, t: &str) {
        log.append(None, Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: t.into(), client_key: None }).unwrap();
    }

    fn assistant_calls(log: &dyn LogStore, ids: &[&str]) {
        let content = ids.iter().map(|i| ContentBlock::ToolUse { id: call(i), name: "bash".into(), args: json!({}) }).collect();
        log.append(
            None,
            Principal::system("model"),
            EntryKind::AssistantMessage { content, usage: Usage::default(), stop: StopReason::ToolUse },
        )
        .unwrap();
        for i in ids {
            log.append(
                None,
                Principal::system("model"),
                EntryKind::ToolCall { call: call(i), tool: "bash".into(), args: json!({}), class: Consequence::SpawnsProcess },
            )
            .unwrap();
        }
    }

    fn result(log: &dyn LogStore, id: &str, text: &str) {
        log.append(
            None,
            Principal::system("tool"),
            EntryKind::ToolResult {
                call: call(id),
                outcome: ToolOutcome::Ok,
                value: json!(null),
                content: vec![ContentBlock::text(text)],
                spill_ref: None,
                meta: None,
                late: false,
            },
        )
        .unwrap();
    }

    fn state(log: &dyn LogStore) -> State {
        fold(&log.read_branch(log.head().unwrap()).unwrap(), &CustomFolds::new()).unwrap()
    }

    #[test]
    fn open_call_derives_placeholder_then_late_result() {
        let log = MemoryLog::new();
        turn(&log, 1, |l| {
            user(l, "run tests");
            assistant_calls(l, &["a", "b"]);
            result(l, "a", "ok");
        });
        let msgs = derive_messages(&state(&log));
        let last = msgs.last().unwrap();
        assert!(
            matches!(&last.content[1], ContentBlock::ToolResult { id, content, .. } if id.0 == "b" && content[0].as_text() == Some(TOOL_CALL_RUNNING))
        );
        turn(&log, 2, |l| result(l, "b", "passed"));
        let msgs = derive_messages(&state(&log));
        let tail = msgs.last().unwrap();
        assert!(
            matches!(&tail.content[0], ContentBlock::ToolResult { id, content, .. } if id.0 == "b" && content[0].as_text() == Some("passed"))
        );
    }

    #[test]
    fn second_result_for_one_call_is_an_invariant_failure() {
        let log = MemoryLog::new();
        turn(&log, 1, |l| {
            assistant_calls(l, &["a"]);
            result(l, "a", "1");
            result(l, "a", "2");
        });
        let err = fold(&log.read_branch(log.head().unwrap()).unwrap(), &CustomFolds::new()).unwrap_err();
        assert!(matches!(err, LogError::Invariant { rule: "tool.at_most_one_result", .. }));
    }

    #[test]
    fn approval_outside_a_turn_is_rejected() {
        let log = MemoryLog::new();
        log.append(
            None,
            Principal::system("x"),
            EntryKind::ApprovalRequested { wants: ConsequenceSet::EMPTY, for_cap: "t".into(), call: None, reason: "r".into() },
        )
        .unwrap();
        let err = fold(&log.read_branch(log.head().unwrap()).unwrap(), &CustomFolds::new()).unwrap_err();
        assert!(matches!(err, LogError::Invariant { rule: "approval.inside_turn", .. }));
    }

    #[test]
    fn policy_widening_needs_an_operator_approval() {
        let log = MemoryLog::new();
        let ro = ConsequenceSet::of(&[Consequence::ReadOnly]);
        let rw = ro.with(Consequence::WorkspaceWrite);
        log.append(
            None,
            Principal::system("boot"),
            EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: ro, preset: "read-only".into(), approval_ref: None },
        )
        .unwrap();
        log.append(
            None,
            Principal::system("agent"),
            EntryKind::PolicyEpoch { epoch: Epoch(2), allowed: rw, preset: "custom".into(), approval_ref: None },
        )
        .unwrap();
        assert!(matches!(
            fold(&log.read_branch(log.head().unwrap()).unwrap(), &CustomFolds::new()),
            Err(LogError::Invariant { rule: "policy.widen_requires_operator_approval", .. })
        ));
    }

    #[test]
    fn fork_is_a_pointer_move_and_branches_fold_independently() {
        let log = MemoryLog::new();
        user(&log, "A");
        let a = log.head().unwrap();
        user(&log, "A2");
        log.fork(a).unwrap();
        user(&log, "A1");
        assert_eq!(log.heads().len(), 2);
        let texts: Vec<String> = derive_messages(&state(&log)).iter().map(|m| m.text()).collect();
        assert_eq!(texts, vec!["A", "A1"]);
    }

    #[test]
    fn replace_skips_range_and_inserts_summary_but_never_splits_a_pair() {
        let log = MemoryLog::new();
        turn(&log, 1, |l| {
            user(l, "one");
            assistant_calls(l, &["x"]);
            result(l, "x", "big output");
        });
        let first = log.read_branch(log.head().unwrap()).unwrap();
        let (u, asst) = (first[1].id, first[2].id);
        // splitting the pair is refused
        log.append(
            None,
            Principal::system("compaction"),
            EntryKind::SurfaceReplace { start: u, end: asst, shadow_price: 1, summary: Some("s".into()), by_cap: "c".into() },
        )
        .unwrap();
        assert!(fold(&log.read_branch(log.head().unwrap()).unwrap(), &CustomFolds::new()).is_err());
        log.fork(first.last().unwrap().id).unwrap();
        let end = first.last().unwrap().id;
        log.append(
            None,
            Principal::system("compaction"),
            EntryKind::SurfaceReplace { start: u, end, shadow_price: 1, summary: Some("did one".into()), by_cap: "c".into() },
        )
        .unwrap();
        let msgs = derive_messages(&state(&log));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text().contains("did one"));
    }

    #[test]
    fn empty_output_marker_and_snapshot_dedupe() {
        let log = MemoryLog::new();
        turn(&log, 1, |l| {
            l.append(
                None,
                Principal::system("ctx"),
                EntryKind::UserMessage { form: Form::Snapshot, text: "policy: ro".into(), client_key: None },
            )
            .unwrap();
            l.append(
                None,
                Principal::system("ctx"),
                EntryKind::UserMessage { form: Form::Snapshot, text: "policy: ro".into(), client_key: None },
            )
            .unwrap();
            assistant_calls(l, &["q"]);
            result(l, "q", "  ");
        });
        let msgs = derive_messages(&state(&log));
        assert_eq!(msgs.iter().filter(|m| m.text() == "policy: ro").count(), 1);
        assert!(
            matches!(&msgs.last().unwrap().content[0], ContentBlock::ToolResult { content, .. } if content[0].as_text() == Some("(bash completed with no output)"))
        );
    }

    #[test]
    fn jsonl_round_trips_and_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        {
            let log = JsonlLog::open(&p).unwrap();
            user(&log, "hello");
            let h = log.head().unwrap();
            user(&log, "b1");
            log.fork(h).unwrap();
            user(&log, "b2");
        }
        let log = JsonlLog::open(&p).unwrap();
        assert_eq!(derive_messages(&state(&log)).iter().map(|m| m.text()).collect::<Vec<_>>(), vec!["hello", "b2"]);
        let text = std::fs::read_to_string(&p).unwrap().replace("hello", "HELLO");
        std::fs::write(&p, text).unwrap();
        assert!(matches!(JsonlLog::open(&p), Err(LogError::ChainBroken { .. })));
    }
}

mod wide_id_tests {
    use crate::*;

    #[test]
    fn jsonl_reopens_logs_with_agent_authors_and_subagent_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        {
            let log = JsonlLog::open(&p).unwrap();
            log.append(None, Principal::Agent { id: AgentId(u128::MAX), turn: TurnId(1) }, EntryKind::Note { text: "x".into() }).unwrap();
            log.append(
                None,
                Principal::system("t"),
                EntryKind::SubagentStarted { child: SessionId(1 << 100), provider: "inproc".into(), depth: 1 },
            )
            .unwrap();
        }
        let log = JsonlLog::open(&p).unwrap();
        let b = log.read_branch(log.head().unwrap()).unwrap();
        assert_eq!(b.len(), 2);
        assert!(matches!(b[1].kind, EntryKind::SubagentStarted { child: SessionId(c), .. } if c == 1 << 100));
    }
}
