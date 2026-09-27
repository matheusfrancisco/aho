use std::borrow::Cow;

use aho_llm::{Request, joined_text};
use aho_types::{ContentBlock, Message, Role, Value};
use serde_json::{Map, json};

use crate::PROTOCOL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Generic,
    Codex,
    Copilot,
}

/// Encode a stateless Responses request. The route/model stamp keeps opaque reasoning and
/// output-item identities from being replayed to a different provider or model.
pub fn encode_request(req: &Request, flavor: Flavor, route: &str) -> Value {
    let mut input = Vec::with_capacity(req.messages.len() + 1);
    let mut instructions =
        req.system.iter().map(|(_, text, _)| text.as_str()).filter(|text| !text.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
    if flavor == Flavor::Codex {
        for message in req.messages.iter().filter(|message| message.role == Role::System) {
            let text = joined_text(&message.content);
            if !text.is_empty() {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(&text);
            }
        }
    } else if !instructions.is_empty() {
        input.push(json!({"role": "system", "content": instructions}));
    }
    for message in &req.messages {
        if flavor == Flavor::Codex && message.role == Role::System {
            continue;
        }
        encode_message(message, &mut input, route, &req.model);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if let Some(key) = &req.cache_key {
        body.insert("prompt_cache_key".into(), json!(key.chars().take(64).collect::<String>()));
    }
    if flavor == Flavor::Codex {
        body.insert("instructions".into(), json!(if instructions.is_empty() { "You are a helpful assistant." } else { &instructions }));
        body.insert("text".into(), json!({"verbosity": "low"}));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        body.insert("tool_choice".into(), json!("auto"));
        body.insert("parallel_tool_calls".into(), json!(true));
        if req.reasoning.is_some() {
            body.insert("reasoning".into(), json!({"effort": "medium", "summary": "auto"}));
        }
        // Codex does not accept max_output_tokens; pi's Codex endpoint deliberately omits it.
    } else {
        if req.max_tokens > 0 {
            body.insert("max_output_tokens".into(), json!(req.max_tokens.max(16)));
        }
        if route == "xai" {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|tool| {
                        let mut encoded = json!({
                            "type": "function", "name": tool.name, "description": tool.description,
                            "parameters": tool.input_schema.0,
                        });
                        if flavor == Flavor::Codex {
                            encoded["strict"] = Value::Null;
                        }
                        encoded
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(body)
}

fn encode_message(message: &Message, out: &mut Vec<Value>, route: &str, model: &str) {
    match message.role {
        Role::System => {
            let text = joined_text(&message.content);
            if !text.is_empty() {
                out.push(json!({"role": "system", "content": text}));
            }
        }
        Role::Assistant => {
            if let Some(native) = message.content.iter().find_map(|block| match block {
                ContentBlock::ProviderMetadata { protocol, data }
                    if protocol == PROTOCOL.tag() && data["route"] == route && data["model"] == model =>
                {
                    data["output"].as_array()
                }
                _ => None,
            }) {
                out.extend(native.iter().cloned());
                return;
            }
            // Unsigned/foreign thinking is never presented as visible assistant text. Without
            // its paired reasoning item, omit the function-call item id (not the call_id).
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } if !text.is_empty() => out.push(json!({
                        "type": "message", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                    })),
                    ContentBlock::ToolUse { id, name, args } => out.push(json!({
                        "type": "function_call", "call_id": call_id(&id.0), "name": name,
                        "arguments": match args { Value::String(raw) => raw.clone(), other => other.to_string() },
                    })),
                    _ => {}
                }
            }
        }
        Role::User | Role::Tool => {
            let mut parts = Vec::new();
            for block in &message.content {
                if let ContentBlock::ToolResult { id, content, is_error } = block {
                    // A late result is another output for the same call_id, not a new call.
                    let text = joined_text(content);
                    let text = if *is_error { format!("[tool error]\n{text}") } else { text };
                    let output = if content.iter().any(|part| matches!(part, ContentBlock::Image { .. })) {
                        let mut output = Vec::new();
                        if !text.is_empty() {
                            output.push(json!({"type": "input_text", "text": text}));
                        }
                        output.extend(content.iter().filter_map(|part| match part {
                            ContentBlock::Image { .. } => user_part(part),
                            _ => None,
                        }));
                        Value::Array(output)
                    } else {
                        json!(if text.is_empty() { "(no tool output)" } else { &text })
                    };
                    out.push(json!({"type": "function_call_output", "call_id": call_id(&id.0), "output": output}));
                } else if let Some(part) = user_part(block) {
                    parts.push(part);
                }
            }
            if !parts.is_empty() {
                out.push(json!({"role": "user", "content": parts}));
            }
        }
    }
}

fn user_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } if !text.is_empty() => Some(json!({"type": "input_text", "text": text})),
        ContentBlock::Image { media_type, data } => Some(json!({
            "type": "input_image", "detail": "auto",
            "image_url": format!("data:{media_type};base64,{}", aho_llm::base64(data)),
        })),
        _ => None,
    }
}

fn call_id(id: &str) -> Cow<'_, str> {
    let id = id.split_once('|').map_or(id, |(call, _)| call);
    if id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
        Cow::Borrowed(id)
    } else {
        Cow::Owned(id.chars().take(64).map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' }).collect())
    }
}
