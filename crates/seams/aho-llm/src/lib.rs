//! The model seam, keyed by *protocol* rather than vendor: an OpenAI-compatible vendor is a new
//! `provider.<name>` config row on an existing adapter, with no new streaming code.
//!
//! Adapters register one [`LlmRoute`] per configured route as the Registry service
//! `llm:<route>` ([`service_name`]); the session picks one by `model.provider`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use aho_cap::{Handle, HandleKind};
use aho_types::{BoxFuture, CallId, ContentBlock, Hash, Message, StopReason, ToolSpec, Usage, Value};

pub mod models;
pub mod sse;

pub const SEAM: &str = "llm";

/// Registry service name for a route: `llm:<route>`.
pub fn service_name(route: &str) -> String {
    format!("llm:{route}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    PiMessages,
}

impl Protocol {
    /// The value of `provider.<name>.protocol` that selects this adapter.
    pub fn tag(&self) -> &'static str {
        match self {
            Protocol::AnthropicMessages => "anthropic-messages",
            Protocol::OpenAiCompletions => "openai-completions",
            Protocol::OpenAiResponses => "openai-responses",
            Protocol::PiMessages => "pi-messages",
        }
    }
}

/// The way an adapter represents a tool result that arrives after later turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LateResults {
    /// A second output item for the same call id (Responses API).
    SameCallId,
    /// A `tool` message referencing the original id (Completions).
    ToolMessage,
    /// Protocol requires results adjacent to their call; late ones render as a user block.
    AsUserBlock,
    /// Cannot represent placeholders; the loop falls back to a bounded wait (`⚠ sync fallback`).
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterInfo {
    pub protocol: Protocol,
    pub late_results: LateResults,
    /// The route name this instance serves (`openai`, `ollama`, `anthropic`, a `provider.<name>` row).
    pub route: String,
}
/// Model reasoning requested for the next answer. Adapters translate this into their native
/// control; adapters without a compatible control ignore it but still surface reasoning events
/// the provider sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reasoning {
    /// Token budget for providers with an explicit budget (Anthropic).
    pub budget_tokens: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub model: String,
    /// `(name, text, cache_break)` in order; `cache_break` marks the end of the cache-stable prefix.
    pub system: Vec<(String, String, bool)>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub reasoning: Option<Reasoning>,
    /// Provider-side cache routing key (e.g. the session id); adapters that support one forward it.
    pub cache_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub content: Vec<ContentBlock>,
    pub usage: Usage,
    pub stop: StopReason,
}

