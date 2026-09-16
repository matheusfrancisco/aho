//! `session_trace`: the causal chain of a call or an entry across every branch of the log (06 tree,
//! 11 rewinds). Links are found by the ids entries carry — `call`, `op`, `req`, checkpoint ids —
//! never by position, so a chain that crosses a fork is still one chain.

use std::collections::{BTreeMap, BTreeSet};

use aho_log::{CheckpointCause, Entry, EntryKind};
use aho_types::{CallId, CheckpointId, ContentBlock, EntryId, OpId, Value};
use serde_json::json;

use crate::query::{Out, Query, Tree, arg_id, call_of, ids, no_such};
use crate::view;

/// A chain being assembled: entry → why it is in the chain.
#[derive(Default)]
struct Chain {
    links: BTreeMap<EntryId, Vec<String>>,
}

impl Chain {
    fn add(&mut self, id: EntryId, note: impl Into<String>) {
        let note = note.into();
        let notes = self.links.entry(id).or_default();
        if !note.is_empty() && !notes.contains(&note) {
            notes.push(note);
        }
    }
}

pub fn trace(q: &Query<'_>, args: &Value) -> Out {
    let id = arg_id(args, "id")?;
    let call = match args.get("call") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(CallId(s.trim().to_string())),
        Some(v) => return Err(aho_cap::ExecOutcome::err("BAD_ARGS", format!("`call` must be a tool call id string; got {v}."))),
    };
    let tree = Tree::load(q.log)?;
    let (title, chain) = match (id, call) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(aho_cap::ExecOutcome::err(
                "BAD_ARGS",
                "Pass exactly one of `id` (an entry id) or `call` (a tool call id) to say what to trace.",
            ));
        }
        (None, Some(c)) => {
            let chain = call_chain(&tree, &c);
            if chain.links.is_empty() {
                return Err(aho_cap::ExecOutcome::err(
                    "NO_SUCH_CALL",
                    format!(
                        "No entry in this session log mentions call `{}`. Find call ids with session_search{{query, kinds: [\"tool_call\"]}}.",
                        q.red.text(&c.0)
                    ),
                ));
            }
            let tool = view::tools_by_call(tree.all.values()).remove(&c).unwrap_or_else(|| "?".into());
            (format!("call {} ({tool})", q.red.text(&c.0)), chain)
        }
        (Some(id), None) => {
            let Some(e) = tree.all.get(&id) else { return Err(no_such(id, &tree)) };
            entry_chain(&tree, e)
        }
    };
    Ok(render(q, &tree, &title, chain))
}

