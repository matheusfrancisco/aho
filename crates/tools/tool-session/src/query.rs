//! `session_search`, `event_list`, `event_get`, and the branch tree they share with `session_trace`.

use std::collections::{BTreeMap, BTreeSet};

use aho_cap::ExecOutcome;
use aho_log::{Entry, EntryKind, LogStore};
use aho_types::{CallId, ContentBlock, EntryId, Value};
use serde_json::json;

use crate::view::{self, DEFAULT_SEARCH, KindFilter, Redactor, Row};

/// Caps from `session_query.*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_hits: usize,
    pub snippet_chars: usize,
    pub max_window: usize,
    pub max_trace: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_hits: 20, snippet_chars: 200, max_window: 50, max_trace: 100 }
    }
}

/// Longest query accepted, in chars.
pub const MAX_QUERY: usize = 1000;
/// Characters of a one-line summary in lists and traces.
pub const LINE_CHARS: usize = 200;
/// Regex compile budget (bytes), so a hostile pattern cannot exhaust memory.
const REGEX_LIMIT: usize = 1 << 20;

pub type Out = Result<ExecOutcome, ExecOutcome>;

pub struct Query<'a> {
    pub log: &'a dyn LogStore,
    pub red: &'a Redactor,
    pub lim: &'a Limits,
}

fn bad(code: &str, msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err(code, msg)
}

fn done(value: Value, text: String) -> Out {
    Ok(ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None })
}

// ───────────────────────────────────── args ─────────────────────────────────────

/// An entry id given as `12`, `"12"` or `"#12"`.
pub fn arg_id(args: &Value, key: &str) -> Result<Option<EntryId>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('#').parse().ok()));
            n.map(|n| Some(EntryId(n)))
                .ok_or_else(|| bad("BAD_ARGS", format!("`{key}` must be an entry id (a positive integer such as 12); got {v}.")))
        }
    }
}

pub fn arg_u64(args: &Value, key: &str) -> Result<Option<u64>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| bad("BAD_ARGS", format!("`{key}` must be a non-negative integer; got {v}."))),
    }
}

fn arg_bool(args: &Value, key: &str) -> Result<bool, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| bad("BAD_ARGS", format!("`{key}` must be true or false; got {v}."))),
    }
}

fn arg_str<'v>(args: &'v Value, key: &str) -> Result<Option<&'v str>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(v) => Err(bad("BAD_ARGS", format!("`{key}` must be a string; got {v}."))),
    }
}

