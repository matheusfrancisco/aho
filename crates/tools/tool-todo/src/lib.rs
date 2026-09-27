//! `todo_write`: the working agent's task list as whole-list snapshots on the session log.
//!
//! * The model sends the entire list every call (`todos: [{content, status}]`, status
//!   `pending | in_progress | completed`); the new list replaces the old, and the last write wins on
//!   replay. An empty list clears it.
//! * Invariants, checked by the tool and again by the fold: every `content` is trimmed and
//!   non-empty, contents are unique, at most one item is `in_progress`.
//! * One list per agent (`agent:<n>`); non-agent calls are refused.
//! * The model-facing result is one summary line (`2/5 done · in progress: write the tests`).
//!
//! # Contract with the session: `value.events`
//!
//! A successful call returns `ExecOutcome::Done { value: { "events": [ { "ns": "Todo", "name":
//! "Written", "body": { "by": "agent:<n>", "todos": [..] } } ], "todos": [..] }, … }`; the session
//! appends the event after the `ToolResult` as `EntryKind::Custom`. `register` installs
//! [`fold_todos`] for the `Todo` namespace; a snapshot that breaks an invariant fails the fold.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, ExecOutcome, ExecRequest, Manifest, Present, Registrar, ToolDecl, ToolExec,
    declare_capability,
};
use aho_types::{Card, Consequence, ContentBlock, Principal, RootSchema, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const NS: &str = "Todo";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// `pending`, `in_progress` (also `in-progress`, `In Progress`), `completed`.
    pub fn parse(s: &str) -> Option<TodoStatus> {
        match s.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodoError {
    NotAList,
    BadItem(usize),
    Empty(usize),
    Untrimmed(usize),
    BadStatus { index: usize, got: String },
    Duplicate(String),
    SeveralInProgress(Vec<String>),
    NotAnAgent(String),
    BadEvent(String),
}

impl fmt::Display for TodoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TodoError::NotAList => write!(f, "`todos` must be the whole list: an array of {{content, status}}; nothing was saved"),
            TodoError::BadItem(i) => write!(f, "item {} is not an object with `content` and `status`; nothing was saved", i + 1),
            TodoError::Empty(i) => write!(f, "item {} has empty content; nothing was saved. Describe the task or drop the item", i + 1),
            TodoError::Untrimmed(i) => write!(f, "item {} has surrounding whitespace in its content", i + 1),
            TodoError::BadStatus { index, got } => {
                write!(f, "item {} has status `{got}`; nothing was saved. Use pending, in_progress or completed", index + 1)
            }
            TodoError::Duplicate(c) => write!(f, "\"{c}\" appears twice; nothing was saved. Each item must be unique"),
            TodoError::SeveralInProgress(items) => write!(
                f,
                "{} items are in_progress ({}); nothing was saved. Keep exactly one item in progress and mark the others pending",
                items.len(),
                items.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
            ),
            TodoError::NotAnAgent(who) => write!(f, "todo_write keeps a working agent's own list; this call came from {who}"),
            TodoError::BadEvent(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for TodoError {}

/// Invariants of one snapshot: trimmed, non-empty, unique, at most one `in_progress`.
pub fn check(items: &[Todo]) -> Result<(), TodoError> {
    let mut seen = BTreeSet::new();
    for (i, t) in items.iter().enumerate() {
        if t.content.trim().is_empty() {
            return Err(TodoError::Empty(i));
        }
        if t.content.trim() != t.content {
            return Err(TodoError::Untrimmed(i));
        }
        if !seen.insert(t.content.as_str()) {
            return Err(TodoError::Duplicate(t.content.clone()));
        }
    }
    let active: Vec<String> = items.iter().filter(|t| t.status == TodoStatus::InProgress).map(|t| t.content.clone()).collect();
    if active.len() > 1 {
        return Err(TodoError::SeveralInProgress(active));
    }
    Ok(())
}

/// Model arguments → a checked snapshot (contents trimmed).
pub fn parse(args: &Value) -> Result<Vec<Todo>, TodoError> {
    let Some(items) = args.get("todos").and_then(Value::as_array) else { return Err(TodoError::NotAList) };
    let mut out = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        let (Some(content), Some(status)) = (it.get("content").and_then(Value::as_str), it.get("status").and_then(Value::as_str)) else {
            return Err(TodoError::BadItem(i));
        };
        let status = TodoStatus::parse(status).ok_or_else(|| TodoError::BadStatus { index: i, got: status.to_string() })?;
        out.push(Todo { content: content.trim().to_string(), status });
    }
    check(&out)?;
    Ok(out)
}

/// `2/5 done · in progress: write the tests`: the one line the model gets back.
pub fn summary(items: &[Todo]) -> String {
    if items.is_empty() {
        return "Todo list cleared.".into();
    }
    let done = items.iter().filter(|t| t.status == TodoStatus::Completed).count();
    let head = format!("{done}/{} done", items.len());
    if let Some(t) = items.iter().find(|t| t.status == TodoStatus::InProgress) {
        return format!("{head} · in progress: {}", t.content);
    }
    match items.iter().find(|t| t.status == TodoStatus::Pending) {
        Some(t) => format!("{head} · nothing in progress · next: {}", t.content),
        None => format!("{head} · all done"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "body")]
pub enum Event {
    /// A whole-list snapshot replacing `by`'s list.
    Written { by: String, todos: Vec<Todo> },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Written { .. } => "Written",
        }
    }
    pub fn body(&self) -> Result<Value, TodoError> {
        let mut v = serde_json::to_value(self).map_err(|e| TodoError::BadEvent(format!("Todo/Written did not serialize: {e}")))?;
        Ok(v.get_mut("body").map(Value::take).unwrap_or(Value::Null))
    }
    /// `{ "ns": "Todo", "name": "Written", "body": .. }`: the shape returned under `value.events`.
    pub fn to_json(&self) -> Result<Value, TodoError> {
        Ok(json!({ "ns": NS, "name": self.name(), "body": self.body()? }))
    }
}

/// The projection: each agent's latest list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todos {
    #[serde(default)]
    lists: BTreeMap<String, Vec<Todo>>,
}

