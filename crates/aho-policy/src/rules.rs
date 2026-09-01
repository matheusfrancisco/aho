//! Content rules under the class lattice (04): `deny bash(rm *)`, `ask write(src/**)`,
//! `allow mcp__github__*`, `deny SpawnsProcess`.

use std::fmt;

use aho_cap::ToolDecl;
use aho_types::{Consequence, Value};

use crate::glob;
use crate::paths::{self, Places};
use crate::shell;

/// Content rules exist *under* the class lattice: the class decides the ceiling, a rule decides
/// ask/deny/allow within it. `tool` is a tool name (glob allowed) or a class name (`SpawnsProcess`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub behavior: RuleBehavior,
    pub tool: String,
    pub content: Option<String>,
    pub origin: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleBehavior {
    Allow,
    Ask,
    Deny,
}

impl RuleBehavior {
    pub fn name(self) -> &'static str {
        match self {
            RuleBehavior::Allow => "allow",
            RuleBehavior::Ask => "ask",
            RuleBehavior::Deny => "deny",
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.content {
            Some(c) => write!(f, "{} {}({c})", self.behavior.name(), self.tool),
            None => write!(f, "{} {}", self.behavior.name(), self.tool),
        }
    }
}

/// Parses `(text, origin)` lines such as `("deny bash(rm *)", "./aho.toml:14")`.
/// Blank lines and `#` comments are skipped; anything else malformed fails the whole set, so a
/// typo never silently drops a deny.
pub fn parse_rules(lines: &[(String, String)]) -> Result<Vec<Rule>, String> {
    let mut out = Vec::new();
    for (text, origin) in lines {
        let t = text.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        out.push(parse_one(t, origin).map_err(|detail| {
            format!(
                "Rule `{t}` from {origin} was not loaded: {detail}. Write it as `allow|ask|deny <tool or class>[(<pattern>)]`, \
                 e.g. `deny bash(rm *)`, or remove the line."
            )
        })?);
    }
    Ok(out)
}

fn parse_one(t: &str, origin: &str) -> Result<Rule, String> {
    let (head, rest) = t.split_once(char::is_whitespace).ok_or("it has no tool or class after the behavior")?;
    let behavior = match head.to_ascii_lowercase().as_str() {
        "allow" => RuleBehavior::Allow,
        "ask" => RuleBehavior::Ask,
        "deny" => RuleBehavior::Deny,
        other => return Err(format!("`{other}` is not a behavior (allow, ask, deny)")),
    };
    let rest = rest.trim();
    let (tool, content) = match rest.find('(') {
        Some(open) => {
            if !rest.ends_with(')') {
                return Err("the pattern is missing its closing `)`".into());
            }
            let content = rest[open + 1..rest.len() - 1].trim();
            if content.is_empty() {
                return Err("the pattern inside `()` is empty".into());
            }
            (rest[..open].trim(), Some(content.to_string()))
        }
        None => (rest, None),
    };
    if tool.is_empty() {
        return Err("it names no tool or class".into());
    }
    if !tool.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*' | '?' | '.' | ':')) {
        return Err(format!("`{tool}` is not a tool name or class"));
    }
    Ok(Rule { behavior, tool: tool.to_string(), content, origin: origin.to_string() })
}

/// What a content pattern is matched against: the tool's primary argument.
pub(crate) enum Subject {
    /// `command` (bash-like): the whole line plus every simple command in it.
    Command { whole: String, parts: Vec<(String, String)> },
    /// `path` / `file_path` / `paths`: each as workspace-relative (when inside) and absolute.
    Paths(Vec<(Option<String>, String)>),
    /// Anything else: the canonical JSON of the args.
    Json(String),
}

/// Path-shaped argument keys, in the order tools use them.
pub(crate) const PATH_KEYS: &[&str] = &["path", "file_path", "notebook_path"];

/// Every path argument a call names (`path`, `file_path`, `notebook_path`, `paths[]`).
pub(crate) fn path_args(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = PATH_KEYS.iter().filter_map(|k| args.get(*k).and_then(Value::as_str)).map(str::to_string).collect();
    if let Some(a) = args.get("paths").and_then(Value::as_array) {
        out.extend(a.iter().filter_map(Value::as_str).map(str::to_string));
    }
    out
}

