//! Skills, commands, and agent definitions are one `Kind::Template` manifest.
//!
//! * [`parse`] reads a `SKILL.md` through `Untrusted`: policy-shaped frontmatter keys
//!   ([`DROPPED_KEYS`]) are dropped and reported, never honored; legacy camelCase keys
//!   (`allowedTools`, `whenToUse`, …) are rejected loudly with the kebab-case spelling to use.
//! * [`catalog_snapshot`] is the digest-deduped `<available_skills>` context (`form: Catalog`).
//! * [`render_invocation`] / [`render_content`] are the one `<skill_content>` render used by the
//!   `skill` tool and by `/name` from a genuine user message.
//! * [`matches_paths`]: a skill with `paths:` activates only once a touched file matches.
//!
//! Providers (e.g. `skills-fsdir`) publish the active templates as the service [`SERVICE`]
//! (`Arc<Vec<Template>>`) and every discovered skill's state as [`STATUS_SERVICE`]
//! (`Arc<Vec<SkillStatus>>`).
//!
//! Frontmatter keys (kebab-case; `snake_case` spellings are accepted too):
//! `name` (required, kebab-case, ≤ 64), `description` (required), `when-to-use`,
//! `invocable-by: operator | agent | both`, `disable-model-invocation: true` (operator only),
//! `user-invocable: false` (agent only), `context: inline | fork`, `allowed-tools` (list or
//! comma/space separated string), `paths` (list or comma separated globs), `model`
//! (`inherit` = none), `argument-hint`. Other lowercase keys (`version`, `license`,
//! `metadata`, …) are ignored.

#![forbid(unsafe_code)]

mod frontmatter;

use std::collections::BTreeSet;
use std::fmt;

use aho_cap::{CapOrigin, SeamId};
use aho_policy::Untrusted;
use aho_types::*;
use frontmatter::{Entry, Val};

pub const SEAM: SeamId = SeamId("skills");
/// Service holding the active templates: `Arc<Vec<Template>>`.
pub const SERVICE: &str = "skills";
/// Service holding every discovered skill and its state: `Arc<Vec<SkillStatus>>`.
pub const STATUS_SERVICE: &str = "skills.status";
/// Frontmatter keys that would steer policy or run commands. Matched case-insensitively,
/// dropped with their whole block, and reported (`ResourceKeysDropped`).
pub const DROPPED_KEYS: &[&str] = &["sandbox", "allow", "policy", "permissions", "hooks"];
/// Longest accepted skill name (Agent Skills limit).
pub const MAX_NAME: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocable {
    Operator,
    Agent,
    Both,
}