/// Live, non-durable stream events (the durable record is the final [`Response`]).
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Thinking(String),
    ToolUseStart {
        id: CallId,
        name: String,
    },
    /// A tool may start the moment its block finishes streaming. `args` are the model's
    /// bytes parsed as JSON; unparseable bytes arrive as a JSON string so the tool's schema check
    /// reports them instead of the adapter guessing.
    ToolUseEnd {
        id: CallId,
        name: String,
        args: Value,
    },
    Usage(Usage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmError {
    /// Either number is 0 when the provider did not state it.
    ContextWindowExceeded {
        requested: u32,
        available: u32,
    },
    ImageOffloadRequired,
    MaxOutputTokens,
    RateLimited {
        retry_after_ms: u64,
    },
    Auth(String),
    Transient(String),
    Fatal(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::ContextWindowExceeded { requested, available } if *requested > 0 && *available > 0 => write!(
                f,
                "the model rejected the request: it needs {requested} tokens and the context window holds {available}; compact the conversation or pick a model with a larger window"
            ),
            LlmError::ContextWindowExceeded { .. } => write!(
                f,
                "the model rejected the request as larger than its context window; compact the conversation or pick a model with a larger window"
            ),
            LlmError::ImageOffloadRequired => {
                write!(f, "the provider rejected an inline image as too large; resize the image or pass it as a file reference")
            }
            LlmError::MaxOutputTokens => {
                write!(f, "the provider rejected the requested output length as above this model's limit; lower `model.max_tokens`")
            }
            LlmError::RateLimited { retry_after_ms } => write!(
                f,
                "the provider is rate-limiting requests; the request can be retried after {retry_after_ms} ms, or switch `model.provider`"
            ),
            LlmError::Auth(detail) => write!(f, "the provider rejected the credentials: {detail}"),
            LlmError::Transient(detail) => write!(f, "the provider failed temporarily ({detail}); the request can be retried"),
            LlmError::Fatal(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Receives live events while a response streams.
pub type Sink<'a> = Box<dyn FnMut(StreamEvent) + Send + 'a>;

/// One `stream` per protocol.
pub trait Llm: Send + Sync {
    fn info(&self) -> AdapterInfo;
    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>>;
}

/// A model route, registered as the service `llm:<route>`. Also usable as a `Deps` handle
/// (`HandleKind::Llm`) by capabilities that call a model themselves.
#[derive(Clone)]
pub struct LlmRoute(pub Arc<dyn Llm>);

impl Handle for LlmRoute {
    const KIND: HandleKind = HandleKind::Llm;
}

impl fmt::Debug for LlmRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let info = self.0.info();
        write!(f, "LlmRoute({} via {})", info.route, info.protocol.tag())
    }
}

/// `derive_messages` places a late `Tool/Result` at its log position: a second
/// `ToolResult` for an id whose first occurrence was the [`aho_log::TOOL_CALL_RUNNING`]
/// placeholder. This renders every such repeat the way `policy` says the protocol can carry it:
///
/// * `SameCallId`, `ToolMessage`: unchanged; the adapter encodes the repeat natively
///   (a second `function_call_output`, a `tool` message with the original `tool_call_id`).
/// * `AsUserBlock`: the late block becomes user text `[result for call <id> (<tool>)]\n<content>`
///   (images kept as separate blocks), so the protocol's "results directly follow their call"
///   rule holds while the placeholder stays where the model saw it.
/// * `Unsupported`: unchanged; such adapters cannot show placeholders, so the loop waits.
pub fn normalize_late_results(messages: &[Message], policy: LateResults) -> Vec<Message> {
    if policy != LateResults::AsUserBlock {
        return messages.to_vec();
    }
    let mut tool_names: BTreeMap<&CallId, &str> = BTreeMap::new();
    let mut answered: BTreeSet<&CallId> = BTreeSet::new();
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let mut content = Vec::with_capacity(m.content.len());
        for b in &m.content {
            match b {
                ContentBlock::ToolUse { id, name, .. } => {
                    tool_names.insert(id, name);
                    content.push(b.clone());
                }
                ContentBlock::ToolResult { id, content: body, is_error } => {
                    if answered.insert(id) {
                        content.push(b.clone());
                        continue;
                    }
                    content.extend(late_as_user_blocks(id, tool_names.get(id).copied(), body, *is_error));
                }
                _ => content.push(b.clone()),
            }
        }
        out.push(Message { role: m.role, content });
    }
    out
}

fn late_as_user_blocks(id: &CallId, tool: Option<&str>, body: &[ContentBlock], is_error: bool) -> Vec<ContentBlock> {
    let mut header = match tool {
        Some(t) => format!("[result for call {} ({t})", id.0),
        None => format!("[result for call {}", id.0),
    };
    header.push_str(if is_error { ", failed]" } else { "]" });
    let text = joined_text(body);
    let mut blocks = vec![ContentBlock::text(if text.trim().is_empty() { header } else { format!("{header}\n{text}") })];
    blocks.extend(body.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).cloned());
    blocks
}

/// True when `block` is the fixed placeholder `derive_messages` renders for a still-open call.
pub fn is_placeholder(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolResult { content, .. }
        if content.len() == 1 && content[0].as_text() == Some(aho_log::TOOL_CALL_RUNNING))
}

/// Client-side prefix-hash tracker; works for providers that report no cache metrics.
///
/// `hashes[0]` covers what providers cache ahead of the conversation (system sections and the
/// tool list); `hashes[i]` for `i >= 1` chains message `i - 1` onto `hashes[i - 1]`, so equal
/// hashes at an index mean an identical prefix up to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrefixTracker {
    hashes: Vec<Hash>,
}

