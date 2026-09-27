//! Chat-completions wire format: request encoding, streamed-chunk decoding, error mapping.
//! Pure (no I/O) so every mapping is testable against recorded bodies.

use std::collections::BTreeMap;

use aho_llm::{Request, Response, Sink, StreamEvent, joined_text};
use aho_types::{CallId, ContentBlock, Message, Role, StopReason, Timestamp, Usage, Value};
use regex::Regex;
use serde_json::{Map, json};

/// The dialect a route speaks on top of the common protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// api.openai.com: `max_completion_tokens`, `prompt_cache_key`.
    OpenAi,
    /// OpenAI-compatible servers (Ollama, llama.cpp, vLLM, hosted vendors): `max_tokens`, no
    /// OpenAI-only parameters, so strict servers do not reject the body.
    Compatible,
}

/// The JSON body for `POST {base_url}/chat/completions`.
///
/// Late results (`LateResults::ToolMessage`) need no rewriting: a second `ToolResult` for
/// an id becomes a second `tool` message carrying the original `tool_call_id`.
pub fn encode_request(req: &Request, flavor: Flavor) -> Value {
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
    let system: Vec<&str> = req.system.iter().map(|(_, text, _)| text.as_str()).filter(|t| !t.trim().is_empty()).collect();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system.join("\n\n")}));
    }
    for m in &req.messages {
        encode_message(m, &mut messages);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if req.max_tokens > 0 {
        let field = match flavor {
            Flavor::OpenAi => "max_completion_tokens",
            Flavor::Compatible => "max_tokens",
        };
        body.insert(field.into(), json!(req.max_tokens));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(
                |t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema.0}}),
            )
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let (Flavor::OpenAi, Some(key)) = (flavor, &req.cache_key) {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    Value::Object(body)
}

fn encode_message(m: &Message, out: &mut Vec<Value>) {
    match m.role {
        Role::System => {
            let text = joined_text(&m.content);
            if !text.is_empty() {
                out.push(json!({"role": "system", "content": text}));
            }
        }
        Role::Assistant => {
            let text = joined_text(&m.content);
            let calls: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, args } => {
                        let arguments = match args {
                            // unparseable model bytes are carried as a string; send them back verbatim
                            Value::String(raw) => raw.clone(),
                            other => other.to_string(),
                        };
                        Some(json!({"id": id.0, "type": "function", "function": {"name": name, "arguments": arguments}}))
                    }
                    _ => None,
                })
                .collect();
            if text.is_empty() && calls.is_empty() {
                return;
            }
            let mut msg = Map::new();
            msg.insert("role".into(), json!("assistant"));
            msg.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
            if !calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(calls));
            }
            out.push(Value::Object(msg));
        }
        Role::User | Role::Tool => {
            // `tool` messages must directly follow the assistant message that issued the calls,
            // so they go first; the rest of the turn follows as one user message.
            let mut parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::ToolResult { id, content, is_error } => {
                        let text = joined_text(content);
                        let text = if *is_error { format!("[tool error]\n{text}") } else { text };
                        out.push(json!({"role": "tool", "tool_call_id": id.0, "content": text}));
                        for img in content.iter().filter(|c| matches!(c, ContentBlock::Image { .. })) {
                            parts.push(json!({"type": "text", "text": format!("[image from call {}]", id.0)}));
                            parts.extend(user_part(img));
                        }
                    }
                    other => parts.extend(user_part(other)),
                }
            }
            match parts.as_slice() {
                [] => {}
                [only] if only["type"] == "text" => out.push(json!({"role": "user", "content": only["text"]})),
                _ => out.push(json!({"role": "user", "content": parts})),
            }
        }
    }
}

fn user_part(b: &ContentBlock) -> Option<Value> {
    match b {
        ContentBlock::Text { text } if !text.is_empty() => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { media_type, data } => {
            Some(json!({"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{}", aho_llm::base64(data))}}))
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    started: bool,
    ended: bool,
}