impl Invocable {
    pub fn by_agent(self) -> bool {
        matches!(self, Invocable::Agent | Invocable::Both)
    }
    pub fn by_operator(self) -> bool {
        matches!(self, Invocable::Operator | Invocable::Both)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Inline,
    Fork,
}

impl Context {
    pub fn name(self) -> &'static str {
        match self {
            Context::Inline => "inline",
            Context::Fork => "fork",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub invocable_by: Invocable,
    pub context: Context,
    /// Data for a forked runner to narrow its tools; never a grant (frontmatter cannot widen policy).
    pub allowed_tools: Vec<String>,
    /// Conditional activation: `Cap/Delta{Activated, reason: PathMatch}` when a touched file matches.
    pub paths: Vec<String>,
    pub model: Option<String>,
    pub argument_hint: Option<String>,
    pub body: String,
    /// Directory the skill's resources are relative to.
    pub base_dir: String,
    /// Files shipped next to the skill, relative to `base_dir` (filled by the discoverer).
    pub resources: Vec<String>,
    pub origin: CapOrigin,
    /// The file the template was parsed from.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SkillError {
    NoFrontmatter { path: String },
    BadFrontmatter { path: String, line: u32, msg: String },
    LegacyKey { path: String, line: u32, key: String, use_instead: String },
    MissingKey { path: String, key: &'static str },
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::NoFrontmatter { path } => write!(
                f,
                "{path}: no frontmatter; the skill was not loaded. Start the file with `---`, `name: <kebab-name>`, `description: <one line>`, `---`."
            ),
            SkillError::BadFrontmatter { path, line, msg } => {
                write!(f, "{path}: frontmatter invalid at line {line}: {msg}. The skill was not loaded; fix the file and reload.")
            }
            SkillError::LegacyKey { path, line, key, use_instead } => write!(
                f,
                "{path}: frontmatter invalid at line {line}: `{key}` is a legacy camelCase key. The skill was not loaded; write `{use_instead}` instead."
            ),
            SkillError::MissingKey { path, key } => {
                write!(f, "{path}: frontmatter has no `{key}`; the skill was not loaded. Add `{key}: …` between the `---` lines.")
            }
        }
    }
}

impl std::error::Error for SkillError {}

/// A discovered skill's standing. Discovered skills start `Unlocked`; the operator enables them.
#[derive(Clone, Debug, PartialEq)]
pub enum SkillState {
    Active,
    Unlocked {
        fix: String,
    },
    Disabled {
        why: String,
    },
    Broken {
        error: String,
    },
    /// A lower-rank root already provides this name.
    Shadowed {
        by: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillStatus {
    /// `skill.<name>`.
    pub id: String,
    pub name: String,
    pub path: String,
    pub origin: CapOrigin,
    pub state: SkillState,
    /// Policy-shaped keys the loader dropped from this file.
    pub dropped: Vec<String>,
}

/// `skill.<name>`: the capability id used by `aho enable` and `--disable`; its setting is `skills.<name>.enabled`.
pub fn skill_id(name: &str) -> String {
    format!("skill.{name}")
}

pub fn is_kebab(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// Parses a `SKILL.md`; returns the template plus the policy-shaped keys it dropped.
/// `origin`, `base_dir` and `resources` are left for the discoverer to fill.
pub fn parse(doc: Untrusted<String>, path: &str) -> Result<(Template, Vec<String>), SkillError> {
    let mut parsed = None;
    let (_, dropped) = doc.sanitize(|text| {
        let result = frontmatter::parse(&text);
        let dropped = match &result {
            Ok(Some(d)) => d.entries.iter().filter(|e| is_dropped(&e.key)).map(|e| e.key.clone()).collect(),
            _ => Vec::new(),
        };
        parsed = Some(result);
        (text, dropped)
    });
    let doc = match parsed {
        Some(Ok(Some(doc))) => doc,
        Some(Ok(None)) | None => return Err(SkillError::NoFrontmatter { path: path.to_string() }),
        Some(Err(e)) => return Err(SkillError::BadFrontmatter { path: path.to_string(), line: e.line, msg: e.msg }),
    };
    let template = build(&doc.entries, doc.body, path)?;
    Ok((template, dropped))
}

fn is_dropped(key: &str) -> bool {
    DROPPED_KEYS.iter().any(|d| d.eq_ignore_ascii_case(key.trim()))
}

/// `allowedTools` → `allowed-tools`.
fn kebab_of(key: &str) -> String {
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out
}

fn build(entries: &[Entry], body: String, path: &str) -> Result<Template, SkillError> {
    let bad = |line: u32, msg: String| SkillError::BadFrontmatter { path: path.to_string(), line, msg };
    let mut name = None;
    let mut description = None;
    let mut when_to_use = None;
    let mut invocable_by = None;
    let (mut agent_ok, mut operator_ok) = (true, true);
    let mut context = Context::Inline;
    let mut allowed_tools = Vec::new();
    let mut paths = Vec::new();
    let mut model = None;
    let mut argument_hint = None;

    for e in entries {
        if is_dropped(&e.key) {
            continue;
        }
        if e.key.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(SkillError::LegacyKey { path: path.to_string(), line: e.line, key: e.key.clone(), use_instead: kebab_of(&e.key) });
        }
        let key = e.key.replace('_', "-");
        match key.as_str() {
            "name" => {
                let n = scalar(e).map_err(|m| bad(e.line, m))?;
                if !is_kebab(&n) {
                    return Err(bad(
                        e.line,
                        format!("`name: {n}` must be kebab-case (lowercase letters, digits, single `-`), at most {MAX_NAME} characters"),
                    ));
                }
                name = Some(n);
            }
            "description" => description = Some(scalar(e).map_err(|m| bad(e.line, m))?),
            "when-to-use" => when_to_use = Some(scalar(e).map_err(|m| bad(e.line, m))?).filter(|s| !s.is_empty()),
            "invocable-by" => {
                invocable_by = Some(match scalar(e).map_err(|m| bad(e.line, m))?.as_str() {
                    "operator" => Invocable::Operator,
                    "agent" => Invocable::Agent,
                    "both" => Invocable::Both,
                    other => return Err(bad(e.line, format!("`invocable-by: {other}` must be operator, agent, or both"))),
                })
            }
            "disable-model-invocation" => agent_ok = !boolean(e).map_err(|m| bad(e.line, m))?,
            "user-invocable" => operator_ok = boolean(e).map_err(|m| bad(e.line, m))?,
            "context" => {
                context = match scalar(e).map_err(|m| bad(e.line, m))?.as_str() {
                    "inline" | "" => Context::Inline,
                    "fork" => Context::Fork,
                    other => return Err(bad(e.line, format!("`context: {other}` must be inline or fork"))),
                }
            }
            "allowed-tools" => allowed_tools = list(e, split_tools).map_err(|m| bad(e.line, m))?,
            "paths" => {
                paths = list(e, |s| s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect())
                    .map_err(|m| bad(e.line, m))?;
                for p in &paths {
                    globset::Glob::new(p).map_err(|g| bad(e.line, format!("`paths` entry `{p}` is not a valid glob ({g})")))?;
                }
            }
            "model" => model = Some(scalar(e).map_err(|m| bad(e.line, m))?).filter(|m| !m.is_empty() && m != "inherit"),
            "argument-hint" => argument_hint = Some(scalar(e).map_err(|m| bad(e.line, m))?).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    let name = name.ok_or(SkillError::MissingKey { path: path.to_string(), key: "name" })?;
    let description =
        description.filter(|d| !d.trim().is_empty()).ok_or(SkillError::MissingKey { path: path.to_string(), key: "description" })?;
    let invocable_by = match invocable_by {
        Some(i) => i,
        None => match (operator_ok, agent_ok) {
            (true, true) => Invocable::Both,
            (true, false) => Invocable::Operator,
            (false, true) => Invocable::Agent,
            (false, false) => {
                let line = entries.iter().find(|e| e.key.replace('_', "-") == "user-invocable").map(|e| e.line).unwrap_or(1);
                return Err(bad(line, "`user-invocable: false` with `disable-model-invocation: true` leaves nobody able to run it".into()));
            }
        },
    };
    Ok(Template {
        name,
        description: description.trim().to_string(),
        when_to_use,
        invocable_by,
        context,
        allowed_tools,
        paths,
        model,
        argument_hint,
        body: body.trim_matches('\n').trim_end().to_string(),
        base_dir: String::new(),
        resources: Vec::new(),
        origin: CapOrigin::Workspace,
        path: path.to_string(),
    })
}

fn scalar(e: &Entry) -> Result<String, String> {
    match &e.value {
        Val::Scalar(s) => Ok(s.clone()),
        Val::Empty => Ok(String::new()),
        Val::List(_) | Val::Nested => Err(format!("`{}` must be a single value, not a list or mapping", e.key)),
    }
}

fn boolean(e: &Entry) -> Result<bool, String> {
    match scalar(e)?.as_str() {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        other => Err(format!("`{}: {other}` must be true or false", e.key)),
    }
}

fn list(e: &Entry, split: fn(&str) -> Vec<String>) -> Result<Vec<String>, String> {
    match &e.value {
        Val::List(items) => Ok(items.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
        Val::Scalar(s) => Ok(split(s)),
        Val::Empty => Ok(Vec::new()),
        Val::Nested => Err(format!("`{}` must be a list, not a mapping", e.key)),
    }
}

/// `Bash(git add:*), Read Grep` → `["Bash(git add:*)", "Read", "Grep"]`: commas or spaces
/// outside parentheses separate entries.
fn split_tools(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur) = (0i32, String::new());
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' | ' ' | '\t' if depth <= 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Text that goes inside a tag or attribute: no markup can escape the catalog block.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Digest-deduped catalog snapshot (`UserMessage{form: Catalog}`), re-published only when the
/// digest changes. Only agent-invocable templates appear; sorted and deduplicated by name, so
/// the digest does not depend on discovery order. No such template → empty text.
pub fn catalog_snapshot(templates: &[Template]) -> (String, Hash) {
    catalog_snapshot_where(templates, |_| true)
}

/// [`catalog_snapshot`] over the templates `keep` accepts (e.g. conditional skills whose paths matched).
pub fn catalog_snapshot_where(templates: &[Template], keep: impl Fn(&Template) -> bool) -> (String, Hash) {
    let mut shown: Vec<&Template> = templates.iter().filter(|t| t.invocable_by.by_agent() && keep(t)).collect();
    shown.sort_by(|a, b| a.name.cmp(&b.name));
    let mut seen = BTreeSet::new();
    shown.retain(|t| seen.insert(t.name.as_str()));
    if shown.is_empty() {
        return (String::new(), Hash::of(b""));
    }
    let mut text = String::from("<available_skills>\n");
    for t in shown {
        text.push_str(&format!("<skill name=\"{}\">{}", t.name, escape(&one_line(&t.description))));
        if let Some(w) = &t.when_to_use {
            text.push_str(&format!(" When to use: {}", escape(&one_line(w))));
        }
        if let Some(h) = &t.argument_hint {
            text.push_str(&format!(" Arguments: {}", escape(&one_line(h))));
        }
        text.push_str("</skill>\n");
    }
    text.push_str(
        "</available_skills>\nA skill's instructions are not in context until you load them: call the `skill` tool with its name before doing a task it describes.",
    );
    let digest = Hash::of(text.as_bytes());
    (text, digest)
}

/// The `<skill_content>` block with `$ARGUMENTS` replaced by `arguments`. If the body has no
/// `$ARGUMENTS` placeholder, non-empty arguments are appended as `ARGUMENTS: …`.
/// The `skill` tool and `/name` both call this, so the two paths render identically.
pub fn render_invocation(t: &Template, arguments: &str) -> String {
    let args = arguments.trim();
    let body = if t.body.contains("$ARGUMENTS") {
        t.body.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        t.body.clone()
    } else {
        format!("{}\n\nARGUMENTS: {args}", t.body)
    };
    let mut out = format!("<skill_content name=\"{}\">\n<skill_resources>\n", t.name);
    if !t.base_dir.is_empty() {
        out.push_str(&format!("base_dir: {}\n", t.base_dir));
    }
    for r in &t.resources {
        out.push_str(&format!("- {r}\n"));
    }
    out.push_str("</skill_resources>\n<skill_instructions>\n");
    out.push_str(&body);
    out.push_str("\n</skill_instructions>\n</skill_content>");
    out
}

/// Identical render on the `skill` tool path and the `/name` path (no arguments).
pub fn render_content(t: &Template) -> String {
    render_invocation(t, "")
}

/// Conditional activation: `true` when `touched` (workspace-relative; a leading `./` is ignored)
/// matches one of the template's `paths` globs. Templates without `paths` are unconditional and
/// never "match".
pub fn matches_paths(t: &Template, touched: &str) -> bool {
    let touched = touched.strip_prefix("./").unwrap_or(touched);
    t.paths.iter().any(|p| globset::Glob::new(p).map(|g| g.compile_matcher().is_match(touched)).unwrap_or(false))
}

/// Unconditional templates, plus conditional ones a touched path activated.
pub fn is_visible(t: &Template, touched: &[String]) -> bool {
    t.paths.is_empty() || touched.iter().any(|p| matches_paths(t, p))
}

pub fn lookup<'a>(templates: &'a [Template], name: &str) -> Option<&'a Template> {
    templates.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests;
