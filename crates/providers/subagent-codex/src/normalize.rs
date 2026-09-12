//! Codex rollout records → [`Event`]s. Every kind and field name here comes from recorded
//! rollouts of the real binary on this machine (codex-tui 0.153.2 and 0.154.0, Codex Desktop
//! 0.148–0.153; fixtures in `tests/fixtures/records.jsonl`, provenance in
//! `tests/fixtures/PROVENANCE.txt`). A kind no recorded rollout has shown is `Unknown`.
//!
//! A rollout line is `{timestamp, type, payload}` (newer versions add `ordinal`).
//!
//! | record | outcome |
//! |---|---|
//! | `session_meta` | `Session{payload.id, payload.cwd, payload.cli_version}` |
//! | `event_msg/task_started` | `TurnStarted` |
//! | `event_msg/task_complete` | `TurnEnd{EndTurn, last_agent_message}` |
//! | `event_msg/turn_aborted` | `TurnEnd{Aborted}` (`reason: "interrupted"`) |
//! | `event_msg/user_message`, `event_msg/item_completed` with `item.type: UserMessage` | `UserPrompt` (a rollout has one or the other) |
//! | `response_item/message` role `assistant` | `AssistantText` (its `output_text` parts) |
//! | `response_item/function_call` / `custom_tool_call` | `ToolUse{call_id, name, arguments \| input}` |
//! | `response_item/function_call_output` / `custom_tool_call_output` | `ToolResult{call_id, output}` |
//! | `response_item/web_search_call` | `ToolUse{id, "web_search", action}` |
//! | everything else recorded | ignored, each with its reason |

use aho_types::{StopReason, Value};
use foreign_cli::{Event, Normalized, Record, text_of};

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

fn owned(v: &Value, k: &str) -> String {
    s(v, k).unwrap_or("").to_string()
}

/// One rollout line.
pub fn normalize(line: &[u8]) -> Record {
    let v: Value = match serde_json::from_slice(line) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return Record { kind: "?".into(), out: Normalized::Malformed("not a JSON object".into()) },
        Err(e) => return Record { kind: "?".into(), out: Normalized::Malformed(e.to_string()) },
    };
    let ty = s(&v, "type").unwrap_or("").to_string();
    let p = v.get("payload").unwrap_or(&Value::Null);
    match ty.as_str() {
        "session_meta" => events(
            ty,
            vec![Event::Session {
                id: owned(p, "id"),
                cwd: s(p, "cwd").map(str::to_string),
                version: s(p, "cli_version").map(str::to_string),
            }],
        ),
        "event_msg" => event_msg(p),
        "response_item" => response_item(p),
        "turn_context" => ignored(ty, "per-turn settings (model, sandbox, approval policy) Codex records for itself"),
        "world_state" => ignored(ty, "environment snapshot Codex keeps for its own prompt"),
        "token_usage_record" => ignored(ty, "token accounting"),
        "compacted" => ignored(ty, "history compaction checkpoint"),
        _ => unknown(ty),
    }
}

fn event_msg(p: &Value) -> Record {
    let t = s(p, "type").unwrap_or("");
    let kind = format!("event_msg/{t}");
    match t {
        "task_started" => events(kind, vec![Event::TurnStarted]),
        "task_complete" => {
            events(kind, vec![Event::TurnEnd { stop: StopReason::EndTurn, last_message: s(p, "last_agent_message").map(str::to_string) }])
        }
        "turn_aborted" => events(kind, vec![Event::TurnEnd { stop: StopReason::Aborted, last_message: None }]),
        "user_message" => events(kind, vec![Event::UserPrompt]),
        "item_completed" => {
            let item = p.pointer("/item/type").and_then(Value::as_str).unwrap_or("");
            let kind = format!("{kind}/{item}");
            match item {
                "UserMessage" => events(kind, vec![Event::UserPrompt]),
                "AgentMessage" | "CommandExecution" | "Reasoning" | "FileChange" | "McpToolCall" | "WebSearch" | "ImageView"
                | "Extension" | "ContextCompaction" => {
                    ignored(kind, "a UI item that repeats a response_item record (message, tool call, reasoning, compaction)")
                }
                _ => unknown(kind),
            }
        }
        "agent_message" => ignored(kind, "repeats response_item/message (assistant)"),
        "agent_reasoning" => ignored(kind, "reasoning summary, not assistant text"),
        "token_count" => ignored(kind, "token and rate-limit accounting"),
        "thread_settings_applied" => ignored(kind, "settings bookkeeping"),
        "context_compacted" => ignored(kind, "compaction notice"),
        "thread_goal_updated" => ignored(kind, "Codex's own goal bookkeeping"),
        _ => unknown(kind),
    }
}

fn response_item(p: &Value) -> Record {
    let t = s(p, "type").unwrap_or("");
    let kind = format!("response_item/{t}");
    match t {
        "message" => {
            let role = s(p, "role").unwrap_or("");
            let kind = format!("{kind}/{role}");
            match role {
                "assistant" => {
                    let text: String = p
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter(|c| s(c, "type") == Some("output_text"))
                                .filter_map(|c| s(c, "text"))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        return ignored(kind, "an assistant message with no text");
                    }
                    events(kind, vec![Event::AssistantText { text }])
                }
                "user" => ignored(
                    kind,
                    "the prompt as sent to the model (with environment context); delivery is confirmed by the user_message event or UserMessage item",
                ),
                "developer" => ignored(kind, "instructions Codex gives its own model"),
                _ => unknown(kind),
            }
        }
        "function_call" => {
            let raw = owned(p, "arguments");
            let input = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
            events(kind, vec![Event::ToolUse { id: owned(p, "call_id"), name: owned(p, "name"), input }])
        }
        "custom_tool_call" => events(
            kind,
            vec![Event::ToolUse { id: owned(p, "call_id"), name: owned(p, "name"), input: p.get("input").cloned().unwrap_or(Value::Null) }],
        ),
        "function_call_output" | "custom_tool_call_output" => {
            let out = p.get("output").unwrap_or(&Value::Null);
            // Older rollouts wrap the output as `{content, success}`.
            let is_error = out.get("success").and_then(Value::as_bool) == Some(false);
            let content = match out.get("content") {
                Some(c) => text_of(c),
                None => text_of(out),
            };
            events(kind, vec![Event::ToolResult { id: owned(p, "call_id"), is_error, content }])
        }
        "web_search_call" => events(
            kind,
            vec![Event::ToolUse { id: owned(p, "id"), name: "web_search".into(), input: p.get("action").cloned().unwrap_or(Value::Null) }],
        ),
        "reasoning" => ignored(kind, "encrypted reasoning; not assistant text"),
        "compaction" => ignored(kind, "compaction checkpoint"),
        _ => unknown(kind),
    }
}