/// Folds `chat.completion.chunk` payloads into live events and the final [`Response`].
#[derive(Debug, Default)]
pub struct StreamDecoder {
    route: String,
    response_id: String,
    text: String,
    thinking: String,
    tools: BTreeMap<u64, ToolAcc>,
    finish: Option<String>,
    usage: Usage,
    done: bool,
}

impl StreamDecoder {
    pub fn new(route: &str) -> StreamDecoder {
        StreamDecoder { route: route.to_string(), ..StreamDecoder::default() }
    }

    /// True once `data: [DONE]` arrived.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// One SSE `data:` payload.
    pub fn on_data(&mut self, data: &str, sink: &mut Sink<'_>) -> Result<(), aho_llm::LlmError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            aho_llm::LlmError::Transient(format!("route `{}` streamed a chunk that is not JSON ({e}): {}", self.route, snippet(data)))
        })?;
        if chunk.get("error").is_some_and(|e| !e.is_null()) {
            return Err(classify(None, None, data, false, &self.route));
        }
        if self.response_id.is_empty()
            && let Some(id) = chunk["id"].as_str()
        {
            self.response_id = id.to_string();
        }
        if let Some(choices) = chunk["choices"].as_array() {
            for choice in choices.iter().filter(|c| c["index"].as_u64().unwrap_or(0) == 0) {
                self.on_delta(&choice["delta"], sink);
                if let Some(reason) = choice["finish_reason"].as_str() {
                    self.finish = Some(reason.to_string());
                    self.end_tools_below(u64::MAX, sink);
                }
            }
        }
        if chunk["usage"].is_object() {
            self.usage = usage(&chunk["usage"]);
            sink(StreamEvent::Usage(self.usage));
        }
        Ok(())
    }

    fn on_delta(&mut self, delta: &Value, sink: &mut Sink<'_>) {
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta[key].as_str().filter(|t| !t.is_empty()) {
                self.thinking.push_str(t);
                sink(StreamEvent::Thinking(t.to_string()));
            }
        }
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            sink(StreamEvent::Text(t.to_string()));
        }
        let Some(calls) = delta["tool_calls"].as_array() else { return };
        for call in calls {
            let index = match call["index"].as_u64() {
                Some(i) => i,
                // servers that omit `index`: a new id opens a new call, otherwise continue the last
                None => match (call["id"].as_str(), self.tools.last_key_value()) {
                    (Some(id), Some((k, acc))) if acc.id != id => k + 1,
                    (_, Some((k, _))) => *k,
                    (_, None) => 0,
                },
            };
            if !self.tools.contains_key(&index) {
                // calls stream in index order: a new index means every earlier one is complete
                self.end_tools_below(index, sink);
            }
            let acc = self.tools.entry(index).or_default();
            if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty())
                && acc.id.is_empty()
            {
                acc.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str().filter(|s| !s.is_empty())
                && acc.name.is_empty()
            {
                acc.name = name.to_string();
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                acc.args.push_str(args);
            }
            if !acc.started && !acc.name.is_empty() {
                if acc.id.is_empty() {
                    acc.id = synthesized_id(&self.response_id, index);
                }
                acc.started = true;
                sink(StreamEvent::ToolUseStart { id: CallId(acc.id.clone()), name: acc.name.clone() });
            }
        }
    }

    fn end_tools_below(&mut self, below: u64, sink: &mut Sink<'_>) {
        let response_id = &self.response_id;
        for (index, acc) in self.tools.range_mut(..below) {
            if acc.ended {
                continue;
            }
            if !acc.started {
                if acc.id.is_empty() {
                    acc.id = synthesized_id(response_id, *index);
                }
                acc.started = true;
                sink(StreamEvent::ToolUseStart { id: CallId(acc.id.clone()), name: acc.name.clone() });
            }
            acc.ended = true;
            sink(StreamEvent::ToolUseEnd { id: CallId(acc.id.clone()), name: acc.name.clone(), args: parse_args(&acc.args) });
        }
    }

    /// End of the byte stream.
    pub fn finish(mut self, sink: &mut Sink<'_>) -> Result<Response, aho_llm::LlmError> {
        if !self.done && self.finish.is_none() {
            return Err(aho_llm::LlmError::Transient(format!(
                "route `{}` closed the stream before the model finished; the partial response was discarded",
                self.route
            )));
        }
        self.end_tools_below(u64::MAX, sink);
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking { text: self.thinking });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }
        let has_tools = !self.tools.is_empty();
        for acc in self.tools.into_values() {
            content.push(ContentBlock::ToolUse { id: CallId(acc.id), name: acc.name, args: parse_args(&acc.args) });
        }
        let stop = match self.finish.as_deref() {
            Some("tool_calls" | "function_call") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            // Ollama and others report `stop` even when the turn ends in tool calls
            _ if has_tools => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        Ok(Response { content, usage: self.usage, stop })
    }
}

