//! Input schemas and argument decoding for the task tools.

use aho_cap::BoundWorld;
use aho_tasks::{ChildSpec, Injection, Kind, SeedSpec, TaskError, TaskId, parse_agent};
use aho_types::{AgentId, RootSchema, Value};
use serde_json::{Map, json};

pub(crate) type Args<'a> = &'a Map<String, Value>;

pub(crate) fn object(args: &Value) -> Result<Args<'_>, String> {
    args.as_object().ok_or_else(|| "the arguments must be a JSON object".to_string())
}

pub(crate) fn req_str(o: Args<'_>, key: &str) -> Result<String, String> {
    match o.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(format!("`{key}` is required and must be a non-empty string")),
    }
}

pub(crate) fn opt_str(o: Args<'_>, key: &str) -> Option<String> {
    match o.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

pub(crate) fn id_list(o: Args<'_>, key: &str) -> Result<Vec<TaskId>, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(vec![]),
        Some(Value::String(s)) => Ok(vec![TaskId::new(s.trim())]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(|s| TaskId::new(s.trim())).ok_or_else(|| format!("`{key}` must list task ids as strings")))
            .collect(),
        Some(_) => Err(format!("`{key}` must be a list of task ids")),
    }
}

pub(crate) fn str_list(o: Args<'_>, key: &str) -> Vec<String> {
    match o.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => vec![],
    }
}

pub(crate) fn agent(o: Args<'_>, key: &str) -> Result<Option<AgentId>, String> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => parse_agent(s).map(Some).ok_or_else(|| format!("`{key}` must name an agent as `agent:<n>`")),
        Some(Value::Number(n)) => n.as_u64().map(|n| Some(AgentId(n as u128))).ok_or_else(|| format!("`{key}` must be an agent id")),
        Some(_) => Err(format!("`{key}` must name an agent as `agent:<n>`")),
    }
}

fn items<'a>(o: Args<'a>, key: &str) -> Result<Vec<Args<'a>>, String> {
    match o.get(key) {
        Some(Value::Array(a)) if !a.is_empty() => {
            a.iter().map(|x| x.as_object().ok_or_else(|| format!("every entry of `{key}` must be an object"))).collect()
        }
        _ => Err(format!("`{key}` must be a non-empty list")),
    }
}

fn kind(o: Args<'_>) -> Result<Kind, String> {
    Kind::parse_work(&req_str(o, "kind")?).map_err(|e: TaskError| e.to_string())
}

pub(crate) fn seeds(o: Args<'_>) -> Result<Vec<SeedSpec>, String> {
    items(o, "tasks")?
        .into_iter()
        .map(|t| {
            Ok(SeedSpec {
                id: TaskId::new(req_str(t, "id")?),
                content: req_str(t, "content")?,
                kind: kind(t)?,
                depends_on: id_list(t, "depends_on")?,
                parent: opt_str(t, "parent").map(TaskId::new),
            })
        })
        .collect()
}

pub(crate) fn children(o: Args<'_>) -> Result<Vec<ChildSpec>, String> {
    items(o, "children")?
        .into_iter()
        .map(|t| {
            Ok(ChildSpec {
                id: TaskId::new(req_str(t, "id")?),
                content: req_str(t, "content")?,
                kind: kind(t)?,
                depends_on: id_list(t, "depends_on")?,
            })
        })
        .collect()
}

pub(crate) fn injections(o: Args<'_>) -> Result<Vec<Injection>, String> {
    items(o, "inject")?
        .into_iter()
        .map(|t| {
            Ok(Injection {
                id: TaskId::new(req_str(t, "id")?),
                content: req_str(t, "content")?,
                kind: kind(t)?,
                depends_on: id_list(t, "depends_on")?,
                addresses: id_list(t, "addresses")?,
            })
        })
        .collect()
}

// ───────────────────────────── schemas ─────────────────────────────

fn ids_schema(desc: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": desc})
}

