//! The normalizer over records recorded from the real Claude Code binary (scrubbed; see
//! `fixtures/PROVENANCE.txt`). Never hand-written samples (12).
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use aho_types::StopReason;
use foreign_cli::{Event, Normalized, Record};
use subagent_claude::normalize::normalize;

fn fixture(name: &str) -> Vec<Vec<u8>> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(p).unwrap().split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect()
}

fn all() -> BTreeMap<String, Record> {
    fixture("records.jsonl").iter().map(|l| normalize(l)).map(|r| (r.kind.clone(), r)).collect()
}

#[test]
fn every_recorded_kind_is_mapped_or_ignored_with_a_reason() {
    let lines = fixture("records.jsonl");
    assert!(lines.len() >= 40, "the fixture holds one record per recorded kind");
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
    let r = all();
    let ev = |k: &str| r.get(k).unwrap_or_else(|| panic!("no fixture record of kind {k}; have {:?}", r.keys())).events().to_vec();
    let ign = |k: &str| matches!(r[k].out, Normalized::Ignored(_));

    assert_eq!(ev("user/prompt"), vec![Event::UserPrompt]);
    assert!(ign("user/meta"));
    assert_eq!(ev("user/interrupt"), vec![Event::TurnEnd { stop: StopReason::Aborted, last_message: None }]);
    // Tool results: ok, a failed command, a denied permission (`is_error: true`), list-shaped content.
    let results: Vec<(String, bool, String)> = fixture("records.jsonl")
        .iter()
        .flat_map(|l| normalize(l).events().to_vec())
        .filter_map(|e| if let Event::ToolResult { id, is_error, content } = e { Some((id, is_error, content)) } else { None })
        .collect();
    assert_eq!(results.len(), 4, "{results:?}");
    assert_eq!(results.iter().filter(|(_, e, _)| *e).count(), 2, "{results:?}");
    assert!(results.iter().all(|(id, _, content)| id.starts_with("toolu_") && !content.is_empty()), "{results:?}");
    assert!(ev("user/tool_result").len() == 1);

    let tool = ev("assistant/tool_use");
    assert!(matches!(&tool[..], [Event::ToolUse { name, input, .. }] if name == "Bash" && input.get("command").is_some()));
    assert!(matches!(&ev("assistant/text")[..], [Event::AssistantText { .. }]));
    assert!(
        matches!(&ev("assistant/thinking,text,tool_use")[..], [Event::AssistantText { .. }, Event::ToolUse { name, .. }] if name == "TodoWrite")
    );
    assert!(ign("assistant/thinking"), "a redacted thinking block alone is ignored with a reason");
    assert!(ign("assistant/fallback"));
    assert!(
        matches!(&ev("assistant/api-error")[..], [Event::Error { message, retrying: false }] if message.starts_with("authentication_failed"))
    );

    assert_eq!(ev("system/turn_duration"), vec![Event::TurnEnd { stop: StopReason::EndTurn, last_message: None }]);
    assert!(matches!(&ev("system/api_error")[..], [Event::Error { message, retrying: true }] if message.starts_with("HTTP 401")));
    assert!(matches!(&ev("system/model_refusal_fallback")[..], [Event::Error { retrying: true, .. }]));
    for k in [
        "system/stop_hook_summary",
        "system/local_command",
        "system/model_consent_fallback",
        "queue-operation/enqueue",
        "queue-operation/dequeue",
        "queue-operation/remove",
        "last-prompt",
        "ai-title",
        "mode",
        "permission-mode",
        "file-history-snapshot",
        "file-history-delta",
        "attachment/skill_listing",
        "attachment/file",
    ] {
        assert!(ign(k), "{k} should be ignored");
    }
}

#[test]
fn a_recorded_turn_reads_as_prompt_tool_call_result_text_end() {
    let evs: Vec<Event> = fixture("turn1.jsonl").iter().flat_map(|l| normalize(l).events().to_vec()).collect();
    let shape: Vec<&str> = evs
        .iter()
        .map(|e| match e {
            Event::UserPrompt => "prompt",
            Event::ToolUse { .. } => "tool_use",
            Event::ToolResult { .. } => "tool_result",
            Event::AssistantText { .. } => "text",
            Event::TurnEnd { .. } => "end",
            _ => "other",
        })
        .collect();
    assert_eq!(shape, vec!["prompt", "tool_use", "tool_result", "text", "end"]);
    let (Event::ToolUse { id: a, .. }, Event::ToolResult { id: b, .. }) = (&evs[1], &evs[2]) else { panic!() };
    assert_eq!(a, b, "the result answers the call");
}

#[test]
fn unrecorded_kinds_and_garbage_are_surfaced_not_guessed() {
    assert_eq!(normalize(br#"{"type":"summary","summary":"x"}"#).out, Normalized::Unknown);
    assert_eq!(normalize(br#"{"type":"system","subtype":"compact_boundary"}"#).out, Normalized::Unknown);
    assert_eq!(normalize(br#"{"type":"attachment","attachment":{"type":"brand_new"}}"#).out, Normalized::Unknown);
    assert_eq!(normalize(br#"{"type":"assistant","message":{"content":[{"type":"server_tool_use"}]}}"#).kind, "assistant/server_tool_use");
    assert!(matches!(normalize(b"{not json").out, Normalized::Malformed(_)));
    assert!(matches!(normalize(b"[1,2]").out, Normalized::Malformed(_)));
    // Sidechain steps (Claude Code's own sub-agents) never count as this conversation.
    let side = normalize(br#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"x"}]}}"#);
    assert!(matches!(side.out, Normalized::Ignored(_)));
}