/// Resolves a tool's path argument lexically: `~`/`$HOME` expanded, relative joined to the workspace.
pub(crate) fn resolve_tool_path(p: &str, places: &Places) -> Option<String> {
    let p = p.replace("${HOME}", "~").replacen("$HOME", "~", 1);
    let p = paths::expand_tilde(&p, places.home.as_deref(), places.workspace.as_deref()).ok()?;
    if p.starts_with('/') { Some(paths::normalize(&p)) } else { places.workspace.as_deref().map(|ws| paths::join(ws, &p)) }
}

impl Subject {
    pub(crate) fn of(args: &Value, places: &Places) -> Subject {
        if let Some(cmd) = args.get("command").and_then(Value::as_str) {
            return Subject::Command { whole: cmd.to_string(), parts: shell::simple_commands(cmd) };
        }
        let ps = path_args(args);
        if !ps.is_empty() {
            return Subject::Paths(
                ps.iter()
                    .map(|p| {
                        let abs = resolve_tool_path(p, places).unwrap_or_else(|| p.clone());
                        let rel = places
                            .workspace
                            .as_deref()
                            .filter(|ws| paths::strictly_within(&abs, ws))
                            .map(|ws| abs[ws.len() + 1..].to_string());
                        (rel, abs)
                    })
                    .collect(),
            );
        }
        Subject::Json(args.to_string())
    }
}

impl Rule {
    /// The class this rule targets, when `tool` is a class name (`SpawnsProcess`).
    pub fn class(&self) -> Option<Consequence> {
        Consequence::ALL.into_iter().find(|c| c.name() == self.tool)
    }

    fn matches_tool(&self, decl: &ToolDecl, name: &str) -> bool {
        if let Some(c) = self.class() {
            return decl.class == c;
        }
        let pat = self.tool.to_ascii_lowercase();
        std::iter::once(name).chain(decl.aliases.iter().copied()).any(|n| glob::matches(&pat, &n.to_ascii_lowercase(), false))
    }

    /// Deny/ask rules match if *any* part matches; allow rules only if *every* part does, so
    /// `allow bash(git *)` never covers `git status && rm -rf x`.
    pub(crate) fn matches(&self, decl: &ToolDecl, name: &str, subject: &Subject) -> bool {
        if !self.matches_tool(decl, name) {
            return false;
        }
        let Some(pat) = &self.content else { return true };
        let any = self.behavior != RuleBehavior::Allow;
        match subject {
            Subject::Command { whole, parts } => {
                let part = |(raw, bare): &(String, String)| glob::matches(pat, raw, false) || glob::matches(pat, bare, false);
                if any { glob::matches(pat, whole, false) || parts.iter().any(part) } else { !parts.is_empty() && parts.iter().all(part) }
            }
            Subject::Paths(ps) => {
                let one = |(rel, abs): &(Option<String>, String)| {
                    rel.as_deref().is_some_and(|r| glob::matches(pat, r, true)) || glob::matches(pat, abs, true)
                };
                if any { ps.iter().any(one) } else { ps.iter().all(one) }
            }
            Subject::Json(s) => glob::matches(pat, s, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> (String, String) {
        (s.to_string(), "./aho.toml:3".to_string())
    }

    #[test]
    fn parses_tool_content_and_class_rules() {
        let rules = parse_rules(&[
            line("deny bash(rm *)"),
            line("  "),
            line("# note"),
            line("Allow mcp__github__*"),
            line("ask SpawnsProcess"),
            line("allow write(src/**)"),
        ])
        .unwrap();
        assert_eq!(rules.len(), 4);
        assert_eq!(
            rules[0],
            Rule { behavior: RuleBehavior::Deny, tool: "bash".into(), content: Some("rm *".into()), origin: "./aho.toml:3".into() }
        );
        assert_eq!(rules[0].to_string(), "deny bash(rm *)");
        assert_eq!(rules[1].behavior, RuleBehavior::Allow);
        assert_eq!(rules[2].class(), Some(Consequence::SpawnsProcess));
        assert_eq!(rules[3].content.as_deref(), Some("src/**"));
    }

    #[test]
    fn malformed_rules_fail_loudly_with_origin() {
        for bad in ["permit bash", "deny", "deny bash(rm *", "deny bash()", "deny ba sh", "deny (x)"] {
            let err = parse_rules(&[line(bad)]).unwrap_err();
            assert!(err.contains("./aho.toml:3") && err.contains(bad), "{err}");
        }
    }
}
