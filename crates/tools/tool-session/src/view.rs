//! Redaction and rendering: what a log entry looks like to the model. Everything that leaves this
//! crate as text passes through a [`Redactor`] first.

use std::collections::BTreeMap;

use aho_log::{Entry, EntryKind, OpStatus, ToolOutcome};
use aho_types::{CallId, ContentBlock, Hash, Principal, Value};

/// Replaces the plaintext (and digest) of a known secret inside any string.
pub const REDACTED: &str = "[redacted]";
/// Replaces a value the config resolver already stored as `{"digest": <hex>}`.
pub const SECRET: &str = "[secret]";

/// What must never be rendered: the plaintext of every loadout secret and its blake3 digest.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    needles: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut needles = Vec::new();
        for s in secrets.into_iter().filter(|s| !s.is_empty()) {
            needles.push(Hash::of(s.as_bytes()).to_hex());
            needles.push(s);
        }
        // Longest first, so a secret that contains another is replaced whole.
        needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup();
        Redactor { needles }
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for n in &self.needles {
            if out.contains(n.as_str()) {
                out = out.replace(n.as_str(), REDACTED);
            }
        }
        out
    }

    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) if is_digest(m) => Value::String(SECRET.into()),
            Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("image") && m.get("data").is_some_and(Value::is_array) => {
                let bytes = m.get("data").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                let mut out = serde_json::Map::new();
                for (k, x) in m {
                    let x = if k == "data" { Value::String(format!("[{bytes} bytes]")) } else { self.value(x) };
                    out.insert(self.text(k), x);
                }
                Value::Object(out)
            }
            Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (self.text(k), self.value(x))).collect()),
            other => other.clone(),
        }
    }

    /// The whole entry as redacted JSON.
    pub fn entry(&self, e: &Entry) -> Value {
        self.value(&serde_json::to_value(e).unwrap_or(Value::Null))
    }

    fn kind_json(&self, k: &EntryKind) -> String {
        let mut v = self.value(&serde_json::to_value(k).unwrap_or(Value::Null));
        if let Some(m) = v.as_object_mut() {
            m.remove("type");
        }
        v.to_string()
    }
}