fn arg_kinds(args: &Value, default: &[&'static str]) -> Result<KindFilter, ExecOutcome> {
    match args.get("kinds") {
        None | Some(Value::Null) => Ok(KindFilter::of(default)),
        Some(Value::Array(a)) => {
            let items: Vec<String> = a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect();
            if items.is_empty() {
                return Ok(KindFilter::of(default));
            }
            KindFilter::parse(&items).map_err(|m| bad("BAD_KIND", m))
        }
        Some(v) => Err(bad("BAD_ARGS", format!("`kinds` must be a list of kind names; got {v}."))),
    }
}

fn turn_range(args: &Value) -> Result<(u64, u64), ExecOutcome> {
    let from = arg_u64(args, "turn_from")?.unwrap_or(0);
    let to = arg_u64(args, "turn_to")?.unwrap_or(u64::MAX);
    if from > to {
        return Err(bad("BAD_ARGS", format!("`turn_from` ({from}) is after `turn_to` ({to}); nothing could match. Swap them.")));
    }
    Ok((from, to))
}

fn arg_limit(args: &Value, cap: usize) -> Result<usize, ExecOutcome> {
    match arg_u64(args, "limit")? {
        None => Ok(cap),
        Some(0) => Err(bad("BAD_ARGS", "`limit` must be at least 1.")),
        Some(n) => Ok((n as usize).min(cap)),
    }
}

// ───────────────────────────────────── tree ─────────────────────────────────────

/// Every entry in the log file, with parent/child links and branch heads.
pub struct Tree {
    pub all: BTreeMap<EntryId, Entry>,
    pub children: BTreeMap<EntryId, Vec<EntryId>>,
    /// Current head first, then every other leaf.
    pub heads: Vec<EntryId>,
    pub current: BTreeSet<EntryId>,
}

impl Tree {
    pub fn load(log: &dyn LogStore) -> Result<Tree, ExecOutcome> {
        let Some(head) = log.head() else { return Err(empty_log()) };
        let mut all = BTreeMap::new();
        let mut children: BTreeMap<EntryId, Vec<EntryId>> = BTreeMap::new();
        for e in log.read_from(None) {
            if let Some(p) = e.parent {
                children.entry(p).or_default().push(e.id);
            }
            all.insert(e.id, e);
        }
        let mut heads = vec![head];
        heads.extend(log.heads().into_iter().filter(|h| *h != head));
        let current = log.read_branch(head).map_err(log_error)?.into_iter().map(|e| e.id).collect();
        Ok(Tree { all, children, heads, current })
    }

    /// `a` is `b` or one of its ancestors.
    pub fn is_ancestor(&self, a: EntryId, b: EntryId) -> bool {
        let mut cur = Some(b);
        while let Some(id) = cur {
            if id == a {
                return true;
            }
            if id < a {
                return false;
            }
            cur = self.all.get(&id).and_then(|e| e.parent);
        }
        false
    }

    pub fn heads_containing(&self, id: EntryId) -> Vec<EntryId> {
        self.heads.iter().copied().filter(|h| self.is_ancestor(id, *h)).collect()
    }

    /// `current branch`, or which heads reach it.
    pub fn where_(&self, id: EntryId) -> String {
        if self.current.contains(&id) {
            return "current branch".into();
        }
        match self.heads_containing(id).as_slice() {
            [] => "no branch head".into(),
            hs => format!("off-branch, head {}", hs.iter().map(|h| format!("#{}", h.0)).collect::<Vec<_>>().join(", ")),
        }
    }

    pub fn range(&self) -> String {
        match (self.all.keys().next(), self.all.keys().next_back()) {
            (Some(a), Some(b)) => format!("#{}–#{}", a.0, b.0),
            _ => "none".into(),
        }
    }

    pub fn heads_text(&self) -> String {
        self.heads
            .iter()
            .enumerate()
            .map(|(i, h)| format!("#{}{}", h.0, if i == 0 { " (current)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn empty_log() -> ExecOutcome {
    bad("EMPTY_LOG", "The session log has no entries yet, so there is nothing to read.")
}

fn log_error(e: aho_log::LogError) -> ExecOutcome {
    bad(
        "LOG_UNREADABLE",
        format!("The session log could not be read ({e}). Nothing was returned; `aho session verify` shows where the log is damaged."),
    )
}

/// The branch `branch`/`at` select: the current head by default.
pub struct Selected {
    pub head: EntryId,
    pub current: bool,
    pub rows: Vec<Row>,
}

impl Query<'_> {
    fn select(&self, args: &Value) -> Result<Selected, ExecOutcome> {
        let branch = arg_id(args, "branch")?;
        let at = arg_id(args, "at")?;
        let Some(head) = self.log.head() else { return Err(empty_log()) };
        if branch.is_some() || at.is_some() {
            let tree = Tree::load(self.log)?;
            if let Some(b) = branch
                && !tree.heads.contains(&b)
            {
                return Err(bad(
                    "NOT_A_BRANCH",
                    format!(
                        "Entry #{} is not a branch head. Heads: {}. Use `at` to read the branch as it was at any entry.",
                        b.0,
                        tree.heads_text()
                    ),
                ));
            }
            if let Some(a) = at {
                if !tree.all.contains_key(&a) {
                    return Err(no_such(a, &tree));
                }
                if let Some(b) = branch
                    && !tree.is_ancestor(a, b)
                {
                    return Err(bad(
                        "NOT_ON_BRANCH",
                        format!(
                            "Entry #{} is not on the branch ending at #{}. It is on: {}. Drop `branch` or pick a head that contains it.",
                            a.0,
                            b.0,
                            tree.where_(a)
                        ),
                    ));
                }
            }
            let sel = at.or(branch).unwrap_or(head);
            let branch = self.log.read_branch(sel).map_err(log_error)?;
            return Ok(Selected { head: sel, current: tree.current.contains(&sel), rows: view::rows(branch) });
        }
        let branch = self.log.read_branch(head).map_err(log_error)?;
        Ok(Selected { head, current: true, rows: view::rows(branch) })
    }

    // ───────────────────────────────── session_search ─────────────────────────────────

    pub fn search(&self, args: &Value, self_call: &CallId) -> Out {
        let query = arg_str(args, "query")?.unwrap_or("");
        if query.trim().is_empty() {
            return Err(bad("BAD_ARGS", "`query` is empty. Pass the text (or with `regex: true`, the pattern) to look for."));
        }
        if query.chars().count() > MAX_QUERY {
            return Err(bad("BAD_ARGS", format!("`query` is longer than {MAX_QUERY} characters. Search for a distinctive part of it.")));
        }
        let is_regex = arg_bool(args, "regex")?;
        let pattern = if is_regex { query.to_string() } else { regex::escape(query) };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!arg_bool(args, "case_sensitive")?)
            .size_limit(REGEX_LIMIT)
            .dfa_size_limit(REGEX_LIMIT)
            .build()
            .map_err(|e| {
                bad(
                    "BAD_REGEX",
                    format!("`{query}` is not a usable regular expression ({e}). Nothing was searched. Fix the pattern, or pass `regex: false` to search for the literal text."),
                )
            })?;
        let kinds = arg_kinds(args, DEFAULT_SEARCH)?;
        let tool = arg_str(args, "tool")?;
        let (turn_from, turn_to) = turn_range(args)?;
        let limit = arg_limit(args, self.lim.max_hits)?;
        let sel = self.select(args)?;
        let tools = view::tools_by_call(sel.rows.iter().map(|r| &r.entry));

        let mut hits = Vec::new();
        for r in &sel.rows {
            let k = &r.entry.kind;
            if is_own_call(k, self_call) || !kinds.matches(k) || r.turn < turn_from || r.turn > turn_to {
                continue;
            }
            let call = call_of(k);
            if let Some(t) = tool
                && call.and_then(|c| tools.get(c)).map(String::as_str) != Some(t)
            {
                continue;
            }
            let text = view::body_text(k, self.red);
            let mut found = re.find_iter(&text).filter(|m| !m.is_empty());
            let Some(first) = found.next() else { continue };
            let count = 1 + found.take(99).count();
            let score = count.min(10) * weight(k);
            let snippet = view::snippet(&text, first.start(), first.end(), self.lim.snippet_chars);
            hits.push((score, r, call.cloned(), snippet));
        }
        let total = hits.len();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.entry.id.cmp(&a.1.entry.id)));
        hits.truncate(limit);

        let scope = scope_text(&sel);
        if hits.is_empty() {
            let v = json!({ "branch_head": sel.head.0, "total": 0, "hits": [] });
            return done(v, format!("No entries match `{query}` on {scope}. Try fewer words, other `kinds`, or `regex: true`."));
        }
        let mut lines = vec![format!(
            "{total} entr{} match `{query}` on {scope}{}:",
            if total == 1 { "y" } else { "ies" },
            if total > hits.len() { format!(" (showing the best {})", hits.len()) } else { String::new() }
        )];
        let mut out = Vec::new();
        for (score, r, call, snippet) in &hits {
            let tool = call.as_ref().and_then(|c| tools.get(c));
            let call_txt = match (call, tool) {
                (Some(c), Some(t)) => format!(" {t} [{}]", self.red.text(&c.0)),
                (Some(c), None) => format!(" [{}]", self.red.text(&c.0)),
                _ => String::new(),
            };
            let flag = if r.compacted { " (compacted)" } else { "" };
            lines.push(format!("#{} {} turn {}{call_txt}{flag}: {snippet}", r.entry.id.0, view::label(&r.entry.kind), r.turn));
            out.push(json!({
                "id": r.entry.id.0, "kind": view::label(&r.entry.kind), "turn": r.turn, "score": score,
                "call": call.as_ref().map(|c| self.red.text(&c.0)), "tool": tool, "compacted": r.compacted, "snippet": snippet,
            }));
        }
        if total > hits.len() {
            lines
                .push(format!("{} more match; narrow with `kinds`, `tool`, `turn_from`/`turn_to`, or a longer query.", total - hits.len()));
        }
        lines.push("Follow a hit with session_trace{id} or event_get{id}.".into());
        done(json!({ "branch_head": sel.head.0, "total": total, "capped": total > hits.len(), "hits": out }), lines.join("\n"))
    }

    // ───────────────────────────────── event_list ─────────────────────────────────

    pub fn list(&self, args: &Value) -> Out {
        let from = arg_id(args, "from")?;
        let to = arg_id(args, "to")?;
        if let (Some(a), Some(b)) = (from, to)
            && a > b
        {
            return Err(bad("BAD_ARGS", format!("`from` (#{}) is after `to` (#{}); nothing could match. Swap them.", a.0, b.0)));
        }
        let kinds = match args.get("kinds") {
            None | Some(Value::Null) => None,
            Some(_) => Some(arg_kinds(args, &[])?),
        };
        let (turn_from, turn_to) = turn_range(args)?;
        let limit = arg_limit(args, self.lim.max_window)?;
        let sel = self.select(args)?;
        let tools = view::tools_by_call(sel.rows.iter().map(|r| &r.entry));
        let matching: Vec<&Row> = sel
            .rows
            .iter()
            .filter(|r| from.is_none_or(|f| r.entry.id >= f) && to.is_none_or(|t| r.entry.id <= t))
            .filter(|r| r.turn >= turn_from && r.turn <= turn_to)
            .filter(|r| kinds.as_ref().is_none_or(|k| k.matches(&r.entry.kind)))
            .collect();
        let total = matching.len();
        let shown: &[&Row] = if from.is_some() { &matching[..limit.min(total)] } else { &matching[total.saturating_sub(limit)..] };
        let scope = scope_text(&sel);
        if shown.is_empty() {
            return done(json!({ "branch_head": sel.head.0, "total": 0, "entries": [] }), format!("No entries in that window on {scope}."));
        }
        let mut lines = vec![format!("{} of {total} matching entries on {scope}:", shown.len())];
        let mut out = Vec::new();
        for r in shown {
            let e = &r.entry;
            let summary = view::summary(&e.kind, &tools, self.red, LINE_CHARS);
            let flag = if r.compacted { " (compacted)" } else { "" };
            lines.push(format!("#{} t{} {} [{}]{flag} {summary}", e.id.0, r.turn, view::label(&e.kind), view::principal(&e.by)));
            out.push(json!({
                "id": e.id.0, "parent": e.parent.map(|p| p.0), "kind": view::label(&e.kind), "turn": r.turn,
                "by": view::principal(&e.by), "at": e.at.0, "compacted": r.compacted, "summary": summary,
            }));
        }
        if total > shown.len() {
            let (first, last) = (shown[0].entry.id.0, shown[shown.len() - 1].entry.id.0);
            lines.push(if from.is_some() {
                format!("{} more after #{last}; continue with from={}.", total - shown.len(), last + 1)
            } else {
                format!("{} earlier; page back with to={}.", total - shown.len(), first.saturating_sub(1))
            });
        }
        done(json!({ "branch_head": sel.head.0, "total": total, "entries": out }), lines.join("\n"))
    }

    // ───────────────────────────────── event_get ─────────────────────────────────

    pub fn get(&self, args: &Value) -> Out {
        let Some(id) = arg_id(args, "id")? else {
            return Err(bad("BAD_ARGS", "`id` is required: the entry id to read (from session_search or event_list)."));
        };
        let tree = Tree::load(self.log)?;
        let Some(e) = tree.all.get(&id) else { return Err(no_such(id, &tree)) };
        let branch = self.log.read_branch(id).map_err(log_error)?;
        let turn = view::rows(branch).last().map(|r| r.turn).unwrap_or(0);
        let heads = tree.heads_containing(id);
        let children = tree.children.get(&id).cloned().unwrap_or_default();
        let mut header = format!("#{} {} turn {} by {} ({})", id.0, view::label(&e.kind), turn, view::principal(&e.by), tree.where_(id));
        if children.len() > 1 {
            header.push_str(&format!("; fork point: branches continue at {}", ids(&children)));
        }
        let body = serde_json::to_string_pretty(&self.red.entry(e)).unwrap_or_default();
        let value = json!({
            "id": id.0, "kind": view::label(&e.kind), "turn": turn, "on_current_branch": tree.current.contains(&id),
            "heads": heads.iter().map(|h| h.0).collect::<Vec<_>>(), "children": children.iter().map(|c| c.0).collect::<Vec<_>>(),
        });
        done(value, format!("{header}\n{body}"))
    }
}

pub fn ids(v: &[EntryId]) -> String {
    v.iter().map(|c| format!("#{}", c.0)).collect::<Vec<_>>().join(", ")
}

pub fn no_such(id: EntryId, tree: &Tree) -> ExecOutcome {
    bad(
        "NO_SUCH_ENTRY",
        format!("Entry #{} is not in this session log (ids run {}). Use event_list or session_search to find ids.", id.0, tree.range()),
    )
}

fn scope_text(sel: &Selected) -> String {
    if sel.current {
        format!("the current branch (up to #{})", sel.head.0)
    } else {
        format!("the branch ending at #{} (not current)", sel.head.0)
    }
}

/// The calling call's own `ToolCall`, and the assistant message that carries its `tool_use`.
pub(crate) fn is_own_call(k: &EntryKind, own: &CallId) -> bool {
    match k {
        EntryKind::ToolCall { call, .. } => call == own,
        EntryKind::AssistantMessage { content, .. } => content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == own)),
        _ => false,
    }
}

pub fn call_of(k: &EntryKind) -> Option<&CallId> {
    match k {
        EntryKind::ToolCall { call, .. } | EntryKind::ToolResult { call, .. } | EntryKind::OpStarted { call, .. } => Some(call),
        EntryKind::ApprovalRequested { call, .. } => call.as_ref(),
        _ => None,
    }
}

/// Ranking weight: what people and the model wrote outranks tool output and metadata.
fn weight(k: &EntryKind) -> usize {
    match k {
        EntryKind::UserMessage { .. } | EntryKind::Note { .. } => 4,
        EntryKind::AssistantMessage { .. } => 3,
        EntryKind::ToolCall { .. } => 2,
        _ => 1,
    }
}