fn kind_schema() -> Value {
    json!({"type": "string", "enum": ["explore", "code", "synthesize"]})
}

fn task_item(with_parent: bool) -> Value {
    let mut props = json!({
        "id": {"type": "string", "description": "Stable id you choose (letters, digits, . _ : / -)."},
        "content": {"type": "string", "description": "What the worker should do."},
        "kind": kind_schema(),
        "depends_on": ids_schema("Tasks that must complete first; their artifacts become this task's inputs."),
    });
    if with_parent {
        props["parent"] = json!({"type": "string", "description": "An already-expanded task whose scope this fills (a gap)."});
    }
    json!({"type": "object", "properties": props, "required": ["id", "content", "kind"]})
}

pub(crate) fn artifact_schema() -> Value {
    let list = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    json!({
        "type": "object",
        "properties": {
            "findings": {"type": "string", "description": "What you found or built."},
            "evidence": list("Files, lines, command output backing the findings."),
            "edge_cases": list("Edge cases you considered."),
            "validation": {"type": "string", "description": "How you validated it."},
            "open_questions": list("What remains open."),
            "confidence": {"type": "string", "description": "low | medium | high (a 0-1 score or `7/10` also reads)."},
            "what_i_did_not_check": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                "description": "Required, never empty: what you did not verify."},
            "covers": ids_schema("Gates only: every task in the gate's scope."),
            "contract": {
                "type": "object",
                "description": "The edge as an interface. Required for code/synthesize tasks in deep mode.",
                "properties": {
                    "outputs": {"type": "array", "items": {"type": "object", "properties": {
                        "path": {"type": "string"}, "format": {"type": "string"},
                        "hash": {"type": "string", "description": "64-hex blake3 digest, optional."}},
                        "required": ["path", "format"]}},
                    "units": list("e.g. lengths in mm, timestamps UTC."),
                    "version": {"type": "string"},
                    "acceptance": list("Checks the dependent runs before trusting the outputs; a failure re-opens you."),
                },
                "required": ["outputs", "acceptance"]
            }
        },
        "required": ["findings", "confidence", "what_i_did_not_check"]
    })
}

pub fn schema_seed(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"tasks": {"type": "array", "minItems": 1, "items": task_item(true)}},
        "required": ["tasks"]
    }))
}

pub fn schema_expand(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "The task you are holding."},
            "children": {"type": "array", "minItems": 1, "items": task_item(false)}
        },
        "required": ["id", "children"]
    }))
}

pub fn schema_claim(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {}}))
}

pub fn schema_complete(_: &BoundWorld) -> RootSchema {
    let mut inject = task_item(false);
    inject["properties"]["addresses"] = ids_schema("In-scope tasks this work follows up (a LOW-confidence sibling is addressed this way).");
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "The task you are holding."},
            "artifact": artifact_schema(),
            "inject": {"type": "array", "minItems": 1, "items": inject,
                "description": "Gates only, instead of `artifact`: turn misses into new work."}
        },
        "required": ["id"]
    }))
}

pub fn schema_checkpoint(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "summary": {"type": "string", "description": "Progress so far, enough for a replacement to pick up."},
            "recent": {"type": "array", "items": {"type": "string"}, "description": "Short summaries of your last steps."}
        },
        "required": ["id", "summary"]
    }))
}

pub fn schema_status(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"id": {"type": "string", "description": "A task to show in full (artifact included); omit for the whole graph."}}
    }))
}

pub fn schema_control(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "op": {"type": "string", "enum": ["stop", "reopen", "retry", "reassign", "replace", "salvage"]},
            "to": {"type": "string", "description": "reassign/salvage: the agent to hand the task to, `agent:<n>`."},
            "consumer": {"type": "string", "description": "reopen: the task you hold whose acceptance check failed (default: your task)."},
            "reason": {"type": "string", "description": "reopen: the failed check and its output; others: why."}
        },
        "required": ["id", "op"]
    }))
}