/// `{"digest": "<64 hex>"}` — how `aho-config` and `aho-compose` store a secret's value.
fn is_digest(m: &serde_json::Map<String, Value>) -> bool {
    m.len() == 1 && m.get("digest").and_then(Value::as_str).is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

// ───────────────────────────────────── kinds ─────────────────────────────────────

/// Every `EntryKind::tag()`, for filter validation.
pub const TAGS: &[&str] = &[
    "ConfigResolved",
    "CapActivated",
    "PlanSealed",
    "CapDelta",
    "LoadoutDiagnostic",
    "PolicyEpoch",
    "ApprovalRequested",
    "ApprovalDecided",
    "LeaseGranted",
    "LeaseRevoked",
    "TurnStart",
    "TurnEnd",
    "StepStart",
    "StepEnd",
    "SystemMessage",
    "UserMessage",
    "AssistantMessage",
    "AssistantAttempt",
    "RequestHeader",
    "ToolCall",
    "ToolResult",
    "OpStarted",
    "OpDone",
    "SurfaceReplace",
    "CompactionStart",
    "CompactionEnd",
    "CompactionFailed",
    "HookInvoked",
    "HookResult",
    "CommandRun",
    "CommandDone",
    "SurfaceAttached",
    "SessionRecoveryIntent",
    "SessionSeen",
    "SessionMeta",
    "SubagentStarted",
    "SubagentEnded",
    "SubagentDescriptor",
    "WorkspaceCheckpoint",
    "WorkspaceRestored",
    "InvariantRegistered",
    "InvariantChecked",
    "InvariantMutated",
    "Custom",
    "Note",
];

const GROUPS: &[(&str, &[&str])] = &[
    ("message", &["UserMessage", "AssistantMessage"]),
    ("tool", &["ToolCall", "ToolResult"]),
    ("op", &["OpStarted", "OpDone"]),
    ("approval", &["ApprovalRequested", "ApprovalDecided"]),
    ("checkpoint", &["WorkspaceCheckpoint", "WorkspaceRestored"]),
];

/// What `session_search` looks at when no `kinds` are given.
pub const DEFAULT_SEARCH: &[&str] = &["UserMessage", "AssistantMessage", "ToolCall", "ToolResult", "Note"];

fn norm(s: &str) -> String {
    s.chars().filter(|c| *c != '_' && *c != '-' && *c != ' ').flat_map(char::to_lowercase).collect()
}

/// A parsed `kinds` argument: core tags, and `Ns/Name` (or `Ns/*`) for capability entries.
#[derive(Debug, Default)]
pub struct KindFilter {
    tags: Vec<&'static str>,
    custom: Vec<(String, Option<String>)>,
}

impl KindFilter {
    pub fn of(tags: &[&'static str]) -> Self {
        KindFilter { tags: tags.to_vec(), custom: Vec::new() }
    }

    pub fn parse(items: &[String]) -> Result<Self, String> {
        let mut f = KindFilter::default();
        for raw in items {
            if let Some((ns, name)) = raw.split_once('/') {
                let name = (name != "*" && !name.is_empty()).then(|| name.to_string());
                f.custom.push((ns.to_string(), name));
                continue;
            }
            let n = norm(raw);
            let known = |s: &str| GROUPS.iter().any(|(g, _)| *g == s) || TAGS.iter().any(|t| norm(t) == s);
            let n = n.strip_suffix('s').filter(|s| !known(&n) && known(s)).map(str::to_string).unwrap_or(n);
            if let Some((_, tags)) = GROUPS.iter().find(|(g, _)| *g == n) {
                f.tags.extend_from_slice(tags);
            } else if let Some(t) = TAGS.iter().find(|t| norm(t) == n) {
                f.tags.push(t);
            } else {
                return Err(format!(
                    "`{raw}` is not an entry kind. Use one of: {}; the groups {}; or `Ns/Name` / `Ns/*` for capability entries.",
                    TAGS.iter().map(|t| snake(t)).collect::<Vec<_>>().join(", "),
                    GROUPS.iter().map(|(g, _)| *g).collect::<Vec<_>>().join(", ")
                ));
            }
        }
        Ok(f)
    }

    pub fn matches(&self, k: &EntryKind) -> bool {
        if self.tags.contains(&k.tag()) {
            return true;
        }
        match k {
            EntryKind::Custom { ns, name, .. } => {
                self.custom.iter().any(|(n, m)| n.eq_ignore_ascii_case(ns) && m.as_deref().is_none_or(|m| m.eq_ignore_ascii_case(name)))
            }
            _ => false,
        }
    }
}

/// `ToolCall` → `tool_call`.
pub fn snake(tag: &str) -> String {
    let mut out = String::new();
    for (i, c) in tag.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// `tool_call`, or `Goal/Changed` for capability entries.
pub fn label(k: &EntryKind) -> String {
    match k {
        EntryKind::Custom { ns, name, .. } => format!("{ns}/{name}"),
        other => snake(other.tag()),
    }
}

pub fn principal(p: &Principal) -> String {
    match p {
        Principal::Operator => "operator".into(),
        Principal::Engineer => "engineer".into(),
        Principal::Agent { turn, .. } => format!("agent@turn{}", turn.0),
        Principal::System { name } => name.clone(),
    }
}

// ───────────────────────────────────── rows ─────────────────────────────────────

/// An entry on a branch with what the branch says about it.
#[derive(Clone, Debug)]
pub struct Row {
    pub entry: Entry,
    /// Entries from `Turn/Start n` up to the next `Turn/Start` belong to turn `n`; before the first prompt, 0.
    pub turn: u64,
    /// A later `Surface/Replace` on this branch covers it: the model no longer sees it verbatim.
    pub compacted: bool,
}

/// Annotates a branch (oldest first) with turns and compaction coverage.
pub fn rows(branch: Vec<Entry>) -> Vec<Row> {
    let mut turn = 0;
    let mut out: Vec<Row> = Vec::with_capacity(branch.len());
    for e in branch {
        if let EntryKind::TurnStart { turn: t } = &e.kind {
            turn = t.0;
        }
        if let EntryKind::SurfaceReplace { start, end, .. } = &e.kind {
            for r in out.iter_mut().filter(|r| r.entry.id >= *start && r.entry.id <= *end) {
                r.compacted = true;
            }
        }
        out.push(Row { entry: e, turn, compacted: false });
    }
    out
}

/// `call → tool` over some entries.
pub fn tools_by_call<'a>(entries: impl Iterator<Item = &'a Entry>) -> BTreeMap<CallId, String> {
    entries
        .filter_map(|e| match &e.kind {
            EntryKind::ToolCall { call, tool, .. } => Some((call.clone(), tool.clone())),
            _ => None,
        })
        .collect()
}

// ───────────────────────────────────── text ─────────────────────────────────────

fn blocks_text(blocks: &[ContentBlock], r: &Redactor) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text } => r.text(text),
            ContentBlock::Image { media_type, data } => format!("[image {media_type}, {} bytes]", data.len()),
            ContentBlock::ToolUse { id, name, args } => format!("[tool_use {name} {}] {}", r.text(&id.0), r.value(args)),
            ContentBlock::ToolResult { id, content, is_error } => {
                format!("[tool_result {}{}] {}", r.text(&id.0), if *is_error { " error" } else { "" }, blocks_text(content, r))
            }
            ContentBlock::ProviderMetadata { .. } => String::new(),
        })
        .collect();
    parts.join("\n")
}

