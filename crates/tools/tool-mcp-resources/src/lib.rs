//! `10` — MCP resources as three shared tools: `list_mcp_resources`,
//! `list_mcp_resource_templates`, `read_mcp_resource`.
//!
//! They reach the running, classified servers in the `McpHandle` directory (quarantined servers
//! are never in it). Each call is gated by the server's class: when the current policy does not
//! allow it, a `pre_execute` asks the operator first ("this reads from `github`, declared
//! NetworkEgress, which the preset does not allow"). Binary blobs render as
//! `[binary resource: N base64 characters; available to programmatic callers]`; the full
//! payload (including the base64) is the result's `value`.
//!
//! The tools stay registered while this capability is active; with no MCP server running they
//! answer with how to add one (a static manifest cannot come and go with servers).

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::*;
use aho_mcp::{McpDirectory, McpError, McpHandle, McpServer, Resource, ResourceTemplate, render_contents};
use aho_types::*;
use serde_json::json;

pub const LIST: &str = "list_mcp_resources";
pub const TEMPLATES: &str = "list_mcp_resource_templates";
pub const READ: &str = "read_mcp_resource";

fn list_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"server": {"type": "string", "description": "Only this MCP server; default: every server."}},
        "additionalProperties": false
    }))
}

fn read_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "server": {"type": "string", "description": "MCP server name."},
            "uri": {"type": "string", "description": "Resource URI from list_mcp_resources (or a filled-in template)."}
        },
        "required": ["server", "uri"],
        "additionalProperties": false
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(LIST, "List the resources MCP servers offer (uri, name, type).", Consequence::ReadOnly).safe().schema(list_schema),
    ToolDecl::new(TEMPLATES, "List MCP resource URI templates (fill in the {placeholders}, then read).", Consequence::ReadOnly)
        .safe()
        .schema(list_schema),
    ToolDecl::new(READ, "Read one MCP resource by server and uri.", Consequence::ReadOnly).safe().schema(read_schema),
];

pub struct ResourceDeps {
    pub mcp: McpHandle,
}

impl Deps for ResourceDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Custom("mcp")];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ResourceDeps { mcp: r.handle()? })
    }
}

pub struct ToolMcpResources {
    dir: Arc<McpDirectory>,
}

const NO_SERVERS: &str = "No MCP server is running. The operator can add one with `aho add mcp <name> --cmd <program> --class <Class>`.";

fn servers<'a>(dir: &'a McpDirectory, args: &Value) -> Result<Vec<&'a Arc<dyn McpServer>>, ExecOutcome> {
    match args.get("server") {
        None | Some(Value::Null) => Ok(dir.servers().iter().collect()),
        Some(Value::String(name)) => match dir.get(name) {
            Some(s) => Ok(vec![s]),
            None => Err(unknown_server(dir, name)),
        },
        Some(_) => Err(ExecOutcome::err("BAD_ARGS", "`server` must be a string; nothing was read.")),
    }
}

fn unknown_server(dir: &McpDirectory, name: &str) -> ExecOutcome {
    let names = dir.names();
    let known = if names.is_empty() { "none is running".to_string() } else { format!("running: {}", names.join(", ")) };
    ExecOutcome::err("MCP_UNKNOWN_SERVER", format!("no running MCP server is named `{name}` ({known}); nothing was read."))
}

/// One line per resource: `- <uri>  <name> (<mime>) — <description>`.
pub fn resource_line(r: &Resource) -> String {
    let mut s = format!("- {}", r.uri);
    let label = r.title.as_deref().unwrap_or(&r.name);
    if !label.is_empty() {
        s.push_str(&format!("  {label}"));
    }
    if let Some(m) = &r.mime {
        s.push_str(&format!(" ({m})"));
    }
    if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
        s.push_str(&format!(" — {d}"));
    }
    s
}

