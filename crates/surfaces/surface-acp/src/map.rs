//! aho → ACP projections: live stream events and durable log entries become `session/update`s;
//! ACP prompts become one prompt text; a finished turn becomes a `stopReason`.
//!
//! One projection serves both the live stream and `session/load` replay. Live, assistant text is
//! sent as it streams and the durable `Assistant/Message` only fills in what did not stream;
//! replayed, the durable entries are the whole story.

use std::collections::BTreeSet;

use aho_acp::RpcError;
use aho_acp::schema::{
    ContentBlock, ContentChunk, EmbeddedResourceResource, ImageContent, McpServer, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use aho_llm::StreamEvent;
use aho_log::{Entry, EntryKind, ToolOutcome, Transition};
use aho_types::{CallId, Consequence, ContentBlock as Block, EntryId, Form, Principal, StopReason as AhoStop, Value};

/// Projection state for one ACP session's update stream.
pub struct Mapper {
    workspace: String,
    /// `messageId` of the assistant message being streamed: `m<entry id of its Request/Header>`.
    message: Option<String>,
    streamed_text: bool,
    streamed_thought: bool,
    /// Approval requests already put to the client (`Approval/Requested` entry ids).
    pub(crate) asked: BTreeSet<EntryId>,
    pub(crate) synthetic: u64,
}

impl Mapper {
    pub fn new(workspace: impl Into<String>) -> Mapper {
        Mapper {
            workspace: workspace.into(),
            message: None,
            streamed_text: false,
            streamed_thought: false,
            asked: BTreeSet::new(),
            synthetic: 0,
        }
    }

    pub fn stream(&mut self, ev: &StreamEvent) -> Vec<SessionUpdate> {
        match ev {
            StreamEvent::Text(t) if !t.is_empty() => {
                self.streamed_text = true;
                vec![SessionUpdate::AgentMessageChunk(ContentChunk::text(t.clone(), self.message.clone()))]
            }
            StreamEvent::Thinking(t) if !t.is_empty() => {
                self.streamed_thought = true;
                vec![SessionUpdate::AgentThoughtChunk(ContentChunk::text(t.clone(), self.message.clone()))]
            }
            // Tool calls are announced from their durable `Tool/Call`, which carries the class.
            _ => Vec::new(),
        }
    }

    /// `replay`: the entry comes from `session/load`, not from the live stream.
    pub fn entry(&mut self, e: &Entry, replay: bool) -> Vec<SessionUpdate> {
        match &e.kind {
            EntryKind::RequestHeader { .. } => {
                self.message = Some(format!("m{}", e.id.0));
                self.streamed_text = false;
                self.streamed_thought = false;
                Vec::new()
            }
            // Live, the client already has what it sent; replayed, it needs its own turns back.
            EntryKind::UserMessage { form: Form::Prompt, text, .. } if replay && e.by == Principal::Operator => {
                vec![SessionUpdate::UserMessageChunk(ContentChunk::text(text.clone(), Some(format!("u{}", e.id.0))))]
            }
            EntryKind::AssistantMessage { content, .. } => {
                let mut out = Vec::new();
                for b in content {
                    match b {
                        Block::Thinking { text } if !text.is_empty() && (replay || !self.streamed_thought) => {
                            out.push(SessionUpdate::AgentThoughtChunk(ContentChunk::text(text.clone(), self.message.clone())));
                        }
                        Block::Text { text } if !text.is_empty() && (replay || !self.streamed_text) => {
                            out.push(SessionUpdate::AgentMessageChunk(ContentChunk::text(text.clone(), self.message.clone())));
                        }
                        _ => {}
                    }
                }
                out
            }
            EntryKind::ToolCall { call, tool, args, class } => vec![SessionUpdate::ToolCall(self.tool_call(call, tool, args, *class))],
            EntryKind::OpStarted { call, .. } => vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: call.0.clone(),
                status: Some(ToolCallStatus::InProgress),
                ..ToolCallUpdate::default()
            })],
            EntryKind::ToolResult { call, outcome, value, content, .. } => {
                let status = if matches!(outcome, ToolOutcome::Ok) { ToolCallStatus::Completed } else { ToolCallStatus::Failed };
                vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: call.0.clone(),
                    status: Some(status),
                    content: Some(tool_content(content)),
                    raw_output: (!value.is_null()).then(|| value.clone()),
                    ..ToolCallUpdate::default()
                })]
            }
            EntryKind::Custom { ns, name, body } if ns == "Todo" && name == "Written" => plan(body).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    pub fn tool_call(&self, call: &CallId, tool: &str, args: &Value, class: Consequence) -> ToolCall {
        ToolCall {
            tool_call_id: call.0.clone(),
            title: title_of(tool, args),
            name: Some(tool.to_string()),
            kind: kind_of(class, tool),
            status: ToolCallStatus::Pending,
            content: Vec::new(),
            locations: self.locations(args),
            raw_input: Some(args.clone()),
            raw_output: None,
            meta: None,
        }
    }

    pub fn locations(&self, args: &Value) -> Vec<ToolCallLocation> {
        ["path", "file_path"]
            .iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .filter(|p| !p.is_empty())
            .map(|p| {
                let path = if p.starts_with('/') || self.workspace.is_empty() {
                    p.to_string()
                } else {
                    format!("{}/{}", self.workspace.trim_end_matches('/'), p.trim_start_matches("./"))
                };
                vec![ToolCallLocation { path, line: args.get("line").and_then(Value::as_u64).and_then(|l| u32::try_from(l).ok()) }]
            })
            .unwrap_or_default()
    }
}

