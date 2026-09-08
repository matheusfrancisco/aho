//! pi's normalized TranscriptContext carries system instructions and tools in system messages.

use std::collections::BTreeMap;

use aho_llm::{Request, normalize_late_results};
use aho_types::{ContentBlock, Message, Role, Value};
use serde_json::json;

use crate::{LATE_RESULTS, PROTOCOL, ROUTE};

pub(crate) fn empty_usage() -> Value {
    json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
    })
}

/// Encode `POST <base_url>/messages`. No chat-completions or Messages API fields are used.
/// pi owns prompt caching; the seam's stable session key becomes `options.sessionId`.
pub fn encode_request(req: &Request) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() || !req.tools.is_empty() {
        let system: Vec<Value> = req.system.iter().map(|(_, text, _)| json!({"type": "text", "text": text})).collect();
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name, "description": tool.description, "parameters": tool.input_schema.0
                })
            })
            .collect();
        messages.push(json!({"role": "system", "content": system, "toolsAdded": tools, "timestamp": 0}));
    }
    let normalized = normalize_late_results(&req.messages, LATE_RESULTS);
    let mut tool_names = BTreeMap::new();
    for message in &normalized {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id, name.as_str());
            }
        }
        if message.role == Role::Assistant {
            messages.push(assistant(message, &req.model));
            continue;
        }
        let role = if message.role == Role::System { "system" } else { "user" };
        let mut content = Vec::new();
        for block in &message.content {
            if let ContentBlock::ToolResult { id, content: result, is_error } = block {
                // Mixed user/result blocks become separate messages without changing their order.
                flush_message(&mut messages, role, &mut content);
                let result: Vec<Value> = result.iter().filter_map(user_block).collect();
                messages.push(json!({
                    "role": "toolResult", "toolCallId": id.0,
                    "toolName": tool_names.get(id).copied().unwrap_or(""),
                    "content": result, "isError": is_error, "timestamp": 0
                }));
            } else if let Some(block) = user_block(block) {
                content.push(block);
            }
        }
        flush_message(&mut messages, role, &mut content);
    }
    let mut options = json!({});
    if req.max_tokens > 0 {
        options["maxTokens"] = json!(req.max_tokens);
    }
    if let Some(key) = &req.cache_key {
        options["sessionId"] = json!(key);
    }
    json!({"model": req.model, "context": {"messages": messages}, "options": options})
}

fn flush_message(messages: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        messages.push(json!({"role": role, "content": std::mem::take(content), "timestamp": 0}));
    }
}

fn user_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { media_type, data } => Some(json!({"type": "image", "data": aho_llm::base64(data), "mimeType": media_type})),
        _ => None,
    }
}

fn assistant(message: &Message, model: &str) -> Value {
    let metadata = message.content.iter().find_map(|block| match block {
        ContentBlock::ProviderMetadata { protocol, data } if protocol == PROTOCOL.tag() => Some(data),
        _ => None,
    });
    let mut content = Vec::new();
    for block in &message.content {
        let mut encoded = match block {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::Thinking { text } => json!({"type": "thinking", "thinking": text}),
            ContentBlock::ToolUse { id, name, args } => json!({"type": "toolCall", "id": id.0, "name": name, "arguments": args}),
            _ => continue,
        };
        if let Some(extra) = metadata.and_then(|data| data["blocks"].get(content.len())).and_then(Value::as_object) {
            for key in ["textSignature", "thinkingSignature", "redacted", "thoughtSignature", "namespace"] {
                if let Some(value) = extra.get(key) {
                    encoded[key] = value.clone();
                }
            }
        }
        content.push(encoded);
    }
    let stop = if content.iter().any(|b| b["type"] == "toolCall") { "toolUse" } else { "stop" };
    let mut out = json!({
        "role": "assistant", "api": PROTOCOL.tag(), "provider": ROUTE, "model": model,
        "stopReason": stop, "timestamp": 0, "usage": empty_usage(), "content": content
    });
    if let Some(extra) = metadata.and_then(|data| data["message"].as_object()) {
        for key in ["api", "provider", "model", "responseId", "providerThinkingLevel", "diagnostics", "usage", "stopReason", "timestamp"] {
            if let Some(value) = extra.get(key) {
                out[key] = value.clone();
            }
        }
    }
    out
}