pub fn template_line(t: &ResourceTemplate) -> String {
    let mut s = format!("- {}", t.uri_template);
    let label = t.title.as_deref().unwrap_or(&t.name);
    if !label.is_empty() {
        s.push_str(&format!("  {label}"));
    }
    if let Some(m) = &t.mime {
        s.push_str(&format!(" ({m})"));
    }
    if let Some(d) = t.description.as_deref().filter(|d| !d.is_empty()) {
        s.push_str(&format!(" — {d}"));
    }
    s
}

/// Per-server sections; a server that failed or has no resources says so in its section.
fn sectioned(sections: Vec<(String, Result<Vec<String>, McpError>)>, empty: &str) -> String {
    sections
        .into_iter()
        .map(|(server, r)| match r {
            Ok(lines) if lines.is_empty() => format!("server `{server}`: {empty}"),
            Ok(lines) => format!("server `{server}`:\n{}", lines.join("\n")),
            Err(McpError::Unsupported(_)) => format!("server `{server}`: does not offer resources"),
            Err(e) => format!("server `{server}`: {e}"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn list(dir: Arc<McpDirectory>, args: Value) -> ExecOutcome {
    let targets = match servers(&dir, &args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if targets.is_empty() {
        return ExecOutcome::Done { value: json!([]), content: vec![ContentBlock::text(NO_SERVERS)], meta: None };
    }
    let (mut value, mut sections) = (Vec::new(), Vec::new());
    for s in targets {
        let r = s.list_resources().await;
        if let Ok(rs) = &r {
            value.extend(rs.iter().map(|x| json!({"server": s.name(), "uri": x.uri, "name": x.name, "title": x.title, "description": x.description, "mimeType": x.mime})));
        }
        sections.push((s.name().to_string(), r.map(|rs| rs.iter().map(resource_line).collect())));
    }
    ExecOutcome::Done { value: Value::Array(value), content: vec![ContentBlock::text(sectioned(sections, "no resources"))], meta: None }
}

async fn templates(dir: Arc<McpDirectory>, args: Value) -> ExecOutcome {
    let targets = match servers(&dir, &args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if targets.is_empty() {
        return ExecOutcome::Done { value: json!([]), content: vec![ContentBlock::text(NO_SERVERS)], meta: None };
    }
    let (mut value, mut sections) = (Vec::new(), Vec::new());
    for s in targets {
        let r = s.list_resource_templates().await;
        if let Ok(ts) = &r {
            value.extend(ts.iter().map(|x| json!({"server": s.name(), "uriTemplate": x.uri_template, "name": x.name, "title": x.title, "description": x.description, "mimeType": x.mime})));
        }
        sections.push((s.name().to_string(), r.map(|ts| ts.iter().map(template_line).collect())));
    }
    ExecOutcome::Done {
        value: Value::Array(value),
        content: vec![ContentBlock::text(sectioned(sections, "no resource templates"))],
        meta: None,
    }
}

async fn read(dir: Arc<McpDirectory>, args: Value) -> ExecOutcome {
    let (Some(server), Some(uri)) = (args.get("server").and_then(Value::as_str), args.get("uri").and_then(Value::as_str)) else {
        return ExecOutcome::err("BAD_ARGS", "`server` and `uri` are required strings; nothing was read.");
    };
    let Some(s) = dir.get(server) else { return unknown_server(&dir, server) };
    match s.read_resource(uri).await {
        Ok(contents) => ExecOutcome::Done {
            value: json!({"server": server, "uri": uri, "contents": contents.iter().map(|c| c.to_json()).collect::<Vec<_>>()}),
            content: vec![ContentBlock::text(render_contents(&contents))],
            meta: None,
        },
        Err(e) => ExecOutcome::err(e.code(), format!("reading `{uri}` from `{server}` failed: {e}")),
    }
}

/// The class gate: a call reaching a server whose class the policy does not allow asks first.
pub fn gate(dir: &McpDirectory, view: &ExecView) -> PreDecision {
    if ![LIST, TEMPLATES, READ].contains(&view.tool.as_str()) {
        return PreDecision::Allow;
    }
    let targets: Vec<&Arc<dyn McpServer>> = match view.args.get("server").and_then(Value::as_str) {
        Some(name) => dir.get(name).into_iter().collect(),
        None => dir.servers().iter().collect(),
    };
    let over: Vec<String> = targets
        .iter()
        .filter_map(|s| s.class().filter(|c| !view.policy.allowed.contains(*c)).map(|c| format!("`{}` ({})", s.name(), c.name())))
        .collect();
    if over.is_empty() {
        PreDecision::Allow
    } else {
        PreDecision::Ask {
            reason: Some(format!(
                "{} talks to MCP server {}, a class the current preset does not allow; approve to let it through once.",
                view.tool,
                over.join(", ")
            )),
        }
    }
}

impl Capability for ToolMcpResources {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ResourceDeps;
    type Config = ();

    fn build(deps: ResourceDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolMcpResources { dir: deps.mcp.0 })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let (dir, which) = (self.dir.clone(), decl.name);
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let (dir, args) = (dir.clone(), req.view.args);
                let fut: BoxFuture<'static, ExecOutcome> = match which {
                    LIST => Box::pin(list(dir, args)),
                    TEMPLATES => Box::pin(templates(dir, args)),
                    _ => Box::pin(read(dir, args)),
                };
                fut
            });
            r.tool(decl, exec);
        }
        let dir = self.dir.clone();
        r.pre_execute(Arc::new(move |view: &ExecView| gate(&dir, view)));
    }
}

declare_capability!(
    ToolMcpResources,
    "tool-mcp-resources",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_mcp::SEAM)],
    tools = TOOLS,
    doc = "list_mcp_resources, list_mcp_resource_templates, read_mcp_resource over running MCP servers; blobs never enter model text (10)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_show_label_type_and_description() {
        let r = Resource {
            uri: "mem://a".into(),
            name: "a".into(),
            title: Some("Alpha".into()),
            description: Some("first".into()),
            mime: Some("text/plain".into()),
        };
        assert_eq!(resource_line(&r), "- mem://a  Alpha (text/plain) — first");
        let bare = Resource { uri: "mem://b".into(), name: String::new(), title: None, description: None, mime: None };
        assert_eq!(resource_line(&bare), "- mem://b");
        let t = ResourceTemplate {
            uri_template: "mem://n/{id}".into(),
            name: "note".into(),
            title: None,
            description: Some("by id".into()),
            mime: None,
        };
        assert_eq!(template_line(&t), "- mem://n/{id}  note — by id");
    }

    #[test]
    fn sections_report_failures_and_unsupported_servers_in_place() {
        let text = sectioned(
            vec![
                ("a".into(), Ok(vec!["- x".into()])),
                ("b".into(), Ok(vec![])),
                ("c".into(), Err(McpError::Unsupported("no".into()))),
                ("d".into(), Err(McpError::Timeout { method: "resources/list".into(), ms: 5 })),
            ],
            "no resources",
        );
        assert_eq!(
            text,
            "server `a`:\n- x\n\nserver `b`: no resources\n\nserver `c`: does not offer resources\n\nserver `d`: the MCP server did not answer `resources/list` within 5 ms; the request was cancelled. Raise `timeout_ms` for this server or retry."
        );
    }

    #[test]
    fn gate_passes_other_tools_and_empty_directories() {
        let dir = McpDirectory::default();
        let view = ExecView {
            call: CallId("c".into()),
            tool: READ.into(),
            class: Consequence::ReadOnly,
            args: json!({"server": "x", "uri": "u"}),
            policy: PolicyView {
                epoch: Epoch(1),
                allowed: ConsequenceSet::of(&[Consequence::ReadOnly]),
                world: BoundWorld::host(),
                workspace: String::new(),
                home: String::new(),
            },
            by: Principal::Operator,
        };
        assert_eq!(gate(&dir, &view), PreDecision::Allow, "an unknown server is the tool's error, not the gate's");
        assert_eq!(gate(&dir, &ExecView { tool: "bash".into(), ..view }), PreDecision::Allow);
    }
}
