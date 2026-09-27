//! MCP servers over stdio (JSON-RPC 2.0, newline-delimited).
//!
//! Configuration, one table per server (`Loadout::table("mcp.servers")`):
//!
//! ```toml
//! [mcp.servers.github]
//! cmd = "npx"                      # or a whole command line when `args` is absent
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! env = { GITHUB_TOKEN = "cred:github" }   # `cred:<n>` reads `credentials.<n>`
//! class = "NetworkEgress"          # what the server's tools do; absent → quarantined
//! timeout_ms = 60000               # per request
//! ```
//!
//! At build, servers start in name order: spawn, `initialize` (protocol `2025-06-18`),
//! `notifications/initialized`, then every page of `tools/list`. Each server's generation is built
//! whole before it is installed ([`aho_mcp::Generations`]); a name conflict leaves that server
//! with zero tools. A server without `class` is probed and stopped: its tools are never
//! registered and its status reads `quarantined: unclassified` with the fix. Registered tools
//! are `Registrar::dynamic` specs named `mcp__<server>__<tool>` carrying the server's class;
//! `isError` results come back as `ExecOutcome::Err`.
//!
//! Provides: `McpHandle` (running classified servers, for `tool-mcp-resources`) and the service
//! `"mcp.status"` (`Arc<Vec<ServerStatus>>`, one row per configured server).
//!
//! Servers run through the bound world's `Subprocess` protocol transport, with that world's
//! workspace and environment. A world without separate protocol streams refuses startup.
//! Tool lists are a startup snapshot: a server's `tools/list_changed` takes effect on restart.

#![forbid(unsafe_code)]

mod client;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_mcp::*;
use aho_proc::SubprocessHandle;
use aho_types::*;
use serde_json::json;

pub use client::{ServerInfo, StdioServer};

pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_HANDSHAKE_MS: u64 = 30_000;

/// One `mcp.servers.<name>` row: a spec, or why it cannot start and how to fix it.
#[derive(Clone, Debug)]
pub struct ServerEntry {
    pub name: String,
    pub spec: Result<ServerSpec, (String, String)>,
}

#[derive(Clone, Debug)]
pub struct McpConfig {
    pub servers: Vec<ServerEntry>,
    pub handshake_timeout_ms: u64,
}

/// Plain value or secret for `key`, with `cred:<n>` indirected through `credentials.<n>`.
fn secret_value(l: &Loadout, key: &str, raw: &str) -> Result<String, (String, String)> {
    let Some(name) = raw.strip_prefix("cred:") else { return Ok(raw.to_string()) };
    let target = format!("credentials.{name}");
    if let Some(s) = l.secrets.get(&target) {
        return Ok(s.expose().to_string());
    }
    match l.get(&target) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err((
            format!("`{key}` names credential `{name}`, which is not set; the server was not started"),
            format!("aho credential set {name}"),
        )),
    }
}

/// Splits a command line on whitespace, honoring `"…"` and `'…'` quoting.
fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quote, mut any) = (String::new(), None::<char>, false);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn class_names() -> String {
    Consequence::ALL.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
}