/// The searchable text of an entry, already redacted.
pub fn body_text(k: &EntryKind, r: &Redactor) -> String {
    match k {
        EntryKind::UserMessage { text, .. } | EntryKind::Note { text } => r.text(text),
        EntryKind::AssistantMessage { content, .. } => blocks_text(content, r),
        EntryKind::ToolCall { tool, args, .. } => format!("{tool} {}", r.value(args)),
        EntryKind::ToolResult { outcome, content, .. } => {
            let body = blocks_text(content, r);
            match outcome {
                ToolOutcome::Ok => body,
                ToolOutcome::Err { code, reason } => format!("{code}: {}\n{body}", r.text(reason)),
                ToolOutcome::Cancelled { reason } => format!("cancelled ({reason:?})\n{body}"),
            }
        }
        other => r.kind_json(other),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` chars, cut on a char boundary with `…`.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `width` chars of single-line context centred on the byte range `[start, end)` of `text`.
pub fn snippet(text: &str, start: usize, end: usize, width: usize) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let first = chars.iter().position(|(b, _)| *b >= start).unwrap_or(chars.len());
    let last = chars.iter().position(|(b, _)| *b >= end).unwrap_or(chars.len());
    let span = last.saturating_sub(first);
    let (from, to) = if span >= width {
        (first, first + width)
    } else {
        let from = first.saturating_sub((width - span) / 2);
        let to = (from + width).min(chars.len());
        (to.saturating_sub(width), to)
    };
    let body: String = chars[from..to].iter().map(|(_, c)| *c).collect();
    let mut out = one_line(&body);
    if from > 0 {
        out.insert(0, '…');
    }
    if to < chars.len() {
        out.push('…');
    }
    out
}

fn outcome(o: &ToolOutcome, r: &Redactor) -> String {
    match o {
        ToolOutcome::Ok => "ok".into(),
        ToolOutcome::Err { code, reason } => format!("error {code}: {}", r.text(reason)),
        ToolOutcome::Cancelled { reason } => format!("cancelled ({})", snake(&format!("{reason:?}"))),
    }
}

pub fn op_status(s: &OpStatus) -> String {
    match s {
        OpStatus::Completed { code } => format!("completed (exit {code})"),
        OpStatus::Failed { err } => format!("failed: {err}"),
        OpStatus::Cancelled { reason } => format!("cancelled ({})", snake(&format!("{reason:?}"))),
        OpStatus::Uncertain { .. } => "uncertain: may or may not have happened".into(),
    }
}

/// One compact, redacted line describing an entry's body (no id or kind prefix), ≤ `max` chars.
pub fn summary(k: &EntryKind, tools: &BTreeMap<CallId, String>, r: &Redactor, max: usize) -> String {
    let tool = |c: &CallId| tools.get(c).cloned().unwrap_or_else(|| "?".into());
    let s = match k {
        EntryKind::UserMessage { form, text, .. } => format!("{}: {}", snake(&format!("{form:?}")), r.text(text)),
        EntryKind::AssistantMessage { content, stop, .. } => format!("{} (stop: {})", blocks_text(content, r), snake(&format!("{stop:?}"))),
        EntryKind::ToolCall { call, tool, args, .. } => format!("{tool} [{}] {}", r.text(&call.0), r.value(args)),
        EntryKind::ToolResult { call, outcome: o, content, late, .. } => format!(
            "{} [{}] {}{}: {}",
            tool(call),
            r.text(&call.0),
            outcome(o, r),
            if *late { " (late)" } else { "" },
            blocks_text(content, r)
        ),
        EntryKind::OpStarted { op, call, kind, .. } => format!("op {} for {} [{}] ({kind:?})", op.0, tool(call), r.text(&call.0)),
        EntryKind::OpDone { op, status } => format!("op {} {}", op.0, r.text(&op_status(status))),
        EntryKind::WorkspaceCheckpoint { id, tree, cause, .. } => {
            format!("checkpoint c{} tree {} ({})", id.0, tree.short(), cause_text(cause, r))
        }
        EntryKind::WorkspaceRestored { from, to, mode, changed, deleted } => format!(
            "rewound workspace to c{} from c{} ({}): {} changed, {} deleted",
            to.0,
            from.0,
            snake(&format!("{mode:?}")),
            changed.len(),
            deleted.len()
        ),
        EntryKind::TurnStart { turn } => format!("turn {}", turn.0),
        EntryKind::TurnEnd { turn } => format!("turn {}", turn.0),
        EntryKind::Note { text } => r.text(text),
        EntryKind::Custom { body, .. } => r.value(body).to_string(),
        other => r.kind_json(other),
    };
    clip(&one_line(&s), max)
}

pub fn cause_text(c: &aho_log::CheckpointCause, r: &Redactor) -> String {
    match c {
        aho_log::CheckpointCause::ToolResult(call) => format!("after call {}", r.text(&call.0)),
        aho_log::CheckpointCause::TurnEnd => "turn end".into(),
        aho_log::CheckpointCause::Manual => "manual".into(),
        aho_log::CheckpointCause::PreRewind => "before a rewind".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redactor_hides_plaintext_digests_and_image_bytes() {
        let r = Redactor::new(["sk-live-123".to_string(), "sk-live-123-longer".to_string(), String::new()]);
        assert_eq!(r.text("a sk-live-123-longer b sk-live-123"), "a [redacted] b [redacted]");
        let digest = Hash::of(b"sk-live-123").to_hex();
        let v = r.value(&json!({
            "k": {"digest": "a".repeat(64)},
            "not": {"digest": "short"},
            "s": format!("hash {digest}"),
            "img": {"type": "image", "media_type": "image/png", "data": [1, 2, 3]}
        }));
        assert_eq!(v["k"], SECRET);
        assert_eq!(v["not"]["digest"], "short");
        assert_eq!(v["s"], "hash [redacted]");
        assert_eq!(v["img"]["data"], "[3 bytes]");
    }

    #[test]
    fn kind_filter_accepts_tags_groups_and_namespaces_and_names_the_vocabulary() {
        let f = KindFilter::parse(&["ToolCall".into(), "notes".into(), "Goal/*".into()]).unwrap();
        assert!(f.matches(&EntryKind::ToolCall {
            call: CallId("c".into()),
            tool: "t".into(),
            args: json!({}),
            class: aho_types::Consequence::ReadOnly
        }));
        assert!(f.matches(&EntryKind::Note { text: "n".into() }));
        assert!(f.matches(&EntryKind::Custom { ns: "goal".into(), name: "Changed".into(), body: json!({}) }));
        assert!(!f.matches(&EntryKind::Custom { ns: "Todo".into(), name: "Set".into(), body: json!({}) }));
        let g = KindFilter::parse(&["messages".into()]).unwrap();
        assert!(g.matches(&EntryKind::UserMessage { form: aho_types::Form::Prompt, text: "x".into(), client_key: None }));
        let err = KindFilter::parse(&["tool_calls_please".into()]).unwrap_err();
        assert!(err.contains("tool_call") && err.contains("Ns/*"), "{err}");
    }

    #[test]
    fn snippet_centres_on_the_match_and_stays_on_char_boundaries() {
        let text = format!("{}needle{}", "é".repeat(100), "ü".repeat(100));
        let start = text.find("needle").unwrap();
        let s = snippet(&text, start, start + 6, 20);
        assert!(s.contains("needle") && s.starts_with('…') && s.ends_with('…'), "{s}");
        assert!(s.chars().count() <= 22, "{s}");
        assert_eq!(snippet("short needle", 6, 12, 200), "short needle");
        assert_eq!(clip("abcdef", 4), "abc…");
    }
}