/// Which call an entry belongs to, following `op` and `req` references.
fn owning_calls(tree: &Tree, e: &Entry) -> Vec<CallId> {
    if let Some(c) = call_of(&e.kind) {
        return vec![c.clone()];
    }
    match &e.kind {
        EntryKind::OpDone { op, .. } => op_start(tree, *op, e.id).and_then(|s| call_of(&s.kind).cloned()).into_iter().collect(),
        EntryKind::ApprovalDecided { req, .. } => tree.all.get(req).and_then(|r| call_of(&r.kind).cloned()).into_iter().collect(),
        EntryKind::WorkspaceCheckpoint { cause: CheckpointCause::ToolResult(c), .. } => vec![c.clone()],
        EntryKind::AssistantMessage { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `Op/Started` for `op` on the branch of `at` (op ids are only unique along a branch).
fn op_start(tree: &Tree, op: OpId, at: EntryId) -> Option<&Entry> {
    ancestors(tree, at).find(|e| matches!(e.kind, EntryKind::OpStarted { op: o, .. } if o == op))
}

fn ancestors(tree: &Tree, from: EntryId) -> impl Iterator<Item = &Entry> {
    let mut cur = tree.all.get(&from);
    std::iter::from_fn(move || {
        let e = cur?;
        cur = e.parent.and_then(|p| tree.all.get(&p));
        Some(e)
    })
}

/// The checkpoint entry with id `cp` on the branch of `at`, nearest first.
fn checkpoint_on_branch(tree: &Tree, cp: CheckpointId, at: EntryId) -> Option<EntryId> {
    ancestors(tree, at).find(|e| matches!(e.kind, EntryKind::WorkspaceCheckpoint { id, .. } if id == cp)).map(|e| e.id)
}

/// The `PreRewind` checkpoint a `Workspace/Restored` names as `from`: taken on the old branch just
/// before the fork, so it is the nearest earlier `PreRewind` with that id anywhere in the file.
fn pre_rewind(tree: &Tree, cp: CheckpointId, restored: EntryId) -> Option<EntryId> {
    checkpoint_on_branch(tree, cp, restored).filter(|id| is_pre_rewind(tree, *id)).or_else(|| {
        tree.all
            .range(..restored)
            .rev()
            .find(|(id, e)| matches!(e.kind, EntryKind::WorkspaceCheckpoint { id: c, .. } if c == cp) && is_pre_rewind(tree, **id))
            .map(|(id, _)| *id)
    })
}

fn is_pre_rewind(tree: &Tree, id: EntryId) -> bool {
    tree.all.get(&id).is_some_and(|e| matches!(e.kind, EntryKind::WorkspaceCheckpoint { cause: CheckpointCause::PreRewind, .. }))
}

fn custom_mentions(body: &Value, call: &CallId) -> bool {
    match body {
        Value::Object(m) => ["call", "call_id", "callId"].iter().any(|k| m.get(*k).and_then(Value::as_str) == Some(call.0.as_str())),
        _ => false,
    }
}

fn call_chain(tree: &Tree, call: &CallId) -> Chain {
    let mut ch = Chain::default();
    let (mut reqs, mut ops, mut cps, mut tool_calls) = (BTreeSet::new(), Vec::new(), Vec::new(), Vec::new());
    // Ids ascend, and every reference points at an older entry: one pass sees causes before effects.
    for (id, e) in &tree.all {
        match &e.kind {
            EntryKind::AssistantMessage { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == call)) =>
            {
                ch.add(*id, "the model asked for it")
            }
            EntryKind::ToolCall { call: c, .. } if c == call => {
                tool_calls.push(*id);
                ch.add(*id, "")
            }
            EntryKind::ApprovalRequested { call: Some(c), .. } if c == call => {
                reqs.insert(*id);
                ch.add(*id, "")
            }
            EntryKind::ApprovalDecided { req, .. } if reqs.contains(req) => ch.add(*id, format!("answers #{}", req.0)),
            EntryKind::OpStarted { call: c, op, .. } if c == call => {
                ops.push((*op, *id));
                ch.add(*id, "")
            }
            EntryKind::OpDone { op, .. } if ops.iter().any(|(o, s)| o == op && tree.is_ancestor(*s, *id)) => ch.add(*id, ""),
            EntryKind::ToolResult { call: c, .. } if c == call => ch.add(*id, ""),
            EntryKind::WorkspaceCheckpoint { id: cp, cause: CheckpointCause::ToolResult(c), .. } if c == call => {
                cps.push((*cp, *id));
                ch.add(*id, "workspace after this call")
            }
            EntryKind::Custom { body, .. } if custom_mentions(body, call) => ch.add(*id, ""),
            EntryKind::WorkspaceRestored { to, .. } => {
                let target = checkpoint_on_branch(tree, *to, *id);
                if cps.iter().any(|(cp, at)| cp == to && Some(*at) == target) {
                    ch.add(*id, format!("rewound the workspace back to this call's checkpoint c{}", to.0));
                } else if let Some(t) = target {
                    let after_call = tool_calls.iter().any(|c| tree.is_ancestor(*c, *id));
                    let before_call = tool_calls.iter().any(|c| tree.is_ancestor(t, *c) && !tree.is_ancestor(*c, *id));
                    if after_call {
                        ch.add(*id, format!("rewound the workspace past this call, to c{}", to.0));
                    } else if before_call {
                        ch.add(*id, format!("forked away before this call: rewound to c{} (#{})", to.0, t.0));
                    }
                }
            }
            _ => {}
        }
    }
    ch
}

fn entry_chain(tree: &Tree, e: &Entry) -> (String, Chain) {
    let calls = owning_calls(tree, e);
    if !calls.is_empty() {
        let mut ch = Chain::default();
        for c in &calls {
            for (id, notes) in call_chain(tree, c).links {
                ch.links.entry(id).or_default().extend(notes);
            }
        }
        ch.add(e.id, "← traced entry");
        let names: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        return (format!("entry #{} (call {})", e.id.0, names.join(", ")), ch);
    }
    let mut ch = Chain::default();
    ch.add(e.id, "← traced entry");
    match &e.kind {
        EntryKind::WorkspaceCheckpoint { id: cp, cause, .. } => {
            for (id, x) in tree.all.range(e.id..) {
                if let EntryKind::WorkspaceRestored { to, from, .. } = &x.kind {
                    if to == cp && checkpoint_on_branch(tree, *to, *id) == Some(e.id) {
                        ch.add(*id, format!("rewound to this checkpoint c{}", cp.0));
                    }
                    if matches!(cause, CheckpointCause::PreRewind) && from == cp && pre_rewind(tree, *from, *id) == Some(e.id) {
                        ch.add(*id, "the rewind this checkpoint made reversible");
                    }
                }
            }
        }
        EntryKind::WorkspaceRestored { from, to, .. } => {
            if let Some(p) = pre_rewind(tree, *from, e.id) {
                ch.add(p, format!("workspace before the rewind (c{}); `aho rewind c{}` undoes it", from.0, from.0));
            }
            if let Some(t) = checkpoint_on_branch(tree, *to, e.id) {
                ch.add(t, format!("restored to c{}", to.0));
                if let Some(EntryKind::WorkspaceCheckpoint { cause: CheckpointCause::ToolResult(c), .. }) =
                    tree.all.get(&t).map(|x| &x.kind)
                    && let Some((id, _)) = tree.all.iter().find(|(_, x)| matches!(&x.kind, EntryKind::ToolCall { call, .. } if call == c))
                {
                    ch.add(*id, "the call that checkpoint followed");
                }
            }
        }
        EntryKind::ApprovalRequested { .. } => {
            for (id, x) in tree.all.range(e.id..) {
                if matches!(x.kind, EntryKind::ApprovalDecided { req, .. } if req == e.id) {
                    ch.add(*id, "the answer");
                }
            }
        }
        _ => {}
    }
    if let Some(p) = e.parent {
        ch.add(p, "parent");
    }
    for c in tree.children.get(&e.id).into_iter().flatten() {
        ch.add(*c, "child");
    }
    (format!("entry #{} ({})", e.id.0, view::label(&e.kind)), ch)
}

fn turn_of(tree: &Tree, id: EntryId) -> u64 {
    ancestors(tree, id)
        .find_map(|e| match e.kind {
            EntryKind::TurnStart { turn } => Some(turn.0),
            _ => None,
        })
        .unwrap_or(0)
}

fn render(q: &Query<'_>, tree: &Tree, title: &str, chain: Chain) -> aho_cap::ExecOutcome {
    let tools = view::tools_by_call(tree.all.values());
    let total = chain.links.len();
    let mut lines = vec![format!("Trace of {title}: {total} entr{} (heads: {}).", if total == 1 { "y" } else { "ies" }, tree.heads_text())];
    let mut out = Vec::new();
    for (id, notes) in chain.links.iter().take(q.lim.max_trace) {
        let Some(e) = tree.all.get(id) else { continue };
        let turn = turn_of(tree, *id);
        let summary = view::summary(&e.kind, &tools, q.red, crate::query::LINE_CHARS);
        let place = tree.where_(*id);
        let mut line = format!("#{} {} t{turn} [{place}] {summary}", id.0, view::label(&e.kind));
        if !notes.is_empty() {
            line.push_str(&format!(" — {}", notes.join("; ")));
        }
        let children = tree.children.get(id).cloned().unwrap_or_default();
        if children.len() > 1 {
            let forks: Vec<String> = children.iter().map(|c| format!("#{} ({})", c.0, tree.where_(*c))).collect();
            line.push_str(&format!("\n    fork point: branches continue at {}", forks.join(", ")));
        }
        lines.push(line);
        out.push(json!({
            "id": id.0, "kind": view::label(&e.kind), "turn": turn, "where": place, "on_current_branch": tree.current.contains(id),
            "notes": notes, "children": children.iter().map(|c| c.0).collect::<Vec<_>>(), "summary": summary,
        }));
    }
    if total > q.lim.max_trace {
        let rest: Vec<EntryId> = chain.links.keys().skip(q.lim.max_trace).copied().collect();
        lines.push(format!(
            "{} more linked entries not shown ({}…); read them with event_get.",
            rest.len(),
            ids(&rest[..rest.len().min(5)])
        ));
    }
    let value = json!({
        "title": title, "total": total, "heads": tree.heads.iter().map(|h| h.0).collect::<Vec<_>>(), "entries": out,
    });
    aho_cap::ExecOutcome::Done { value, content: vec![ContentBlock::text(lines.join("\n"))], meta: None }
}
