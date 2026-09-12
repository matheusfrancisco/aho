//! `.claude/settings.json` / `hooks.json` → hook groups.
//!
//! Accepted shape: `{"hooks": {"<Event>": [{"matcher": "<regex>", "hooks": [{"type": "command",
//! "command": "…", "timeout": <seconds>, "on_error": "allow"|"deny"}]}]}}`, or the inner
//! `{"<Event>": [...]}` object at top level (plugin `hooks.json`). `on_error` is an aho extension
//! (Claude Code ignores unknown keys). Unsupported events and non-`command` hook types are
//! skipped with a warning; anything malformed fails the whole file.

use std::time::Duration;

use aho_cap::OnError;
use aho_types::Value;
use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CcEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStop,
    PreCompact,
}

impl CcEvent {
    pub const ALL: [CcEvent; 8] = [
        CcEvent::PreToolUse,
        CcEvent::PostToolUse,
        CcEvent::UserPromptSubmit,
        CcEvent::SessionStart,
        CcEvent::SessionEnd,
        CcEvent::Stop,
        CcEvent::SubagentStop,
        CcEvent::PreCompact,
    ];
    pub fn name(self) -> &'static str {
        match self {
            CcEvent::PreToolUse => "PreToolUse",
            CcEvent::PostToolUse => "PostToolUse",
            CcEvent::UserPromptSubmit => "UserPromptSubmit",
            CcEvent::SessionStart => "SessionStart",
            CcEvent::SessionEnd => "SessionEnd",
            CcEvent::Stop => "Stop",
            CcEvent::SubagentStop => "SubagentStop",
            CcEvent::PreCompact => "PreCompact",
        }
    }
    pub fn parse(s: &str) -> Option<CcEvent> {
        CcEvent::ALL.into_iter().find(|e| e.name() == s)
    }
}

/// Claude Code matcher: empty or `*` matches everything; otherwise a regex over the whole name.
#[derive(Clone, Debug)]
pub struct Matcher {
    pub raw: String,
    re: Option<Regex>,
}

impl Matcher {
    pub fn new(raw: &str) -> Result<Matcher, String> {
        let t = raw.trim();
        let re = if t.is_empty() || t == "*" {
            None
        } else {
            Some(Regex::new(&format!("^(?:{t})$")).map_err(|e| format!("matcher `{t}` is not a valid regex ({e})"))?)
        };
        Ok(Matcher { raw: t.to_string(), re })
    }
    /// True when any candidate name matches.
    pub fn matches(&self, candidates: &[&str]) -> bool {
        match &self.re {
            None => true,
            Some(re) => candidates.iter().any(|c| re.is_match(c)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookCommand {
    pub command: String,
    pub timeout: Duration,
    pub on_error: OnError,
    /// The file it came from.
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct HookGroup {
    pub event: CcEvent,
    pub matcher: Matcher,
    pub commands: Vec<HookCommand>,
}

#[derive(Clone, Copy, Debug)]
pub struct Defaults {
    pub timeout: Duration,
    pub on_error: OnError,
}

#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub groups: Vec<HookGroup>,
    pub warnings: Vec<String>,
}

pub fn parse_settings(text: &str, path: &str, d: &Defaults) -> Result<Parsed, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| format!("{path} is not valid JSON ({e})"))?;
    let Value::Object(root) = root else { return Err(format!("{path} must hold a JSON object")) };
    let hooks = match root.get("hooks") {
        Some(Value::Object(h)) => h.clone(),
        Some(_) => return Err(format!("{path}: `hooks` must be an object of event names")),
        None if root.keys().any(|k| CcEvent::parse(k).is_some()) => root.clone(),
        None => return Ok(Parsed::default()),
    };
    let mut out = Parsed::default();
    for (event_name, groups) in &hooks {
        let Some(event) = CcEvent::parse(event_name) else {
            out.warnings.push(format!("{path}: hook event `{event_name}` has no aho equivalent; its hooks were skipped"));
            continue;
        };
        let Value::Array(groups) = groups else { return Err(format!("{path}: `hooks.{event_name}` must be an array")) };
        for (gi, g) in groups.iter().enumerate() {
            let at = format!("{path}: `hooks.{event_name}[{gi}]`");
            let Value::Object(g) = g else { return Err(format!("{at} must be an object")) };
            let matcher = match g.get("matcher") {
                None | Some(Value::Null) => Matcher::new("")?,
                Some(Value::String(m)) => Matcher::new(m).map_err(|e| format!("{at}: {e}"))?,
                Some(_) => return Err(format!("{at}.matcher must be a string")),
            };
            let Some(Value::Array(entries)) = g.get("hooks") else { return Err(format!("{at}.hooks must be an array")) };
            let mut commands = Vec::new();
            for (hi, h) in entries.iter().enumerate() {
                let at = format!("{at}.hooks[{hi}]");
                let Value::Object(h) = h else { return Err(format!("{at} must be an object")) };
                match h.get("type").and_then(Value::as_str) {
                    Some("command") => {}
                    Some(other) => {
                        out.warnings.push(format!("{at}: `{other}` hooks are not supported by aho; it was skipped"));
                        continue;
                    }
                    None => return Err(format!("{at}.type must be \"command\"")),
                }
                let command = match h.get("command") {
                    Some(Value::String(c)) if !c.trim().is_empty() => c.clone(),
                    _ => return Err(format!("{at}.command must be a non-empty string")),
                };
                let timeout = match h.get("timeout") {
                    None | Some(Value::Null) => d.timeout,
                    Some(v) => match v.as_f64().filter(|s| *s > 0.0) {
                        Some(s) => Duration::from_secs_f64(s),
                        None => return Err(format!("{at}.timeout must be a positive number of seconds")),
                    },
                };
                let on_error = match h.get("on_error") {
                    None | Some(Value::Null) => d.on_error,
                    Some(Value::String(s)) if s == "deny" => OnError::Deny,
                    Some(Value::String(s)) if s == "allow" => OnError::Allow,
                    Some(_) => return Err(format!("{at}.on_error must be \"allow\" or \"deny\"")),
                };
                commands.push(HookCommand { command, timeout, on_error, source: path.to_string() });
            }
            if !commands.is_empty() {
                out.groups.push(HookGroup { event, matcher, commands });
            }
        }
    }
    Ok(out)
}