fn tool_content(content: &[Block]) -> Vec<ToolCallContent> {
    content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(ToolCallContent::text(text.clone())),
            Block::Image { media_type, data } => Some(ToolCallContent::Content {
                content: ContentBlock::Image(ImageContent {
                    data: aho_llm::base64(data),
                    mime_type: media_type.clone(),
                    uri: None,
                    annotations: None,
                    meta: None,
                }),
            }),
            _ => None,
        })
        .collect()
}

/// `Todo/Written` (tool-todo) is the agent's plan: `{todos: [{content, status}]}` → `plan`.
fn plan(body: &Value) -> Option<SessionUpdate> {
    let todos = body.get("todos")?.as_array()?;
    let entries = todos
        .iter()
        .filter_map(|t| {
            let status = match t.get("status")?.as_str()? {
                "pending" => PlanEntryStatus::Pending,
                "in_progress" => PlanEntryStatus::InProgress,
                "completed" => PlanEntryStatus::Completed,
                _ => return None,
            };
            Some(PlanEntry { content: t.get("content")?.as_str()?.to_string(), priority: PlanEntryPriority::Medium, status, meta: None })
        })
        .collect();
    Some(SessionUpdate::Plan(Plan { entries, meta: None }))
}

fn tokens(name: &str) -> impl Iterator<Item = &str> {
    name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty())
}

/// The ACP `kind` of an aho tool: from its consequence class, refined by its name.
pub fn kind_of(class: Consequence, tool: &str) -> ToolKind {
    let has = |words: &[&str]| tokens(tool).any(|t| words.iter().any(|w| t.eq_ignore_ascii_case(w)));
    match class {
        Consequence::WorkspaceWrite | Consequence::HostWrite => {
            if has(&["delete", "remove", "rm", "unlink"]) {
                ToolKind::Delete
            } else if has(&["move", "rename", "mv"]) {
                ToolKind::Move
            } else {
                ToolKind::Edit
            }
        }
        Consequence::SpawnsProcess => ToolKind::Execute,
        Consequence::NetworkEgress => ToolKind::Fetch,
        Consequence::ReadOnly => {
            if has(&["grep", "glob", "search", "find"]) {
                ToolKind::Search
            } else if has(&["todo", "plan", "think"]) {
                ToolKind::Think
            } else {
                ToolKind::Read
            }
        }
        Consequence::ControlsVM | Consequence::ControlsDesktop | Consequence::TalksToHuman => ToolKind::Other,
    }
}