/// Parses `mcp.servers.<name>`; errors carry `(what happened, fix)`.
pub fn server_spec(l: &Loadout, name: &str, row: &Value) -> Result<ServerSpec, (String, String)> {
    let key = |k: &str| format!("mcp.servers.{name}.{k}");
    let Value::Object(row) = row else {
        return Err((
            format!("`mcp.servers.{name}` must be a table; the server was not started"),
            format!("aho set {} <program>", key("cmd")),
        ));
    };
    if row.contains_key("url") {
        return Err((
            format!("`{}` is set, but this build speaks MCP over stdio only; the server was not started", key("url")),
            format!("aho set {} <program>", key("cmd")),
        ));
    }
    let cmd_line = match row.get("cmd") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return Err((format!("`{}` is not set; the server was not started", key("cmd")), format!("aho set {} <program>", key("cmd")))),
    };
    let (cmd, args) = match row.get("args") {
        None | Some(Value::Null) => {
            let mut parts = split_command(&cmd_line);
            let cmd = if parts.is_empty() { cmd_line.clone() } else { parts.remove(0) };
            (cmd, parts)
        }
        Some(Value::Array(a)) => {
            let args = a.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>().ok_or_else(|| {
                (
                    format!("`{}` must be an array of strings; the server was not started", key("args")),
                    format!("aho set {} '[\"--flag\"]'", key("args")),
                )
            })?;
            (cmd_line.clone(), args)
        }
        Some(_) => {
            return Err((
                format!("`{}` must be an array of strings; the server was not started", key("args")),
                format!("aho set {} '[\"--flag\"]'", key("args")),
            ));
        }
    };
    let mut env = Vec::new();
    match row.get("env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(vars)) => {
            for (var, v) in vars {
                let k = format!("{}.{var}", key("env"));
                let Some(raw) = v.as_str() else {
                    return Err((format!("`{k}` must be a string; the server was not started"), format!("aho set {k} <value>")));
                };
                env.push((var.clone(), secret_value(l, &k, raw)?));
            }
        }
        Some(_) => {
            return Err((
                format!("`{}` must be a table of strings; the server was not started", key("env")),
                format!("aho set {}.<VAR> <value>", key("env")),
            ));
        }
    }
    let class = match row.get("class") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(Consequence::parse(s).ok_or_else(|| {
            (
                format!("`{} = {s}` is not a consequence class ({}); the server was not started", key("class"), class_names()),
                format!("aho set {} NetworkEgress", key("class")),
            )
        })?),
        Some(_) => {
            return Err((
                format!("`{}` must be a class name ({})", key("class"), class_names()),
                format!("aho set {} NetworkEgress", key("class")),
            ));
        }
    };
    let timeout_ms = match row.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
            (
                format!("`{}` must be a positive number of milliseconds", key("timeout_ms")),
                format!("aho set {} {DEFAULT_TIMEOUT_MS}", key("timeout_ms")),
            )
        })?,
    };
    Ok(ServerSpec { name: name.to_string(), cmd, args, env, class, timeout_ms })
}

impl Config for McpConfig {
    const PREFIX: &'static str = "mcp";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "mcp.servers",
                Ty::Table,
                None,
                "MCP servers over stdio: `mcp.servers.<name>.{cmd, args, env, class, timeout_ms}`; `env` values may be `cred:<name>`; without `class` a server is quarantined.",
            ),
            ConfigKey::new(
                "mcp.handshake_timeout_ms",
                Ty::Int,
                Some(json!(DEFAULT_HANDSHAKE_MS)),
                "Time limit for a server to answer `initialize`.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let handshake_timeout_ms = match l.get("mcp.handshake_timeout_ms") {
            None => DEFAULT_HANDSHAKE_MS,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::Type { key: "mcp.handshake_timeout_ms".into(), expected: "a positive integer" })?,
        };
        let servers =
            l.table("mcp.servers").into_iter().map(|(name, row)| ServerEntry { spec: server_spec(l, &name, &row), name }).collect();
        Ok(McpConfig { servers, handshake_timeout_ms })
    }
}

struct Registered {
    spec: ToolSpec,
    server: StdioServer,
    raw: String,
}

pub struct McpStdio {
    tools: Vec<Registered>,
    directory: Arc<McpDirectory>,
    status: Arc<Vec<ServerStatus>>,
}

fn failed(name: &str, class: Option<Consequence>, error: String, fix: Option<String>) -> ServerStatus {
    ServerStatus { name: name.to_string(), class, state: ServerState::Failed { error }, tools: vec![], fix }
}