impl Todos {
    pub fn list(&self, agent: &str) -> &[Todo] {
        self.lists.get(agent).map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn apply(&mut self, e: &Event) -> Result<(), TodoError> {
        match e {
            Event::Written { by, todos } => {
                if !is_agent_label(by) {
                    return Err(TodoError::NotAnAgent(by.clone()));
                }
                check(todos)?;
                self.lists.insert(by.clone(), todos.clone());
            }
        }
        Ok(())
    }
}

fn is_agent_label(s: &str) -> bool {
    s.strip_prefix("agent:").is_some_and(|n| n.parse::<u128>().is_ok())
}

/// The `CustomFold` for `Todo/*`.
pub fn fold_todos(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut todos: Todos = match prev {
        None => Todos::default(),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the todo projection is unreadable: {e}"))?,
    };
    if name != "Written" {
        return Err(format!("unknown todo event `Todo/{name}`"));
    }
    let e: Event = serde_json::from_value(json!({"name": name, "body": body})).map_err(|e| format!("malformed Todo/{name}: {e}"))?;
    todos.apply(&e).map_err(|e| e.to_string())?;
    serde_json::to_value(&todos).map_err(|e| format!("the todo projection did not serialize: {e}"))
}

// ───────────────────────────── the tool ─────────────────────────────

pub fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "description": "The whole list, every call; it replaces the previous one.",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    },
                    "required": ["content", "status"]
                }
            }
        },
        "required": ["todos"]
    }))
}

fn present_call(args: &Value) -> Card {
    let body = match parse(args) {
        Ok(items) => summary(&items),
        Err(e) => e.to_string(),
    };
    Card::Generic { title: "Todos".into(), body }
}

fn present_result(args: &Value, _: &Value) -> Card {
    present_call(args)
}

pub static TOOLS: &[ToolDecl] = &[ToolDecl::new(
    "todo_write",
    "Keep your task list for multi-step work: send the whole list every time (it replaces the previous one), each item \
     {content, status} with status pending, in_progress or completed. Keep exactly one item in_progress while working, mark items \
     completed as soon as they are done, and do not use it for single-step work.",
    Consequence::ReadOnly,
)
.schema(schema)
.present(Present { call: present_call, result: present_result })];

pub struct ToolTodo;

impl Capability for ToolTodo {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();

    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(ToolTodo)
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_todos);
        let exec: ToolExec = Arc::new(|req: ExecRequest| {
            let out = write(&req.view.args, &req.view.by);
            Box::pin(async move { out })
        });
        r.tool(&TOOLS[0], exec);
    }
}

declare_capability!(
    ToolTodo,
    "tool-todo",
    kind = aho_cap::Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    entry_kinds = ["Todo/Written"],
    doc = "todo_write: the working agent's whole-list task snapshots, invariants checked in the fold."
);

/// The tool body: validate, then hand back the snapshot event and the summary line.
pub fn write(args: &Value, by: &Principal) -> ExecOutcome {
    let Principal::Agent { id, .. } = by else {
        let who = match by {
            Principal::Operator => "the operator".to_string(),
            Principal::Engineer => "the engineer".to_string(),
            Principal::System { name } => format!("system `{name}`"),
            Principal::Agent { .. } => "an agent".to_string(),
        };
        return ExecOutcome::err("not_allowed", TodoError::NotAnAgent(who).to_string());
    };
    let todos = match parse(args) {
        Ok(t) => t,
        Err(e) => return ExecOutcome::err("bad_todos", e.to_string()),
    };
    let line = summary(&todos);
    let event = Event::Written { by: format!("agent:{}", id.0), todos: todos.clone() };
    match (event.to_json(), serde_json::to_value(&todos)) {
        (Ok(ev), Ok(list)) => {
            ExecOutcome::Done { value: json!({ "events": [ev], "todos": list }), content: vec![ContentBlock::text(line)], meta: None }
        }
        (Err(e), _) => ExecOutcome::err("bad_todos", e.to_string()),
        (_, Err(e)) => ExecOutcome::err("bad_todos", e.to_string()),
    }
}

#[cfg(test)]
mod tests;
