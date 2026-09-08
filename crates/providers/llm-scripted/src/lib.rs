//! Deterministic fake model (01): the route `scripted` consumes a script one step per
//! `stream` call, emitting exactly the scripted events. Expectations turn a drifted conversation
//! into a precise `LlmError::Fatal` instead of a confusing downstream assertion.
//!
//! Script (`provider.scripted.script` = path to JSON; unknown fields are rejected):
//! ```json
//! {"steps": [
//!   {"expect_last_user_contains": "run the tests", "text": "Starting.",
//!    "tool_calls": [{"id": "c1", "name": "bash", "args": {"command": "cargo test"}}]},
//!   {"expect_tool_result_contains": "passed", "thinking": "done", "text": "All green.", "stop": "end_turn"},
//!   {"error": "rate_limited"}
//! ]}
//! ```
//! * `expect_last_user_contains` — text of the most recent user message that has text blocks.
//! * `expect_tool_result_contains` — text of the most recent `ToolResult` block in the request
//!   (a still-running call shows `aho_log::TOOL_CALL_RUNNING`).
//! * `stop` defaults to `tool_use` when the step has tool calls, else `end_turn`.
//! * `error`: `context_window` | `rate_limited` | `transient`; the step emits nothing else.
//! * tool call `id` defaults to `scripted-<instance>-<step>-<k>`; each model instance has a
//!   fresh namespace, so reopening a daemon session cannot reuse an earlier call id.
//! * explicit ids are preserved unless already used; a replayed explicit id is namespaced.
//!
//! A step is consumed even when its expectation fails, so a retry never replays it. The route
//! reports `Protocol::OpenAiResponses` / `LateResults::SameCallId`: the identity late-result
//! rendering, so a script sees exactly the messages `derive_messages` produced.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Protocol, Request, Response, Sink, StreamEvent, joined_text};
use aho_types::{BoxFuture, CallId, ContentBlock, Message, Role, StopReason, Usage, Value};
use serde::Deserialize;