impl PrefixTracker {
    pub fn of(req: &Request) -> PrefixTracker {
        let mut hashes = Vec::with_capacity(req.messages.len() + 1);
        let mut running = Hash::of_json(&(&req.system, &req.tools));
        hashes.push(running);
        for m in &req.messages {
            running = Hash::chain(&running, &Hash::of_json(m).0);
            hashes.push(running);
        }
        PrefixTracker { hashes }
    }

    pub fn hashes(&self) -> &[Hash] {
        &self.hashes
    }

    /// First index at which `previous` is not a prefix of `self`: `Some(0)` = system/tools changed,
    /// `Some(i)` = message `i - 1` changed (or, when `i == self.hashes().len()`, the request is
    /// shorter than `previous`). `None` = the previous request is an unchanged prefix of this one.
    pub fn check(&self, previous: &PrefixTracker) -> Option<usize> {
        (0..previous.hashes.len()).find(|&i| self.hashes.get(i) != Some(&previous.hashes[i]))
    }
}

/// Standard base64 (RFC 4648, padded) for inline image payloads.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let b = [chunk[0], if n > 1 { chunk[1] } else { 0 }, if n > 2 { chunk[2] } else { 0 }];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(v >> 18) as usize & 63] as char);
        out.push(ALPHABET[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 { ALPHABET[(v >> 6) as usize & 63] as char } else { '=' });
        out.push(if n > 2 { ALPHABET[v as usize & 63] as char } else { '=' });
    }
    out
}

