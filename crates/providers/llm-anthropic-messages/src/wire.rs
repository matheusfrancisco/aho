//! Messages API wire format: request encoding, streamed-event decoding, error mapping.
//! Pure (no I/O) so every mapping is testable against recorded bodies.

use std::collections::BTreeMap;

use aho_llm::{LateResults, LlmError, Request, Response, Sink, StreamEvent, normalize_late_results};
use aho_types::{CallId, ContentBlock, Role, StopReason, Usage, Value};
use regex::Regex;
use serde_json::{Map, json};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The API requires `max_tokens`; used when the request leaves it at 0 (the core default).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// The API accepts at most four `cache_control` breakpoints per request.
const MAX_BREAKPOINTS: usize = 4;

/// The JSON body for `POST {base_url}/v1/messages`.
///
/// * System sections become text blocks; a section with `cache_break` carries
///   `cache_control: {type: ephemeral}` (the last four, if more are marked).
/// * `tool_result` blocks must directly follow their `tool_use`, so late results are first
///   rewritten by `normalize_late_results(.., AsUserBlock)`; consecutive same-role messages are
///   merged and `tool_result` blocks lead their user message.
/// * Thinking blocks are not sent back: without their signature the API rejects them.
pub fn encode_request(req: &Request) -> Value {
    encode_authenticated_request(req, false)
}

pub(crate) fn encode_authenticated_request(req: &Request, claude_oauth: bool) -> Value {
    let mut system: Vec<Value> = Vec::new();
    let mut breakpoints: Vec<usize> = Vec::new();
    if claude_oauth {
        system.push(json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}));
    }
    for (_, text, cache_break) in &req.system {
        if !text.trim().is_empty() {
            system.push(json!({"type": "text", "text": text}));
        }
        // an empty marked section moves its breakpoint onto the last block before it
        if *cache_break && !system.is_empty() && breakpoints.last() != Some(&(system.len() - 1)) {
            breakpoints.push(system.len() - 1);
        }
    }
    let skip = breakpoints.len().saturating_sub(MAX_BREAKPOINTS);
    for &i in &breakpoints[skip..] {
        system[i]["cache_control"] = json!({"type": "ephemeral"});
    }

    let normalized = normalize_late_results(&req.messages, LateResults::AsUserBlock);
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for m in &normalized {
        let role = if m.role == Role::Assistant { "assistant" } else { "user" };
        let blocks: Vec<Value> = m.content.iter().filter_map(|b| block(b, claude_oauth)).collect();
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((r, prev)) if *r == role => prev.extend(blocks),
            _ => messages.push((role, blocks)),
        }
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, mut blocks)| {
            if role == "user" {
                // stable: results first, in their order; then everything else in its order
                blocks.sort_by_key(|b| b["type"] != "tool_result");
            }
            json!({"role": role, "content": blocks})
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(if req.max_tokens == 0 { DEFAULT_MAX_TOKENS } else { req.max_tokens }));
    body.insert("stream".into(), json!(true));
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let name = if claude_oauth { claude_tool_name(&t.name) } else { &t.name };
                json!({"name": name, "description": t.description, "input_schema": t.input_schema.0})
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

fn claude_tool_name(name: &str) -> &str {
    const NAMES: &[&str] = &[
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ];
    NAMES.iter().copied().find(|canonical| canonical.eq_ignore_ascii_case(name)).unwrap_or(name)
}

fn block(b: &ContentBlock, claude_oauth: bool) -> Option<Value> {
    match b {
        ContentBlock::Text { text } if !text.is_empty() => Some(json!({"type": "text", "text": text})),
        ContentBlock::Text { .. } | ContentBlock::Thinking { .. } | ContentBlock::ProviderMetadata { .. } => None,
        ContentBlock::Image { media_type, data } => {
            Some(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": aho_llm::base64(data)}}))
        }
        ContentBlock::ToolUse { id, name, args } => {
            let name = if claude_oauth { claude_tool_name(name) } else { name };
            let input = match args {
                Value::Object(_) => args.clone(),
                Value::Null => json!({}),
                // `input` must be an object; unparseable model bytes are kept, wrapped
                other => json!({"_args": other}),
            };
            Some(json!({"type": "tool_use", "id": id.0, "name": name, "input": input}))
        }
        ContentBlock::ToolResult { id, content, is_error } => {
            let inner: Vec<Value> = content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text { .. } | ContentBlock::Image { .. } => block(c, claude_oauth),
                    _ => None,
                })
                .collect();
            let mut r = json!({"type": "tool_result", "tool_use_id": id.0, "content": inner});
            if *is_error {
                r["is_error"] = json!(true);
            }
            Some(r)
        }
    }
}