pub const ROUTE: &str = "scripted";

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub expect_last_user_contains: Option<String>,
    #[serde(default)]
    pub expect_tool_result_contains: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ScriptedCall>,
    #[serde(default)]
    pub stop: Option<ScriptedStop>,
    #[serde(default)]
    pub error: Option<ScriptedError>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedCall {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedStop {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedError {
    ContextWindow,
    RateLimited,
    Transient,
}

/// Builders for tests in other crates: `Step::text("hi").expect_user("hello")`.
impl Step {
    pub fn text(text: &str) -> Step {
        Step { text: Some(text.to_string()), ..Step::default() }
    }
    pub fn tool_call(name: &str, args: Value) -> Step {
        Step::default().with_call(None, name, args)
    }
    pub fn error(kind: ScriptedError) -> Step {
        Step { error: Some(kind), ..Step::default() }
    }
    pub fn with_call(mut self, id: Option<&str>, name: &str, args: Value) -> Step {
        self.tool_calls.push(ScriptedCall { id: id.map(str::to_string), name: name.to_string(), args });
        self
    }
    pub fn with_thinking(mut self, text: &str) -> Step {
        self.thinking = Some(text.to_string());
        self
    }
    pub fn with_stop(mut self, stop: ScriptedStop) -> Step {
        self.stop = Some(stop);
        self
    }
    pub fn expect_user(mut self, needle: &str) -> Step {
        self.expect_last_user_contains = Some(needle.to_string());
        self
    }
    pub fn expect_tool_result(mut self, needle: &str) -> Step {
        self.expect_tool_result_contains = Some(needle.to_string());
        self
    }
}

struct Cursor {
    next: usize,
    steps: VecDeque<Step>,
    calls: BTreeSet<String>,
}

pub struct ScriptedLlm {
    source: String,
    total: usize,
    instance: String,
    cursor: Mutex<Cursor>,
}

impl ScriptedLlm {
    pub fn from_steps(steps: Vec<Step>) -> ScriptedLlm {
        ScriptedLlm::new(steps, "inline steps")
    }

    /// Parses a script document; `source` names it in error messages (e.g. the file path).
    pub fn from_json(text: &str, source: &str) -> Result<ScriptedLlm, String> {
        let script: Script = serde_json::from_str(text).map_err(|e| format!("the model script {source} is not valid ({e})"))?;
        Ok(ScriptedLlm::new(script.steps, source))
    }

    fn new(steps: Vec<Step>, source: &str) -> ScriptedLlm {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        ScriptedLlm {
            source: source.to_string(),
            total: steps.len(),
            instance: format!("{}-{at}-{nonce}", std::process::id()),
            cursor: Mutex::new(Cursor { next: 1, steps: steps.into(), calls: BTreeSet::new() }),
        }
    }

    /// Steps not yet consumed.
    pub fn remaining(&self) -> usize {
        self.cursor.lock().unwrap_or_else(PoisonError::into_inner).steps.len()
    }

    fn take(&self, req: &Request) -> Result<(usize, Step), LlmError> {
        // The cursor is never left half-updated, so a poisoned lock still holds a valid script.
        let mut c = self.cursor.lock().unwrap_or_else(PoisonError::into_inner);
        let n = c.next;
        for block in req.messages.iter().flat_map(|m| &m.content) {
            if let ContentBlock::ToolUse { id, .. } | ContentBlock::ToolResult { id, .. } = block
                && !c.calls.contains(&id.0)
            {
                c.calls.insert(id.0.clone());
            }
        }
        c.next += 1;
        match c.steps.pop_front() {
            Some(step) => Ok((n, step)),
            None => Err(LlmError::Fatal(format!(
                "scripted model: request {n} has no step — the script ({}) has {} step(s) and all were consumed; last user message: {:?}. Add a step or end the conversation sooner",
                self.source,
                self.total,
                last_user_text(&req.messages).unwrap_or_default()
            ))),
        }
    }

    fn call_id(&self, explicit: Option<String>, step: usize, index: usize) -> CallId {
        let mut cursor = self.cursor.lock().unwrap_or_else(PoisonError::into_inner);
        let generated = || format!("scripted-{}-{step}-{index}", self.instance);
        let mut id = explicit.unwrap_or_else(generated);
        if cursor.calls.contains(&id) {
            id = generated();
        }
        // Explicit script ids may name another call's generated spelling.
        while !cursor.calls.insert(id.clone()) {
            id.push('_');
        }
        CallId(id)
    }

    fn respond(&self, req: Request, mut sink: Sink<'_>) -> Result<Response, LlmError> {
        let (n, step) = self.take(&req)?;
        if let Some(want) = &step.expect_last_user_contains {
            let got = last_user_text(&req.messages);
            if !got.as_deref().is_some_and(|g| g.contains(want.as_str())) {
                return Err(LlmError::Fatal(match got {
                    Some(g) => format!("scripted model: step {n} expected the last user message to contain {want:?}, but it was {g:?}"),
                    None => format!(
                        "scripted model: step {n} expected the last user message to contain {want:?}, but the request has no user text"
                    ),
                }));
            }
        }
        if let Some(want) = &step.expect_tool_result_contains {
            let got = last_tool_result(&req.messages);
            if !got.as_deref().is_some_and(|g| g.contains(want.as_str())) {
                return Err(LlmError::Fatal(match got {
                    Some(g) => format!("scripted model: step {n} expected the latest tool result to contain {want:?}, but it was {g:?}"),
                    None => format!(
                        "scripted model: step {n} expected the latest tool result to contain {want:?}, but the request carries no tool result"
                    ),
                }));
            }
        }
        if let Some(kind) = step.error {
            return Err(match kind {
                ScriptedError::ContextWindow => LlmError::ContextWindowExceeded { requested: estimate_request(&req), available: 0 },
                ScriptedError::RateLimited => LlmError::RateLimited { retry_after_ms: 0 },
                ScriptedError::Transient => LlmError::Transient(format!("scripted model: step {n} scripts a transient failure")),
            });
        }

        let mut content = Vec::new();
        let mut output_chars = 0usize;
        if let Some(t) = step.thinking.filter(|t| !t.is_empty()) {
            output_chars += t.chars().count();
            sink(StreamEvent::Thinking(t.clone()));
            content.push(ContentBlock::Thinking { text: t });
        }
        if let Some(t) = step.text.filter(|t| !t.is_empty()) {
            output_chars += t.chars().count();
            sink(StreamEvent::Text(t.clone()));
            content.push(ContentBlock::Text { text: t });
        }
        let has_calls = !step.tool_calls.is_empty();
        for (k, call) in step.tool_calls.into_iter().enumerate() {
            let id = self.call_id(call.id, n, k);
            let args = if call.args.is_null() { Value::Object(Default::default()) } else { call.args };
            output_chars += call.name.len() + args.to_string().len();
            sink(StreamEvent::ToolUseStart { id: id.clone(), name: call.name.clone() });
            sink(StreamEvent::ToolUseEnd { id: id.clone(), name: call.name.clone(), args: args.clone() });
            content.push(ContentBlock::ToolUse { id, name: call.name, args });
        }
        let usage =
            Usage { input_tokens: u64::from(estimate_request(&req)), output_tokens: output_chars.div_ceil(4) as u64, ..Usage::default() };
        sink(StreamEvent::Usage(usage));
        let stop = match step.stop {
            Some(ScriptedStop::EndTurn) => StopReason::EndTurn,
            Some(ScriptedStop::ToolUse) => StopReason::ToolUse,
            Some(ScriptedStop::MaxTokens) => StopReason::MaxTokens,
            None if has_calls => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        Ok(Response { content, usage, stop })
    }
}

fn last_user_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .find(|m| m.content.iter().any(|b| matches!(b, ContentBlock::Text { .. })))
        .map(|m| joined_text(&m.content))
}

fn last_tool_result(messages: &[Message]) -> Option<String> {
    messages.iter().rev().flat_map(|m| m.content.iter().rev()).find_map(|b| match b {
        ContentBlock::ToolResult { content, .. } => Some(joined_text(content)),
        _ => None,
    })
}

/// ~4 characters per token over everything the request carries; deterministic.
fn estimate_request(req: &Request) -> u32 {
    let mut chars: usize = req.system.iter().map(|(_, t, _)| t.chars().count()).sum();
    for b in req.messages.iter().flat_map(|m| &m.content) {
        chars += match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text } => text.chars().count(),
            ContentBlock::ToolUse { name, args, .. } => name.len() + args.to_string().len(),
            ContentBlock::ToolResult { content, .. } => joined_text(content).chars().count(),
            ContentBlock::Image { .. } | ContentBlock::ProviderMetadata { .. } => 0,
        };
    }
    u32::try_from(chars.div_ceil(4)).unwrap_or(u32::MAX)
}