/// The `Text` blocks of `blocks` joined by newlines (tool-result bodies, expectation checks).
pub fn joined_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Milliseconds to wait from a 429/503 response's `retry-after-ms` (preferred, fractional ms)
/// or `retry-after` (delta-seconds, fractional accepted) header. HTTP-date values yield `None`.
pub fn retry_after_ms(retry_after_ms: Option<&str>, retry_after: Option<&str>) -> Option<u64> {
    let parse = |s: Option<&str>| s.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0);
    if let Some(ms) = parse(retry_after_ms) {
        return Some(ms.ceil() as u64);
    }
    parse(retry_after).map(|s| (s * 1000.0).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_types::Role;
    use serde_json::json;

    fn call(id: &str) -> CallId {
        CallId(id.into())
    }

    fn use_block(id: &str) -> ContentBlock {
        ContentBlock::ToolUse { id: call(id), name: "bash".into(), args: json!({"command": "cargo test"}) }
    }

    fn result(id: &str, text: &str) -> ContentBlock {
        ContentBlock::ToolResult { id: call(id), content: vec![ContentBlock::text(text)], is_error: false }
    }

    /// The shape `derive_messages` produces for a call that was a placeholder and landed later.
    fn late_history() -> Vec<Message> {
        vec![
            Message::user("run the tests"),
            Message::assistant(vec![use_block("a"), use_block("b")]),
            Message { role: Role::User, content: vec![result("a", "ok"), result("b", aho_log::TOOL_CALL_RUNNING)] },
            Message::assistant(vec![ContentBlock::text("reading code meanwhile")]),
            Message { role: Role::User, content: vec![result("b", "412 passed")] },
        ]
    }

    #[test]
    fn as_user_block_rewrites_only_the_late_repeat() {
        let msgs = late_history();
        assert!(is_placeholder(&msgs[2].content[1]));
        let out = normalize_late_results(&msgs, LateResults::AsUserBlock);
        assert_eq!(out[..4], msgs[..4], "the placeholder and everything before the late result are untouched");
        assert_eq!(out[4].role, Role::User);
        assert_eq!(out[4].content, vec![ContentBlock::text("[result for call b (bash)]\n412 passed")]);
    }

    #[test]
    fn as_user_block_marks_failures_and_keeps_images() {
        let mut msgs = late_history();
        msgs[4].content = vec![ContentBlock::ToolResult {
            id: call("b"),
            content: vec![ContentBlock::text("boom"), ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2] }],
            is_error: true,
        }];
        let out = normalize_late_results(&msgs, LateResults::AsUserBlock);
        assert_eq!(
            out[4].content,
            vec![
                ContentBlock::text("[result for call b (bash), failed]\nboom"),
                ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2] },
            ]
        );
    }

    #[test]
    fn as_user_block_without_a_known_call_omits_the_tool_name() {
        let msgs = vec![
            Message { role: Role::User, content: vec![result("x", aho_log::TOOL_CALL_RUNNING)] },
            Message { role: Role::User, content: vec![result("x", "")] },
        ];
        let out = normalize_late_results(&msgs, LateResults::AsUserBlock);
        assert_eq!(out[1].content, vec![ContentBlock::text("[result for call x]")]);
    }

    #[test]
    fn native_and_unsupported_policies_leave_history_unchanged() {
        let msgs = late_history();
        for p in [LateResults::SameCallId, LateResults::ToolMessage, LateResults::Unsupported] {
            assert_eq!(normalize_late_results(&msgs, p), msgs, "{p:?}");
        }
    }

    #[test]
    fn a_single_result_per_call_is_never_rewritten() {
        let msgs = vec![Message::assistant(vec![use_block("a")]), Message { role: Role::User, content: vec![result("a", "ok")] }];
        assert_eq!(normalize_late_results(&msgs, LateResults::AsUserBlock), msgs);
    }

    fn req(messages: Vec<Message>) -> Request {
        Request {
            model: "m".into(),
            system: vec![("identity".into(), "you are aho".into(), true)],
            messages,
            max_tokens: 10,
            ..Default::default()
        }
    }

    #[test]
    fn prefix_tracker_accepts_growth_and_flags_the_first_changed_message() {
        let a = PrefixTracker::of(&req(vec![Message::user("one"), Message::user("two")]));
        let grown = PrefixTracker::of(&req(vec![Message::user("one"), Message::user("two"), Message::user("three")]));
        assert_eq!(a.hashes().len(), 3);
        assert_eq!(grown.check(&a), None);
        let edited = PrefixTracker::of(&req(vec![Message::user("one"), Message::user("TWO"), Message::user("three")]));
        assert_eq!(edited.check(&a), Some(2), "message 1 changed → index 2");
        assert_eq!(edited.check(&grown), Some(2));
    }

    #[test]
    fn prefix_tracker_flags_system_tool_changes_and_truncation() {
        let base = req(vec![Message::user("one"), Message::user("two")]);
        let a = PrefixTracker::of(&base);
        let mut sys = base.clone();
        sys.system[0].1 = "you are someone else".into();
        assert_eq!(PrefixTracker::of(&sys).check(&a), Some(0));
        let mut tools = base.clone();
        tools.tools.push(ToolSpec {
            name: "read".into(),
            description: "d".into(),
            input_schema: Default::default(),
            class: aho_types::Consequence::ReadOnly,
        });
        assert_eq!(PrefixTracker::of(&tools).check(&a), Some(0));
        let compacted = PrefixTracker::of(&req(vec![Message::user("one")]));
        assert_eq!(compacted.check(&a), Some(2), "a shorter request cannot contain the previous prefix");
        assert_eq!(a.check(&PrefixTracker::default()), None);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, enc) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(plain.as_bytes()), enc);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn errors_render_operator_facing_sentences() {
        let e = LlmError::ContextWindowExceeded { requested: 130_000, available: 128_000 };
        assert!(e.to_string().contains("130000 tokens and the context window holds 128000"));
        let unknown = LlmError::ContextWindowExceeded { requested: 0, available: 128_000 };
        assert!(!unknown.to_string().contains(" 0 "), "unknown counts are not printed as zero");
        assert!(LlmError::RateLimited { retry_after_ms: 1500 }.to_string().contains("1500 ms"));
    }

    #[test]
    fn retry_after_prefers_milliseconds_and_rejects_dates() {
        assert_eq!(retry_after_ms(Some("250.5"), Some("9")), Some(251));
        assert_eq!(retry_after_ms(None, Some("7")), Some(7000));
        assert_eq!(retry_after_ms(None, Some("0.5")), Some(500));
        assert_eq!(retry_after_ms(Some("soon"), Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
        assert_eq!(retry_after_ms(None, Some("-3")), None);
    }
}
