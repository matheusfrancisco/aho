//! `12` — model-facing long-term memory: `memory_search` and `memory_save` over the `memory`
//! handle (provided by `memory-local`).
//!
//! `memory_save` is classed `WorkspaceWrite`: the store is aho-owned state under
//! `~/.aho/memory`, not a workspace file, but saving is a durable write that outlives the
//! session, so the preset must admit writes. Ceilings derive from handles, so the tool holds
//! the world's `FsWrite` handle for that ceiling (and is only built where a world grants it);
//! it never writes through it.

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::*;
use aho_fs::FsWrite;
use aho_log::{EntryKind, OpKind};
use aho_memory::{Entry, MemoryHandle, Scope};
use aho_types::*;
use serde_json::json;

pub const DEFAULT_K: usize = 5;
pub const MAX_K: usize = 20;
/// Tag routing a saved memory to the store shared by every project.
pub const GLOBAL_TAG: &str = "global";
/// Provenance tag on every agent-saved memory.
pub const AGENT_TAG: &str = "agent";
/// Agent-saved facts start above the default and below operator `remember:` lines (0.8).
pub const SAVED_CONFIDENCE: f32 = 0.6;

fn search_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to look for: words, identifiers, file names, or a question."},
            "k": {"type": "integer", "minimum": 1, "maximum": MAX_K, "description": "Most results to return (default 5)."}
        },
        "required": ["query"],
        "additionalProperties": false
    }))
}

fn save_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "One self-contained fact, phrased so it makes sense in a future session."},
            "tags": {"type": "array", "items": {"type": "string"}, "description": "Short topic tags. Include `global` to share the fact with every project."}
        },
        "required": ["text"],
        "additionalProperties": false
    }))
}

fn arg_str<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn search_call(v: &Value) -> Card {
    Card::Generic { title: "memory_search".into(), body: arg_str(v, "query").to_string() }
}

fn search_result(args: &Value, r: &Value) -> Card {
    let hits = r["hits"].as_array().map(|a| a.iter().map(|h| format!("#{} {}", h["id"], arg_str(h, "text"))).collect()).unwrap_or_default();
    Card::Search { query: arg_str(args, "query").to_string(), hits }
}

fn save_call(v: &Value) -> Card {
    Card::Generic { title: "memory_save".into(), body: arg_str(v, "text").to_string() }
}

fn save_result(_: &Value, r: &Value) -> Card {
    let verb = if r["merged"].as_bool() == Some(true) { "merged into" } else { "saved as" };
    Card::Generic { title: "memory".into(), body: format!("{verb} #{}", r["id"]) }
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "memory_search",
        "Search long-term memory: facts saved in earlier sessions about this project and the operator (decisions, conventions, environment details, resolved problems). Ranks by exact terms and meaning together. Entries may be outdated; verify before relying on them.",
        Consequence::ReadOnly,
    )
    .schema(search_schema)
    .safe()
    .present(Present { call: search_call, result: search_result }),
    ToolDecl::new(
        "memory_save",
        "Save one durable fact to long-term memory so future sessions can recall it: a decision, convention, environment detail, or how a problem was solved. Stored by aho under ~/.aho/memory (aho-owned state, not a workspace file). A near-duplicate merges into the existing entry; a contradiction supersedes it. Tag `global` to share across projects.",
        Consequence::WorkspaceWrite,
    )
    .schema(save_schema)
    .op(OpKind::Io, true, false)
    .present(Present { call: save_call, result: save_result }),
];

/// Queued `Memory/*` entries (maintenance prunes, extractions, errors) in the session's
/// `value.events` shape, so they land in the log after this tool's result. `Cap/Delta`
/// activations are not a tool's to append; they stay queued.
fn forward_events(mem: &MemoryHandle, mut value: Value) -> Value {
    let events: Vec<Value> = mem
        .0
        .drain_events(&|e| matches!(e, EntryKind::Custom { .. }))
        .into_iter()
        .filter_map(|e| match e {
            EntryKind::Custom { ns, name, body } => Some(json!({ "ns": ns, "name": name, "body": body })),
            _ => None,
        })
        .collect();
    if !events.is_empty() {
        value["events"] = json!(events);
    }
    value
}