impl Llm for ScriptedLlm {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { protocol: Protocol::OpenAiResponses, late_results: LateResults::SameCallId, route: ROUTE.to_string() }
    }

    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(async move { self.respond(req, sink) })
    }
}

#[derive(Clone, Debug)]
pub struct ScriptedConfig {
    pub script: String,
}

impl Config for ScriptedConfig {
    const PREFIX: &'static str = "provider";

    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            "provider.scripted.script",
            Ty::String,
            None,
            "Path to a JSON model script for the deterministic `scripted` route (system tests); relative paths resolve against the working directory.",
        )]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        match l.get("provider.scripted.script") {
            Some(Value::String(s)) => Ok(ScriptedConfig { script: s.clone() }),
            Some(_) => Err(ConfigError::Type { key: "provider.scripted.script".into(), expected: "a path string" }),
            None => Err(ConfigError::MissingKey("provider.scripted.script".into())),
        }
    }
}

/// The capability: registers `llm:scripted` from the script file.
pub struct Scripted {
    llm: Arc<ScriptedLlm>,
}

impl Capability for Scripted {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ScriptedConfig;

    #[allow(clippy::disallowed_methods, reason = "authority: scripted-read-script")]
    fn build(_: (), cfg: ScriptedConfig) -> Result<Self, BuildError> {
        let text = std::fs::read_to_string(&cfg.script).map_err(|e| {
            BuildError::Probe(format!(
                "could not read the model script {} ({e}); point `provider.scripted.script` at a JSON script",
                cfg.script
            ))
        })?;
        let llm = ScriptedLlm::from_json(&text, &cfg.script).map_err(BuildError::Probe)?;
        Ok(Scripted { llm: Arc::new(llm) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.service(aho_llm::service_name(ROUTE), LlmRoute(self.llm.clone()));
    }
}

declare_capability!(
    Scripted,
    "llm-scripted",
    kind = Kind::Provider,
    provides = [SeamId(aho_llm::SEAM)],
    requires = [Requirement::Setting { key: "provider.scripted.script", pred: Pred::Set }],
    tools = &[],
    doc = "Deterministic scripted model route `scripted` for system tests: one scripted step per request, expectations fail loudly (01)."
);