impl McpStdio {
    fn start(proc: SubprocessHandle, cfg: McpConfig) -> McpStdio {
        let handshake = Duration::from_millis(cfg.handshake_timeout_ms);
        let mut gens = Generations::default();
        let (mut tools, mut running, mut status) = (Vec::new(), Vec::<Arc<dyn McpServer>>::new(), Vec::new());
        for entry in cfg.servers {
            let name = entry.name;
            let spec = match entry.spec {
                Ok(s) => s,
                Err((error, fix)) => {
                    status.push(failed(&name, None, error, Some(fix)));
                    continue;
                }
            };
            let class = spec.class;
            let server = match StdioServer::start(&proc, &spec, handshake) {
                Ok(s) => s,
                Err(e) => {
                    status.push(failed(&name, class, e.to_string(), Some(format!("aho explain mcp.servers.{name}.cmd"))));
                    continue;
                }
            };
            let listed = server.list_tools_blocking();
            let Some(class) = class else {
                // Quarantine: probe, report what it would expose, stop it.
                let (tools, reason) = match &listed {
                    Ok(ts) => (ts.iter().map(|t| normalize(&name, &t.raw_name)).collect(), "unclassified".to_string()),
                    Err(e) => (vec![], format!("unclassified; probing its tools failed: {e}")),
                };
                status.push(ServerStatus {
                    name: name.clone(),
                    class: None,
                    state: ServerState::Quarantined { reason },
                    tools,
                    fix: Some(format!("aho set mcp.servers.{name}.class NetworkEgress")),
                });
                drop(server);
                continue;
            };
            match gens.swap(&name, listed) {
                Ok(generation) => {
                    for (normalized, t) in &generation.tools {
                        tools.push(Registered {
                            spec: ToolSpec {
                                name: normalized.clone(),
                                description: clip_description(&t.description),
                                input_schema: t.schema.clone(),
                                class,
                            },
                            server: server.clone(),
                            raw: t.raw_name.clone(),
                        });
                    }
                    let names = generation.tools.iter().map(|(n, _)| n.clone()).collect();
                    status.push(ServerStatus {
                        name: name.clone(),
                        class: Some(class),
                        state: ServerState::Ready,
                        tools: names,
                        fix: None,
                    });
                    running.push(Arc::new(server));
                }
                Err(e @ McpError::NameConflict { .. }) => {
                    status.push(ServerStatus {
                        name: name.clone(),
                        class: Some(class),
                        state: ServerState::Conflict { error: e.to_string() },
                        tools: vec![],
                        fix: Some(format!("rename the server: move `mcp.servers.{name}` to a different name")),
                    });
                    running.push(Arc::new(server));
                }
                Err(e) => status.push(failed(
                    &name,
                    Some(class),
                    format!("listing its tools failed: {e}"),
                    Some(format!("aho explain mcp.servers.{name}")),
                )),
            }
        }
        McpStdio { tools, directory: Arc::new(McpDirectory::new(running)), status: Arc::new(status) }
    }
}

impl Capability for McpStdio {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = (SubprocessHandle,);
    type Config = McpConfig;

    fn build((proc,): Self::Deps, cfg: McpConfig) -> Result<Self, BuildError> {
        Ok(McpStdio::start(proc, cfg))
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for t in &self.tools {
            let (server, raw) = (t.server.clone(), t.raw.clone());
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let (server, raw) = (server.clone(), raw.clone());
                Box::pin(async move {
                    match server.call(&raw, req.view.args.clone(), req.cancel.clone()).await {
                        Ok(result) => call_outcome(&result),
                        Err(McpError::Cancelled) => ExecOutcome::Cancelled,
                        Err(e) => ExecOutcome::err(e.code(), e.to_string()),
                    }
                })
            });
            r.dynamic(t.spec.clone(), exec);
        }
        r.provide(McpHandle(self.directory.clone()));
        r.service(STATUS_SERVICE, self.status.clone());
    }
}

declare_capability!(
    McpStdio,
    "mcp-stdio",
    kind = Kind::Provider,
    provides = [aho_mcp::SEAM],
    requires = [Requirement::Seam(aho_proc::SEAM)],
    tools = &[],
    provides_handles = [HandleKind::Custom("mcp")],
    doc = "MCP servers over stdio: `mcp.servers.<name>`; tools `mcp__<server>__<tool>` under the server's class; unclassified servers quarantined."
);

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn command_lines_split_on_whitespace_outside_quotes() {
        assert_eq!(split_command("npx -y @modelcontextprotocol/server-github"), vec!["npx", "-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(split_command("  run \"a b\" 'c d' '' e"), vec!["run", "a b", "c d", "", "e"]);
        assert_eq!(split_command("single"), vec!["single"]);
    }
}