pub async fn search(mem: &MemoryHandle, args: &Value) -> ExecOutcome {
    let Some(query) = args.get("query").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()) else {
        return ExecOutcome::err("invalid_args", "`query` is required: the words, identifier, or question to look up");
    };
    let k = match args.get("k") {
        None | Some(Value::Null) => DEFAULT_K,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_K as u64).contains(&n) => n as usize,
            _ => return ExecOutcome::err("invalid_args", format!("`k` must be an integer from 1 to {MAX_K}")),
        },
    };
    let hits = match mem.0.search(query, k).await {
        Ok(h) => h,
        Err(e) => return ExecOutcome::err("memory_unavailable", format!("{e}; the search did not run")),
    };
    let listed: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "id": h.entry.id,
                "text": h.entry.text,
                "tags": h.entry.tags,
                "confidence": h.entry.confidence,
                "scope": h.entry.scope,
                "score": h.score,
            })
        })
        .collect();
    let value = forward_events(mem, json!({ "hits": listed }));
    let text = if hits.is_empty() {
        format!("No saved memory matches `{query}`.")
    } else {
        hits.iter()
            .map(|h| {
                let tags = if h.entry.tags.is_empty() { String::new() } else { format!(" [{}]", h.entry.tags.join(", ")) };
                format!("#{}{tags} (confidence {:.2}): {}", h.entry.id, h.entry.confidence, h.entry.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let meta = search_result(&json!({ "query": query }), &value);
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: Some(meta) }
}

pub async fn save(mem: &MemoryHandle, args: &Value) -> ExecOutcome {
    let Some(text) = args.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
        return ExecOutcome::err("invalid_args", "`text` is required: the fact to remember, as one self-contained sentence");
    };
    let mut tags: Vec<String> = match args.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => match a.iter().map(|t| t.as_str().map(|s| s.trim().to_string())).collect::<Option<Vec<_>>>() {
            Some(t) => t.into_iter().filter(|t| !t.is_empty()).collect(),
            None => return ExecOutcome::err("invalid_args", "`tags` must be a list of strings"),
        },
        Some(_) => return ExecOutcome::err("invalid_args", "`tags` must be a list of strings"),
    };
    let scope = if tags.iter().any(|t| t == GLOBAL_TAG) { Scope::Global } else { Scope::Project };
    if !tags.iter().any(|t| t == AGENT_TAG) {
        tags.push(AGENT_TAG.to_string());
    }
    let mut entry = Entry::new(text, tags);
    entry.scope = scope;
    entry.confidence = SAVED_CONFIDENCE;
    let up = match mem.0.upsert(entry).await {
        Ok(u) => u,
        Err(e) => return ExecOutcome::err("memory_write_failed", format!("{e}; nothing was saved")),
    };
    let mut msg = if up.merged {
        format!("Merged into existing memory #{} (the same fact was already stored).", up.id)
    } else {
        format!("Saved memory #{}.", up.id)
    };
    if !up.supersedes.is_empty() {
        let ids: Vec<String> = up.supersedes.iter().map(|i| format!("#{i}")).collect();
        msg.push_str(&format!(" It supersedes {}, which contradicted it.", ids.join(", ")));
    }
    let value = forward_events(mem, json!({ "id": up.id, "merged": up.merged, "supersedes": up.supersedes, "scope": scope }));
    let meta = save_result(&Value::Null, &value);
    ExecOutcome::Done { value, content: vec![ContentBlock::text(msg)], meta: Some(meta) }
}

pub struct MemoryToolDeps {
    pub memory: MemoryHandle,
}

impl Deps for MemoryToolDeps {
    /// `FsWrite` derives the `WorkspaceWrite` ceiling `memory_save` needs (see the crate doc).
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Custom("memory"), HandleKind::FsWrite];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        r.handle::<FsWrite>()?;
        Ok(MemoryToolDeps { memory: r.handle()? })
    }
}

pub struct ToolMemory {
    memory: MemoryHandle,
}

fn exec(mem: &MemoryHandle, f: for<'a> fn(&'a MemoryHandle, &'a Value) -> BoxFuture<'a, ExecOutcome>) -> ToolExec {
    let mem = mem.clone();
    Arc::new(move |req: ExecRequest| {
        let mem = mem.clone();
        Box::pin(async move {
            if req.cancel.is_cancelled() {
                return ExecOutcome::Cancelled;
            }
            f(&mem, &req.view.args).await
        })
    })
}

impl Capability for ToolMemory {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = MemoryToolDeps;
    type Config = ();

    fn build(deps: MemoryToolDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolMemory { memory: deps.memory })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(&TOOLS[0], exec(&self.memory, |m, a| Box::pin(search(m, a))));
        r.tool(&TOOLS[1], exec(&self.memory, |m, a| Box::pin(save(m, a))));
    }
}

declare_capability!(
    ToolMemory,
    "tool-memory",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_memory::SEAM)],
    tools = TOOLS,
    doc = "memory_search (ReadOnly) and memory_save (WorkspaceWrite; the store is aho-owned state under ~/.aho/memory) over the memory handle (12)."
);