#[derive(Debug)]
enum Block {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
        args: Option<Value>,
    },
    /// Server tools, redacted thinking: not part of the seam's vocabulary.
    Other,
}

/// Folds Messages streaming events into live events and the final [`Response`].
#[derive(Debug, Default)]
pub struct StreamDecoder {
    route: String,
    blocks: BTreeMap<u64, Block>,
    usage: Usage,
    stop: Option<String>,
    stopped: bool,
}

impl StreamDecoder {
    pub fn new(route: &str) -> StreamDecoder {
        StreamDecoder { route: route.to_string(), ..StreamDecoder::default() }
    }

    /// True once `message_stop` arrived.
    pub fn is_done(&self) -> bool {
        self.stopped
    }

    /// One SSE event (`event:` name, `data:` payload).
    pub fn on_event(&mut self, event: Option<&str>, data: &str, sink: &mut Sink<'_>) -> Result<(), LlmError> {
        self.on_authenticated_event(event, data, sink, &[])
    }

    pub(crate) fn on_authenticated_event(
        &mut self,
        event: Option<&str>,
        data: &str,
        sink: &mut Sink<'_>,
        oauth_tools: &[aho_types::ToolSpec],
    ) -> Result<(), LlmError> {
        if data.trim().is_empty() {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::Transient(format!("route `{}` streamed an event that is not JSON ({e}): {}", self.route, snippet(data)))
        })?;
        let kind = v["type"].as_str().or(event).unwrap_or("");
        let index = v["index"].as_u64().unwrap_or(0);
        match kind {
            "message_start" => self.merge_usage(&v["message"]["usage"]),
            "content_block_start" => {
                let cb = &v["content_block"];
                let b = match cb["type"].as_str() {
                    Some("text") => {
                        let t = cb["text"].as_str().unwrap_or("").to_string();
                        if !t.is_empty() {
                            sink(StreamEvent::Text(t.clone()));
                        }
                        Block::Text(t)
                    }
                    Some("thinking") => {
                        let t = cb["thinking"].as_str().unwrap_or("").to_string();
                        if !t.is_empty() {
                            sink(StreamEvent::Thinking(t.clone()));
                        }
                        Block::Thinking(t)
                    }
                    Some("tool_use") => {
                        let id = cb["id"].as_str().unwrap_or("").to_string();
                        let wire_name = cb["name"].as_str().unwrap_or("");
                        let name = oauth_tools
                            .iter()
                            .find(|tool| tool.name.eq_ignore_ascii_case(wire_name))
                            .map(|tool| tool.name.as_str())
                            .unwrap_or(wire_name)
                            .to_string();
                        sink(StreamEvent::ToolUseStart { id: CallId(id.clone()), name: name.clone() });
                        Block::ToolUse { id, name, json: String::new(), args: None }
                    }
                    _ => Block::Other,
                };
                self.blocks.insert(index, b);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match (self.blocks.get_mut(&index), d["type"].as_str()) {
                    (Some(Block::Text(t)), Some("text_delta")) => {
                        let s = d["text"].as_str().unwrap_or("");
                        t.push_str(s);
                        sink(StreamEvent::Text(s.to_string()));
                    }
                    (Some(Block::Thinking(t)), Some("thinking_delta")) => {
                        let s = d["thinking"].as_str().unwrap_or("");
                        t.push_str(s);
                        sink(StreamEvent::Thinking(s.to_string()));
                    }
                    (Some(Block::ToolUse { json, .. }), Some("input_json_delta")) => {
                        json.push_str(d["partial_json"].as_str().unwrap_or(""))
                    }
                    // signature_delta, citations_delta, deltas for blocks we do not model
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(Block::ToolUse { id, name, json, args }) = self.blocks.get_mut(&index) {
                    let parsed = parse_args(json);
                    sink(StreamEvent::ToolUseEnd { id: CallId(id.clone()), name: name.clone(), args: parsed.clone() });
                    *args = Some(parsed);
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(r.to_string());
                }
                self.merge_usage(&v["usage"]);
                sink(StreamEvent::Usage(self.usage));
            }
            "message_stop" => self.stopped = true,
            "error" => return Err(classify(None, None, data, false, &self.route)),
            _ => {} // ping and future event types
        }
        Ok(())
    }

