//! Claude Code transcript records → [`Event`]s. Every kind and field name here comes from
//! recorded sessions of the real binary on this machine (versions 2.1.121–2.1.234; fixtures in
//! `tests/fixtures/records.jsonl`, provenance in `tests/fixtures/PROVENANCE.txt`). A kind no
//! recorded session has shown is `Unknown`, never guessed.
//!
//! | record | outcome |
//! |---|---|
//! | `user`, string content | `UserPrompt` (delivery confirmation); `isMeta: true` ignored |
//! | `user`, `tool_result` blocks | `ToolResult{tool_use_id, is_error, content}` per block |
//! | `user`, text `[Request interrupted by user…` | `TurnEnd{Aborted}` |
//! | `user`, other text/image blocks | `UserPrompt`; `isMeta: true` (skill expansions) ignored |
//! | `assistant` | `text` → `AssistantText`, `tool_use` → `ToolUse{id, name, input}`; `thinking` (redacted: signature only) and `fallback` (model switch) produce nothing; `isApiErrorMessage` → `Error{error, text}` |
//! | `system/turn_duration` | `TurnEnd{EndTurn}`: the interactive CLI's turn boundary |
//! | `system/api_error` | `Error{retrying}` (Claude Code retries by itself: `retryAttempt`/`maxRetries`) |
//! | `system/model_refusal_fallback` | `Error{retrying: true}` (it retried on `fallbackModel`) |
//! | `attachment/queued_command` | `UserPrompt` (text typed while it worked, taken up now) |
//! | other `attachment/*`, `system/*`, metadata kinds | ignored, each with its reason |
//! | any record with `isSidechain: true` | ignored (a Claude Code sub-agent's own step) |

use aho_types::{StopReason, Value};
use foreign_cli::{Event, Normalized, Record, text_of};

const INTERRUPT_MARK: &str = "[Request interrupted by user";

/// `attachment.type` values recorded so far that are context Claude Code adds to its own prompt.
const CONTEXT_ATTACHMENTS: &[&str] = &[
    "agent_listing_delta",
    "command_permissions",
    "date_change",
    "deferred_tools_delta",
    "directory",
    "edited_text_file",
    "file",
    "hook_additional_context",
    "mcp_instructions_delta",
    "plan_mode",
    "plan_mode_exit",
    "skill_listing",
    "task_reminder",
    "todo_reminder",
];

fn ignored(kind: String, why: &'static str) -> Record {
    Record { kind, out: Normalized::Ignored(why) }
}

fn events(kind: String, ev: Vec<Event>) -> Record {
    Record { kind, out: Normalized::Events(ev) }
}

fn unknown(kind: String) -> Record {
    Record { kind, out: Normalized::Unknown }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// One transcript line.
pub fn normalize(line: &[u8]) -> Record {
    let v: Value = match serde_json::from_slice(line) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return Record { kind: "?".into(), out: Normalized::Malformed("not a JSON object".into()) },
        Err(e) => return Record { kind: "?".into(), out: Normalized::Malformed(e.to_string()) },
    };
    let ty = s(&v, "type").unwrap_or("").to_string();
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return ignored(ty, "a step of one of Claude Code's own sub-agents (sidechain), not this conversation");
    }
    match ty.as_str() {
        "user" => user(&v),
        "assistant" => assistant(&v),
        "system" => system(&v),
        "attachment" => attachment(&v),
        "queue-operation" => ignored(
            format!("queue-operation/{}", s(&v, "operation").unwrap_or("")),
            "input-queue bookkeeping; a queued prompt is confirmed by its queued_command attachment or user record",
        ),
        "last-prompt" => ignored(ty, "tail metadata re-appended for session listings"),
        "ai-title" => ignored(ty, "the session title Claude Code generated"),
        "mode" => ignored(ty, "UI mode marker"),
        "permission-mode" => ignored(ty, "permission-mode marker; aho chose the mode at launch"),
        "file-history-snapshot" => ignored(ty, "Claude Code's own file checkpoint bookkeeping"),
        "file-history-delta" => ignored(ty, "Claude Code's own file backup bookkeeping"),
        _ => unknown(ty),
    }
}

