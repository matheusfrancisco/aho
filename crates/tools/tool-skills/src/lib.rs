//! The `skill` tool and the skill catalog context.
//!
//! * `skill{name, arguments?}` (ReadOnly, concurrency-safe) returns the template's
//!   `<skill_content name=..><skill_resources>..</skill_resources><skill_instructions>..
//!   </skill_instructions></skill_content>` with `$ARGUMENTS` substituted: the same render
//!   [`slash_invocation`] produces for `/name`.
//! * A `Registrar::context` publishes the `<available_skills>` catalog only when its digest
//!   changes. Conditional skills (`paths:`) join the catalog once a tool call touched a matching
//!   path (`path`, `file_path` or `paths` arguments in the session's calls).
//! * [`slash_invocation`] renders `/name args` for operator-invocable skills. The session calls it
//!   for genuine user messages only, so tool output can never forge an invocation.
//!
//! Templates come from the `"skills"` service (`Arc<Vec<Template>>`, e.g. `skills-fsdir`).
//! `context: fork` is reported in the result's value (`"context": "fork"`) for a subagent runner
//! to honor; this tool always returns the instructions inline.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use aho_cap::*;
use aho_log::State;
use aho_skills::{Template, catalog_snapshot_where, is_visible, render_invocation};
use aho_types::*;
use serde_json::json;

fn skill_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "Skill name from <available_skills>."},
            "arguments": {"type": "string", "description": "Optional text substituted for $ARGUMENTS in the skill's instructions."}
        },
        "required": ["name"],
        "additionalProperties": false
    }))
}

fn present_call(args: &Value) -> Card {
    Card::Generic { title: "skill".into(), body: args.get("name").and_then(Value::as_str).unwrap_or("?").to_string() }
}

fn present_result(args: &Value, _: &Value) -> Card {
    Card::Generic { title: "skill loaded".into(), body: args.get("name").and_then(Value::as_str).unwrap_or("?").to_string() }
}

pub static TOOLS: &[ToolDecl] = &[ToolDecl::new(
    "skill",
    "Load a skill's full instructions by name (see <available_skills>). Pass `arguments` to fill the skill's $ARGUMENTS.",
    Consequence::ReadOnly,
)
.safe()
.schema(skill_schema)
.max_inline(None)
.present(Present { call: present_call, result: present_result })];

/// The templates published by the skills provider.
pub struct SkillsDeps {
    pub templates: Arc<Vec<Template>>,
}

impl Deps for SkillsDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        r.service::<Arc<Vec<Template>>>(aho_skills::SERVICE)
            .map(|templates| SkillsDeps { templates })
            .ok_or(MissingDep(HandleKind::Custom("skills")))
    }
}

pub struct ToolSkills {
    templates: Arc<Vec<Template>>,
}

/// `/name args` from a genuine user message → the rendered skill, if `name` is an
/// operator-invocable template. Anything else (no leading `/`, unknown name, agent-only) → `None`.
pub fn slash_invocation(user_text: &str, templates: &[Template]) -> Option<String> {
    let rest = user_text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, args) = rest.split_at(end);
    let t = templates.iter().find(|t| t.name == name && t.invocable_by.by_operator())?;
    Some(render_invocation(t, args))
}

/// Paths the session's tool calls named (`path`, `file_path`, `paths[]`).
pub fn touched_paths(state: &State) -> Vec<String> {
    let mut out = Vec::new();
    for (_, _, args) in state.calls.values() {
        for key in ["path", "file_path"] {
            if let Some(p) = args.get(key).and_then(Value::as_str) {
                out.push(p.to_string());
            }
        }
        if let Some(ps) = args.get("paths").and_then(Value::as_array) {
            out.extend(ps.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn exec(templates: Arc<Vec<Template>>, args: &Value) -> ExecOutcome {
    let Some(name) = args.get("name").and_then(Value::as_str) else {
        return ExecOutcome::err("BAD_ARGS", "`name` is required and must be a string; nothing was loaded.");
    };
    let arguments = match args.get("arguments") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return ExecOutcome::err("BAD_ARGS", "`arguments` must be a string; nothing was loaded."),
    };
    let Some(t) = templates.iter().find(|t| t.name == name) else {
        let mut names: Vec<&str> = templates.iter().filter(|t| t.invocable_by.by_agent()).map(|t| t.name.as_str()).collect();
        names.sort();
        let list = if names.is_empty() { "none are active".to_string() } else { format!("active skills: {}", names.join(", ")) };
        return ExecOutcome::err(
            "SKILL_NOT_FOUND",
            format!("no active skill is named `{name}` ({list}); nothing was loaded. Use a name from <available_skills>."),
        );
    };
    if !t.invocable_by.by_agent() {
        return ExecOutcome::err(
            "SKILL_OPERATOR_ONLY",
            format!("`{name}` can only be started by the operator (`/{name}`); nothing was loaded. Ask the operator to run it."),
        );
    }
    let text = render_invocation(t, &arguments);
    ExecOutcome::Done {
        value: json!({
            "name": t.name,
            "arguments": arguments,
            "base_dir": t.base_dir,
            "context": t.context.name(),
            "allowed_tools": t.allowed_tools,
            "model": t.model,
        }),
        content: vec![ContentBlock::text(text)],
        meta: Some(Card::Generic { title: "skill loaded".into(), body: t.name.clone() }),
    }
}

impl Capability for ToolSkills {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = SkillsDeps;
    type Config = ();

    fn build(deps: SkillsDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolSkills { templates: deps.templates })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let templates = self.templates.clone();
        let tool: ToolExec = Arc::new(move |req: ExecRequest| {
            let templates = templates.clone();
            Box::pin(async move { exec(templates, &req.view.args) })
        });
        r.tool(&TOOLS[0], tool);

        let templates = self.templates.clone();
        let last: Mutex<Option<Hash>> = Mutex::new(None);
        r.context(Arc::new(move |state: &State| {
            let touched = touched_paths(state);
            let (text, digest) = catalog_snapshot_where(&templates, |t| is_visible(t, &touched));
            let mut seen = last.lock().unwrap_or_else(|p| p.into_inner());
            if text.is_empty() || *seen == Some(digest) {
                return None;
            }
            *seen = Some(digest);
            Some(text)
        }));
    }
}

declare_capability!(
    ToolSkills,
    "tool-skills",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_skills::SEAM)],
    tools = TOOLS,
    doc = "`skill` tool and the digest-deduped <available_skills> catalog."
);

#[cfg(test)]
mod tests;