    /// `message_start` carries input + cache counts; `message_delta` the cumulative output
    /// (and, on newer API versions, final input counts). Present fields overwrite.
    fn merge_usage(&mut self, u: &Value) {
        let set = |field: &str, slot: &mut u64| {
            if let Some(n) = u[field].as_u64() {
                *slot = n;
            }
        };
        set("input_tokens", &mut self.usage.input_tokens);
        set("output_tokens", &mut self.usage.output_tokens);
        set("cache_read_input_tokens", &mut self.usage.cache_read_tokens);
        set("cache_creation_input_tokens", &mut self.usage.cache_write_tokens);
    }

    pub fn finish(self, sink: &mut Sink<'_>) -> Result<Response, LlmError> {
        if !self.stopped && self.stop.is_none() {
            return Err(LlmError::Transient(format!(
                "route `{}` closed the stream before the model finished; the partial response was discarded",
                self.route
            )));
        }
        let mut content = Vec::new();
        let mut has_tools = false;
        for b in self.blocks.into_values() {
            match b {
                Block::Text(t) if !t.is_empty() => content.push(ContentBlock::Text { text: t }),
                Block::Thinking(t) if !t.is_empty() => content.push(ContentBlock::Thinking { text: t }),
                Block::ToolUse { id, name, json, args } => {
                    has_tools = true;
                    let args = match args {
                        Some(a) => a,
                        // no content_block_stop (the stream ended on message_delta): parse now
                        None => {
                            let a = parse_args(&json);
                            sink(StreamEvent::ToolUseEnd { id: CallId(id.clone()), name: name.clone(), args: a.clone() });
                            a
                        }
                    };
                    content.push(ContentBlock::ToolUse { id: CallId(id), name, args });
                }
                _ => {}
            }
        }
        let stop = match self.stop.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens" | "model_context_window_exceeded") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            Some(_) => StopReason::EndTurn,
            None if has_tools => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        Ok(Response { content, usage: self.usage, stop })
    }
}

fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => Value::String(raw.to_string()),
    }
}

