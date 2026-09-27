//! The normalizer over records recorded from the real Codex binary (scrubbed; see
//! `fixtures/PROVENANCE.txt`). Never hand-written samples.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use aho_types::StopReason;
use foreign_cli::{Event, Normalized, Record};
use subagent_codex::normalize::normalize;

fn fixture(name: &str) -> Vec<Vec<u8>> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(p).unwrap().split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect()
}

fn by_kind() -> BTreeMap<String, Vec<Record>> {
    let mut m: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for l in fixture("records.jsonl") {
        let r = normalize(&l);
        m.entry(r.kind.clone()).or_default().push(r);
    }
    m
}

#[test]
fn every_recorded_kind_is_mapped_or_ignored_with_a_reason() {
    let lines = fixture("records.jsonl");
    assert!(lines.len() >= 35, "the fixture holds one record per recorded kind");
    for l in &lines {
        let r = normalize(l);
        match &r.out {
            Normalized::Events(e) => assert!(!e.is_empty(), "{} maps to nothing without saying why", r.kind),
            Normalized::Ignored(why) => assert!(why.len() > 10, "{}: the reason must say something", r.kind),
            Normalized::Unknown | Normalized::Malformed(_) => panic!("recorded kind `{}` is not handled: {:?}", r.kind, r.out),
        }
    }
}

#[test]
fn the_mapping_table_holds_for_each_recorded_kind() {
    let m = by_kind();
    let ev = |k: &str| {
        m.get(k)
            .unwrap_or_else(|| panic!("no fixture of kind {k}; have {:?}", m.keys()))
            .iter()
            .flat_map(|r| r.events().to_vec())
            .collect::<Vec<_>>()
    };
    let ign = |k: &str| m[k].iter().all(|r| matches!(r.out, Normalized::Ignored(_)));
    let assistant = ev("response_item/message/assistant");
    assert_eq!(assistant.len(), 3, "no phase (older), commentary, final_answer");
    assert!(assistant.iter().all(|e| matches!(e, Event::AssistantText { text } if !text.is_empty())));

    let metas = ev("session_meta");
    assert_eq!(metas.len(), 3, "cli, vscode, and a sub-agent thread");
    assert!(metas.iter().any(|e| matches!(e, Event::Session { version: Some(v), cwd: Some(_), .. } if v == "0.154.0")));
    assert_eq!(ev("event_msg/task_started"), vec![Event::TurnStarted]);
    assert!(matches!(&ev("event_msg/task_complete")[..], [Event::TurnEnd { stop: StopReason::EndTurn, last_message: Some(_) }]));
    assert_eq!(ev("event_msg/turn_aborted"), vec![Event::TurnEnd { stop: StopReason::Aborted, last_message: None }]);
    assert_eq!(ev("event_msg/user_message"), vec![Event::UserPrompt]);
    assert_eq!(ev("event_msg/item_completed/UserMessage"), vec![Event::UserPrompt]);

    assert!(
        matches!(&ev("response_item/function_call")[..], [Event::ToolUse { id, name, .. }] if id.starts_with("call_") && !name.is_empty())
    );
    assert!(matches!(&ev("response_item/custom_tool_call")[..], [Event::ToolUse { input, .. }] if input.is_string()));
    assert!(
        matches!(&ev("response_item/function_call_output")[..], [Event::ToolResult { id, is_error: false, .. }] if id.starts_with("call_"))
    );
    assert!(matches!(&ev("response_item/custom_tool_call_output")[..], [Event::ToolResult { .. }]));
    assert!(matches!(&ev("response_item/web_search_call")[..], [Event::ToolUse { name, .. }] if name == "web_search"));

    for k in [
        "response_item/message/user",
        "response_item/message/developer",
        "response_item/reasoning",
        "response_item/compaction",
        "event_msg/agent_message",
        "event_msg/agent_reasoning",
        "event_msg/token_count",
        "event_msg/thread_settings_applied",
        "event_msg/context_compacted",
        "event_msg/thread_goal_updated",
        "event_msg/item_completed/AgentMessage",
        "event_msg/item_completed/CommandExecution",
        "turn_context",
        "world_state",
        "token_usage_record",
        "compacted",
    ] {
        assert!(ign(k), "{k} should be ignored");
    }
}

#[test]
fn a_recorded_turn_reads_as_start_prompt_tool_call_result_text_end() {
    let evs: Vec<Event> = fixture("turn1.jsonl").iter().flat_map(|l| normalize(l).events().to_vec()).collect();
    let shape: Vec<&str> = evs
        .iter()
        .map(|e| match e {
            Event::TurnStarted => "start",
            Event::UserPrompt => "prompt",
            Event::ToolUse { .. } => "tool_use",
            Event::ToolResult { .. } => "tool_result",
            Event::AssistantText { .. } => "text",
            Event::TurnEnd { .. } => "end",
            _ => "other",
        })
        .collect();
    assert_eq!(shape, vec!["start", "prompt", "text", "tool_use", "tool_result", "text", "end"]);
    let aborted: Vec<Event> = fixture("aborted.jsonl").iter().flat_map(|l| normalize(l).events().to_vec()).collect();
    assert_eq!(aborted.last(), Some(&Event::TurnEnd { stop: StopReason::Aborted, last_message: None }));
}

#[test]
fn unrecorded_kinds_and_garbage_are_surfaced_not_guessed() {
    assert_eq!(normalize(br#"{"type":"event_msg","payload":{"type":"stream_error","message":"x"}}"#).out, Normalized::Unknown);
    assert_eq!(normalize(br#"{"type":"response_item","payload":{"type":"local_shell_call"}}"#).out, Normalized::Unknown);
    assert_eq!(
        normalize(br#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Hologram"}}}"#).out,
        Normalized::Unknown
    );
    assert_eq!(normalize(br#"{"type":"brand_new","payload":{}}"#).out, Normalized::Unknown);
    assert!(matches!(normalize(b"not json").out, Normalized::Malformed(_)));
}