fn user(v: &Value) -> Record {
    let meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
    match v.pointer("/message/content") {
        Some(Value::String(_)) if meta => {
            ignored("user/meta".into(), "a message Claude Code injected itself (caveats, command output), not a prompt")
        }
        Some(Value::String(_)) => events("user/prompt".into(), vec![Event::UserPrompt]),
        Some(Value::Array(blocks)) => {
            let mut out = Vec::new();
            let mut prompt = false;
            for b in blocks {
                match s(b, "type") {
                    Some("tool_result") => out.push(Event::ToolResult {
                        id: s(b, "tool_use_id").unwrap_or("").to_string(),
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        content: text_of(b.get("content").unwrap_or(&Value::Null)),
                    }),
                    Some("text") if s(b, "text").is_some_and(|t| t.starts_with(INTERRUPT_MARK)) => {
                        out.push(Event::TurnEnd { stop: StopReason::Aborted, last_message: None });
                    }
                    Some("text") | Some("image") => prompt = true,
                    other => return unknown(format!("user/{}", other.unwrap_or("?"))),
                }
            }
            if prompt && out.is_empty() {
                if meta {
                    return ignored(
                        "user/meta".into(),
                        "text Claude Code expanded into its own prompt (skills, commands), not a user prompt",
                    );
                }
                out.push(Event::UserPrompt);
            }
            let kind = if out.iter().any(|e| matches!(e, Event::TurnEnd { .. })) {
                "user/interrupt"
            } else if prompt {
                "user/prompt"
            } else {
                "user/tool_result"
            };
            events(kind.into(), out)
        }
        _ => unknown("user/?".into()),
    }
}

fn assistant(v: &Value) -> Record {
    let blocks = match v.pointer("/message/content") {
        Some(Value::Array(b)) => b,
        _ => return unknown("assistant/?".into()),
    };
    let text: String = blocks.iter().filter(|b| s(b, "type") == Some("text")).filter_map(|b| s(b, "text")).collect::<Vec<_>>().join("\n");
    if v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
        let code = s(v, "error").unwrap_or("error");
        return events("assistant/api-error".into(), vec![Event::Error { message: format!("{code}: {text}"), retrying: false }]);
    }
    let mut out = Vec::new();
    let mut kinds = Vec::new();
    for b in blocks {
        let t = s(b, "type").unwrap_or("?");
        kinds.push(t);
        match t {
            "text" => {
                if let Some(x) = s(b, "text").filter(|x| !x.trim().is_empty()) {
                    out.push(Event::AssistantText { text: x.to_string() });
                }
            }
            "tool_use" => out.push(Event::ToolUse {
                id: s(b, "id").unwrap_or("").to_string(),
                name: s(b, "name").unwrap_or("").to_string(),
                input: b.get("input").cloned().unwrap_or(Value::Null),
            }),
            "thinking" | "fallback" => {}
            _ => return unknown(format!("assistant/{t}")),
        }
    }
    let kind = format!("assistant/{}", kinds.join(","));
    if out.is_empty() {
        return ignored(kind, "only a redacted thinking block (a signature) or a model-fallback marker; no visible text or tool call");
    }
    events(kind, out)
}

fn system(v: &Value) -> Record {
    let sub = s(v, "subtype").unwrap_or("");
    let kind = format!("system/{sub}");
    match sub {
        "turn_duration" => events(kind, vec![Event::TurnEnd { stop: StopReason::EndTurn, last_message: None }]),
        "api_error" => {
            let e = v.get("error").unwrap_or(&Value::Null);
            let detail = e
                .pointer("/error/error/message")
                .and_then(Value::as_str)
                .or_else(|| s(e, "type"))
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string());
            let status = e.get("status").and_then(Value::as_u64).map(|x| format!("HTTP {x}: ")).unwrap_or_default();
            let attempt = v.get("retryAttempt").and_then(Value::as_u64).unwrap_or(0);
            let max = v.get("maxRetries").and_then(Value::as_u64).unwrap_or(0);
            let retrying = v.get("retryInMs").is_some() && attempt < max;
            events(kind, vec![Event::Error { message: format!("{status}{detail} (attempt {attempt} of {max})"), retrying }])
        }
        "model_refusal_fallback" => {
            let msg = format!(
                "{} declined; Claude Code retried on {}",
                s(v, "originalModel").unwrap_or("the model"),
                s(v, "fallbackModel").unwrap_or("a fallback model")
            );
            events(kind, vec![Event::Error { message: msg, retrying: true }])
        }
        "stop_hook_summary" => ignored(kind, "Stop-hook bookkeeping; the turn boundary is system/turn_duration"),
        "local_command" => ignored(kind, "output of a slash command entered in the pane"),
        "model_consent_fallback" => ignored(kind, "notice that Claude Code switched models for the session; the turn continues"),
        _ => unknown(kind),
    }
}

fn attachment(v: &Value) -> Record {
    let at = v.pointer("/attachment/type").and_then(Value::as_str).unwrap_or("");
    let kind = format!("attachment/{at}");
    if at == "queued_command" {
        return events(kind, vec![Event::UserPrompt]);
    }
    if CONTEXT_ATTACHMENTS.contains(&at) {
        return ignored(kind, "context Claude Code attaches to its own prompt (files, listings, reminders), not a conversational event");
    }
    unknown(kind)
}