fn synthesized_id(response_id: &str, index: u64) -> String {
    if response_id.is_empty() { format!("call-{}-{index}", Timestamp::now().0) } else { format!("{response_id}-{index}") }
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

/// `prompt_tokens` includes cached tokens; `Usage::input_tokens` counts only uncached input.
fn usage(u: &Value) -> Usage {
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

fn snippet(s: &str) -> String {
    let t = s.trim();
    match t.char_indices().nth(400) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

fn capture_u32(text: &str, patterns: &[&str]) -> Option<u32> {
    patterns.iter().find_map(|p| Regex::new(p).ok()?.captures(text)?.get(1)?.as_str().parse().ok())
}

/// Maps a failed request (`status`) or an in-stream `{"error": …}` chunk (`status = None`) to
/// the seam's error vocabulary.
pub fn classify(status: Option<u16>, retry_after_ms: Option<u64>, body: &str, has_images: bool, route: &str) -> aho_llm::LlmError {
    use aho_llm::LlmError;
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = if parsed["error"].is_object() { &parsed["error"] } else { &parsed };
    let message = err["message"].as_str().or_else(|| parsed["error"].as_str()).map(str::to_string).unwrap_or_else(|| snippet(body));
    let code = err["code"].as_str().unwrap_or("");
    let kind = err["type"].as_str().unwrap_or("");
    let lower = message.to_lowercase();
    let at = match status {
        Some(s) => format!("route `{route}` answered HTTP {s}"),
        None => format!("route `{route}` reported an error mid-stream"),
    };

    let context = code == "context_length_exceeded"
        || kind == "exceed_context_size_error"
        || lower.contains("maximum context length")
        || lower.contains("exceeds the available context size")
        || lower.contains("exceeds the context window");
    if context {
        let requested = err["n_prompt_tokens"]
            .as_u64()
            .map(|n| n as u32)
            .or_else(|| capture_u32(&message, &[r"resulted in (\d+) tokens", r"requested (\d+) tokens", r"request \((\d+) tokens\)"]));
        let available = err["n_ctx"]
            .as_u64()
            .map(|n| n as u32)
            .or_else(|| capture_u32(&message, &[r"maximum context length is (\d+)", r"context size \((\d+) tokens\)"]));
        return LlmError::ContextWindowExceeded { requested: requested.unwrap_or(0), available: available.unwrap_or(0) };
    }
    if (lower.contains("max_tokens") || lower.contains("max_completion_tokens"))
        && (lower.contains("too large") || lower.contains("supports at most"))
    {
        return LlmError::MaxOutputTokens;
    }
    match status {
        Some(401 | 403) => LlmError::Auth(format!(
            "{at}: {message}; set a valid key with `aho credential set {route}` (config key `provider.{route}.key`)"
        )),
        Some(429) if code == "insufficient_quota" || kind == "insufficient_quota" => {
            LlmError::Fatal(format!("{at}: the account's quota is exhausted ({message}); add credit or switch `model.provider`"))
        }
        // No header: one second, the smallest wait the server did not contradict.
        Some(429) => LlmError::RateLimited { retry_after_ms: retry_after_ms.unwrap_or(1000) },
        Some(413) if has_images => LlmError::ImageOffloadRequired,
        Some(408 | 409 | 500..=599) => LlmError::Transient(format!("{at}: {message}")),
        None if kind == "server_error" || code == "server_error" || kind.contains("overloaded") => {
            LlmError::Transient(format!("{at}: {message}"))
        }
        _ => LlmError::Fatal(format!("{at}: the request was rejected ({message}); nothing was retried")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_llm::{LateResults, LlmError, normalize_late_results};
    use aho_types::{Consequence, RootSchema, ToolSpec};

    fn call(id: &str) -> CallId {
        CallId(id.into())
    }

    fn request(messages: Vec<Message>) -> Request {
        Request {
            model: "gpt-4.1-mini".into(),
            system: vec![
                ("identity".into(), "You are aho.".into(), true),
                ("empty".into(), " ".into(), false),
                ("tools".into(), "Go wide.".into(), false),
            ],
            messages,
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: RootSchema(json!({"type": "object", "properties": {"path": {"type": "string"}}})),
                class: Consequence::ReadOnly,
            }],
            max_tokens: 512,
            reasoning: None,
            cache_key: Some("session-7".into()),
        }
    }

    #[test]
    fn encodes_system_tools_and_flavor_specific_fields() {
        let body = encode_request(&request(vec![Message::user("hi")]), Flavor::OpenAi);
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "You are aho.\n\nGo wide."}));
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "hi"}));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body["max_completion_tokens"], json!(512));
        assert_eq!(body["prompt_cache_key"], json!("session-7"));
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"], json!("string"));
        let compat = encode_request(&request(vec![Message::user("hi")]), Flavor::Compatible);
        assert_eq!(compat["max_tokens"], json!(512));
        assert!(compat.get("max_completion_tokens").is_none() && compat.get("prompt_cache_key").is_none());
    }

    #[test]
    fn encodes_tool_calls_results_images_and_late_results_as_tool_messages() {
        let history = vec![
            Message::user("look"),
            Message::assistant(vec![
                ContentBlock::Thinking { text: "hidden".into() },
                ContentBlock::text("Checking."),
                ContentBlock::ToolUse { id: call("c1"), name: "read".into(), args: json!({"path": "a.rs"}) },
                ContentBlock::ToolUse { id: call("c2"), name: "bash".into(), args: Value::String("{\"cmd\": oops".into()) },
            ]),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult { id: call("c1"), content: vec![ContentBlock::text("fn main() {}")], is_error: false },
                    ContentBlock::ToolResult {
                        id: call("c2"),
                        content: vec![ContentBlock::text(aho_log::TOOL_CALL_RUNNING)],
                        is_error: false,
                    },
                    ContentBlock::Image { media_type: "image/png".into(), data: b"PNG".to_vec() },
                ],
            },
            Message::assistant(vec![ContentBlock::text("Waiting.")]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult { id: call("c2"), content: vec![ContentBlock::text("exit 1")], is_error: true }],
            },
        ];
        assert_eq!(normalize_late_results(&history, LateResults::ToolMessage), history);
        let body = encode_request(&request(history), Flavor::OpenAi);
        let m = body["messages"].as_array().cloned().unwrap_or_default();
        assert_eq!(m[2]["content"], json!("Checking."), "thinking is not sent back");
        assert_eq!(
            m[2]["tool_calls"][0],
            json!({"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}})
        );
        assert_eq!(m[2]["tool_calls"][1]["function"]["arguments"], json!("{\"cmd\": oops"), "malformed args go back verbatim");
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "c1", "content": "fn main() {}"}));
        assert_eq!(m[4]["tool_call_id"], json!("c2"));
        assert_eq!(m[5]["role"], json!("user"));
        assert_eq!(m[5]["content"][0]["image_url"]["url"], json!("data:image/png;base64,UE5H"));
        assert_eq!(m[6], json!({"role": "assistant", "content": "Waiting."}));
        assert_eq!(
            m[7],
            json!({"role": "tool", "tool_call_id": "c2", "content": "[tool error]\nexit 1"}),
            "late result reuses the original id"
        );
        assert_eq!(m.len(), 8);
    }

    fn collect(chunks: &[&str]) -> (Vec<StreamEvent>, Result<Response, LlmError>) {
        let mut events = Vec::new();
        let result = {
            let mut sink: Sink<'_> = Box::new(|e| events.push(e));
            let mut d = StreamDecoder::new("t");
            let mut res = Ok(());
            for c in chunks {
                res = d.on_data(c, &mut sink);
                if res.is_err() {
                    break;
                }
            }
            res.and_then(|_| d.finish(&mut sink))
        };
        (events, result)
    }

    #[test]
    fn missing_index_and_id_still_yield_distinct_calls() {
        let (events, res) = collect(&[
            r#"{"id":"r9","choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":"a","arguments":"{}"}}]}}]}"#,
            r#"{"id":"r9","choices":[{"index":0,"delta":{"tool_calls":[{"id":"x2","function":{"name":"b","arguments":"{\"k\":1}"}}]},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        let res = res.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res.stop, StopReason::ToolUse);
        assert!(matches!(&res.content[0], ContentBlock::ToolUse { id, name, .. } if id.0 == "r9-0" && name == "a"));
        assert!(matches!(&res.content[1], ContentBlock::ToolUse { id, args, .. } if id.0 == "x2" && args == &json!({"k": 1})));
        let ends = events.iter().filter(|e| matches!(e, StreamEvent::ToolUseEnd { .. })).count();
        assert_eq!(ends, 2);
    }

    #[test]
    fn stream_without_finish_or_done_is_transient() {
        let (_, res) = collect(&[r#"{"choices":[{"index":0,"delta":{"content":"par"}}]}"#]);
        assert!(matches!(res, Err(LlmError::Transient(m)) if m.contains("closed the stream")));
    }

    #[test]
    fn classify_maps_documented_error_bodies() {
        let quota = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert!(matches!(classify(Some(429), Some(5), quota, false, "openai"), LlmError::Fatal(m) if m.contains("quota")));
        let auth =
            r#"{"error":{"message":"Incorrect API key provided: sk-abc***","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        assert!(matches!(classify(Some(401), None, auth, false, "openai"), LlmError::Auth(m) if m.contains("aho credential set openai")));
        assert!(
            matches!(classify(Some(503), None, "upstream unavailable", false, "groq"), LlmError::Transient(m) if m.contains("HTTP 503"))
        );
        assert_eq!(classify(Some(429), None, "{}", false, "openai"), LlmError::RateLimited { retry_after_ms: 1000 });
        assert_eq!(classify(Some(413), None, "too big", true, "openai"), LlmError::ImageOffloadRequired);
        let too_many = r#"{"error":{"message":"max_tokens is too large: 100000. This model supports at most 32768 completion tokens, whereas you provided 100000.","type":"invalid_request_error","param":"max_tokens","code":"invalid_value"}}"#;
        assert_eq!(classify(Some(400), None, too_many, false, "openai"), LlmError::MaxOutputTokens);
        let vllm = r#"{"object":"error","message":"This model's maximum context length is 4096 tokens. However, you requested 5000 tokens (4000 in the messages, 1000 in the completion). Please reduce the length of the messages or completion.","type":"BadRequestError","param":null,"code":400}"#;
        assert_eq!(classify(Some(400), None, vllm, false, "vllm"), LlmError::ContextWindowExceeded { requested: 5000, available: 4096 });
        assert!(matches!(classify(Some(400), None, r#"{"error":{"message":"bad"}}"#, false, "x"), LlmError::Fatal(_)));
    }
}