/// `bash: cargo test`, `read: src/lib.rs`, or the bare tool name.
pub fn title_of(tool: &str, args: &Value) -> String {
    const KEYS: [&str; 8] = ["command", "cmd", "path", "file_path", "pattern", "url", "query", "prompt"];
    let detail = KEYS.iter().find_map(|k| args.get(*k).and_then(Value::as_str)).map(|s| one_line(s, 80)).filter(|s| !s.is_empty());
    match detail {
        Some(d) => format!("{tool}: {d}"),
        None => tool.to_string(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// One prompt text from ACP content. Text and resource links are the protocol baseline;
/// embedded text resources are inlined (`embeddedContext`); images and audio are refused because
/// the initialize response says `image: false, audio: false`.
pub fn prompt_text(blocks: &[ContentBlock]) -> Result<String, RpcError> {
    let mut parts = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::ResourceLink(l) => {
                let label = l.title.as_deref().unwrap_or(&l.name);
                let about = l.description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default();
                parts.push(format!("[resource: {label} <{}>{about}]", l.uri));
            }
            ContentBlock::Resource(r) => match &r.resource {
                EmbeddedResourceResource::Text(t) => parts.push(format!("[resource {}]\n{}\n[end of resource {}]", t.uri, t.text, t.uri)),
                EmbeddedResourceResource::Blob(bl) => parts.push(format!(
                    "[binary resource {} ({}), {} base64 characters, not included]",
                    bl.uri,
                    bl.mime_type.as_deref().unwrap_or("unknown type"),
                    bl.blob.len()
                )),
            },
            ContentBlock::Image(_) | ContentBlock::Audio(_) => {
                return Err(RpcError::invalid_params(
                    "aho takes text prompts: the initialize response advertised image: false and audio: false. Send text, a resource link, or an embedded text resource instead.",
                ));
            }
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Err(RpcError::invalid_params("the prompt is empty; nothing was sent to the model. Include at least one text block."));
    }
    Ok(text)
}

/// How a finished turn reads to the client. A turn the client cancelled is `cancelled` whatever
/// the loop reported (prompt-turn §Cancellation); an aborted turn is a JSON-RPC error, because
/// ACP has no error stop reason.
pub fn stop_reason(t: &Transition, cancelled: bool, last: Option<AhoStop>) -> Result<StopReason, String> {
    if cancelled {
        return Ok(StopReason::Cancelled);
    }
    match t {
        Transition::Completed => Ok(match last {
            Some(AhoStop::MaxTokens) => StopReason::MaxTokens,
            Some(AhoStop::Refusal) => StopReason::Refusal,
            _ => StopReason::EndTurn,
        }),
        Transition::MaxTurns => Ok(StopReason::MaxTurnRequests),
        Transition::StopHookBlocking => Ok(StopReason::Refusal),
        Transition::Aborted(reason) => {
            Err(format!("the turn stopped with an error: {reason}. The session log has the details (`aho session show`)."))
        }
        _ => Ok(StopReason::EndTurn),
    }
}

/// `mcp.servers.<name>.*` config sets (`--set` syntax: TOML values) for the stdio MCP servers a
/// client passes to `session/new` / `session/load`. aho speaks MCP over stdio only (the initialize
/// response says `http: false, sse: false`); without an operator-set `class` the servers start
/// quarantined like any other (10). Env values land in the session log like any `--set`.
pub fn mcp_sets(servers: &[McpServer]) -> Result<Vec<(String, String)>, String> {
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut out = Vec::new();
    for s in servers {
        let stdio = match s {
            McpServer::Stdio(x) => x,
            McpServer::Http(h) | McpServer::Sse(h) => {
                return Err(format!(
                    "MCP server `{}` uses an HTTP transport; aho connects to MCP servers over stdio only (mcpCapabilities http/sse are false). Nothing was started.",
                    h.name
                ));
            }
        };
        let valid = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid(&stdio.name) {
            return Err(format!(
                "MCP server name `{}` cannot be a config key; use letters, digits, `-` and `_` only. Nothing was started.",
                stdio.name
            ));
        }
        let key = format!("mcp.servers.{}", stdio.name);
        out.push((format!("{key}.cmd"), quote(&stdio.command)));
        out.push((format!("{key}.args"), serde_json::to_string(&stdio.args).unwrap_or_else(|_| "[]".into())));
        for v in &stdio.env {
            if !valid(&v.name) {
                return Err(format!(
                    "environment variable `{}` for MCP server `{}` cannot be a config key. Nothing was started.",
                    v.name, stdio.name
                ));
            }
            out.push((format!("{key}.env.{}", v.name), quote(&v.value)));
        }
    }
    Ok(out)
}