fn snippet(s: &str) -> String {
    let t = s.trim();
    match t.char_indices().nth(400) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

fn capture_pair(text: &str, pattern: &str) -> Option<(u32, u32)> {
    let c = Regex::new(pattern).ok()?.captures(text)?;
    Some((c.get(1)?.as_str().parse().ok()?, c.get(2)?.as_str().parse().ok()?))
}

/// Maps a failed request (`status`) or an in-stream `error` event (`status = None`).
pub fn classify(status: Option<u16>, retry_after_ms: Option<u64>, body: &str, has_images: bool, route: &str) -> LlmError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let kind = parsed["error"]["type"].as_str().unwrap_or("");
    let message = parsed["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| snippet(body));
    let lower = message.to_lowercase();
    let at = match status {
        Some(s) => format!("route `{route}` answered HTTP {s}"),
        None => format!("route `{route}` reported an error mid-stream"),
    };

    if lower.contains("prompt is too long") || lower.contains("exceeds the context window") || lower.contains("context window exceeded") {
        let (requested, available) = capture_pair(&message, r"(\d+) tokens > (\d+)").unwrap_or((0, 0));
        return LlmError::ContextWindowExceeded { requested, available };
    }
    if lower.contains("image") && (lower.contains("exceeds") || lower.contains("too large")) {
        return LlmError::ImageOffloadRequired;
    }
    if kind == "request_too_large" || status == Some(413) {
        return if has_images {
            LlmError::ImageOffloadRequired
        } else {
            LlmError::Fatal(format!("{at}: the request body is larger than the API accepts ({message}); compact the conversation"))
        };
    }
    if lower.contains("max_tokens") && lower.contains("maximum allowed") {
        return LlmError::MaxOutputTokens;
    }
    match (kind, status) {
        ("authentication_error" | "permission_error", _) | (_, Some(401 | 403)) => LlmError::Auth(format!(
            "{at}: {message}; set a valid key with `aho credential set {route}` (config key `provider.{route}.key`)"
        )),
        // No header: one second, the smallest wait the server did not contradict.
        ("rate_limit_error", _) | (_, Some(429)) => LlmError::RateLimited { retry_after_ms: retry_after_ms.unwrap_or(1000) },
        ("overloaded_error" | "api_error" | "timeout_error", _) | (_, Some(408 | 500..=599)) => {
            LlmError::Transient(format!("{at}: {message}"))
        }
        _ => LlmError::Fatal(format!("{at}: the request was rejected ({message}); nothing was retried")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_types::Message;

    fn call(id: &str) -> CallId {
        CallId(id.into())
    }

    #[test]
    fn cache_breakpoint_lands_on_the_marked_section_or_the_block_before_an_empty_one() {
        let mut req = Request { model: "m".into(), messages: vec![Message::user("hi")], ..Default::default() };
        req.system =
            vec![("identity".into(), "A".into(), false), ("tools".into(), "B".into(), true), ("dynamic".into(), "C".into(), false)];
        let body = encode_request(&req);
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "A"},
                {"type": "text", "text": "B", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "C"},
            ])
        );
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        req.system = vec![("identity".into(), "A".into(), false), ("empty".into(), "".into(), true)];
        assert_eq!(encode_request(&req)["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        req.system = (0..6).map(|i| (format!("s{i}"), format!("t{i}"), true)).collect();
        let marked = encode_request(&req)["system"].as_array().map(|a| a.iter().filter(|b| b.get("cache_control").is_some()).count());
        assert_eq!(marked, Some(4), "the API accepts at most four breakpoints");
    }

    #[test]
    fn tool_use_args_that_are_not_objects_are_wrapped() {
        let req = Request {
            model: "m".into(),
            messages: vec![
                Message::user("x"),
                Message::assistant(vec![ContentBlock::ToolUse { id: call("t"), name: "bash".into(), args: Value::String("{oops".into()) }]),
            ],
            ..Default::default()
        };
        assert_eq!(encode_request(&req)["messages"][1]["content"][0]["input"], json!({"_args": "{oops"}));
    }

    #[test]
    fn classify_maps_documented_error_bodies() {
        let too_long =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 208310 tokens > 200000 maximum"}}"#;
        assert_eq!(
            classify(Some(400), None, too_long, false, "anthropic"),
            LlmError::ContextWindowExceeded { requested: 208_310, available: 200_000 }
        );
        let image = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.1.image.source.base64: image exceeds 5 MB maximum: 7340032 bytes > 5242880 bytes"}}"#;
        assert_eq!(classify(Some(400), None, image, true, "anthropic"), LlmError::ImageOffloadRequired);
        let max = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 100000 > 64000, which is the maximum allowed number of output tokens for claude-sonnet-4-5-20250929"}}"#;
        assert_eq!(classify(Some(400), None, max, false, "anthropic"), LlmError::MaxOutputTokens);
        let overloaded = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert!(matches!(classify(None, None, overloaded, false, "anthropic"), LlmError::Transient(m) if m.contains("mid-stream")));
        let auth = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert!(
            matches!(classify(Some(401), None, auth, false, "anthropic"), LlmError::Auth(m) if m.contains("aho credential set anthropic"))
        );
        let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#;
        assert!(matches!(classify(Some(400), None, bad, false, "anthropic"), LlmError::Fatal(m) if m.contains("roles must alternate")));
    }
}
