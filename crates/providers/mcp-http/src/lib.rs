//! MCP servers over Streamable HTTP (spec revision `2025-06-18`, `basic/transports`).
//!
//! Configuration shares `mcp.servers` with `mcp-stdio`; a row with `url` is an HTTP server
//! (`Loadout::table("mcp.servers")`):
//!
//! ```toml
//! [mcp.servers.linear]
//! url = "https://mcp.linear.app/mcp"   # the single MCP endpoint (http or https)
//! bearer = "cred:linear"               # `Authorization: Bearer <credentials.linear>`
//! headers = { X-Team = "core" }        # extra headers; values may be `cred:<name>`
//! class = "NetworkEgress"              # what the server's tools do; absent → quarantined
//! timeout_ms = 60000                   # per request
//! ```
//!
//! At build every HTTP server is probed concurrently on the provider's own I/O thread
//! ([`IoRuntime`]): POST `initialize`, keep `Mcp-Session-Id`, POST `notifications/initialized`,
//! then every page of `tools/list`. Generations install in name order through one
//! [`ToolTable`] (two-phase, `aho_mcp::Generations`), checked against every other HTTP server and
//! against the tools `mcp-stdio` already registered; a conflict leaves that server with zero
//! tools. A server without `class` is probed, reported `quarantined: unclassified`, and its session
//! deleted. Classified servers then open the GET notification stream; `tools/list_changed`
//! re-lists and swaps the generation. Registered tools (`Registrar::dynamic`, the server's class)
//! resolve their raw name through the current generation at call time, so a tool the server
//! withdrew fails with `MCP_TOOL_GONE` without being sent; tools it added reach the model when the
//! session restarts (the registry is fixed per session).
//!
//! The destination is checked against the operator's egress list (`egress.allow` /
//! `egress.deny`) before connecting and on every redirect hop. Like stdio servers, HTTP
//! servers are reached from the host, not through the bound world: the declared class is
//! what policy sees. Authentication is a bearer token or headers from `cred:<name>`; the OAuth
//! authorization flow is not implemented.
//!
//! Provides: `McpHandle` and `"mcp.status"`, merged with what `mcp-stdio` provided so
//! `tool-mcp-resources` reaches servers of both kinds and the status service has one row per
//! configured server; plus [`SERVERS_SERVICE`] (`Arc<Vec<HttpServer>>`, live generations and
//! notification-stream state). Merging needs this capability to build after `mcp-stdio`: it
//! declares the `mcp` handle in `Deps` (resolved optionally), so the plan's dependency order
//! (which `Composition::start` builds in) puts it after stdio.

#![forbid(unsafe_code)]

mod client;
mod runtime;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_egress::HostList;
use aho_mcp::*;
use aho_types::*;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;

pub use client::{ConnectError, HttpServer, Listening, MAX_RESPONSE_BYTES, SESSION_HEADER, VERSION_HEADER, shown_url};
pub use runtime::{IoRuntime, ToolTable};
pub use wire::ServerInfo;

pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_HANDSHAKE_MS: u64 = 30_000;
/// Service holding the running HTTP servers: `Arc<Vec<HttpServer>>`.
pub const SERVERS_SERVICE: &str = "mcp.http.servers";
/// Redirect hops followed (each checked against the egress list).
pub const MAX_REDIRECTS: usize = 5;
/// Headers aho sets itself; `headers` may not override them.
const RESERVED_HEADERS: &[&str] =
    &["accept", "content-type", "content-length", "host", "mcp-session-id", "mcp-protocol-version", "last-event-id"];

/// One `mcp.servers.<name>` row with `url`. `Debug` never prints header values, the token, or the
/// URL's query (any may hold credentials).
#[derive(Clone)]
pub struct HttpServerSpec {
    pub name: String,
    pub url: String,
    /// Extra headers; `cred:<name>` values are already resolved.
    pub headers: Vec<(String, String)>,
    /// Resolved bearer token.
    pub bearer: Option<String>,
    /// The credential behind the token or a header (named in fixes).
    pub credential: Option<String>,
    /// Required for the server's tools to be model-visible; otherwise `quarantined: unclassified`.
    pub class: Option<Consequence>,
    /// Per-request limit for this server.
    pub timeout_ms: u64,
}

impl fmt::Debug for HttpServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = reqwest::Url::parse(&self.url).map(|u| shown_url(&u)).unwrap_or_else(|_| "(not a URL)".into());
        f.debug_struct("HttpServerSpec")
            .field("name", &self.name)
            .field("url", &url)
            .field("headers", &self.headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>())
            .field("bearer", &self.bearer.as_ref().map(|_| "(set)"))
            .field("credential", &self.credential)
            .field("class", &self.class)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

impl HttpServerSpec {
    /// Operator headers plus `Authorization`, every value marked sensitive. Errors carry
    /// `(what happened, fix)`.
    pub fn header_map(&self) -> Result<HeaderMap, (String, String)> {
        let key = format!("mcp.servers.{}.headers", self.name);
        let mut map = HeaderMap::new();
        let pairs = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(self.bearer.iter().map(|t| ("authorization".to_string(), format!("Bearer {t}"))));
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| {
                (format!("`{key}.{k}` is not a valid header name; the server was not started"), format!("rename `{k}` in `{key}`"))
            })?;
            let mut value = HeaderValue::from_str(&v).map_err(|_| {
                (format!("the value for header `{k}` of MCP server `{}` has characters HTTP headers cannot carry; the server was not started", self.name), format!("aho set {key}.{k} <value>"))
            })?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        Ok(map)
    }

    /// Whether any credential-like header is sent (for refusal messages).
    pub fn sends_auth(&self) -> bool {
        self.bearer.is_some() || self.headers.iter().any(|(h, _)| h.eq_ignore_ascii_case("authorization"))
    }
}

/// One HTTP row: a spec, or why it cannot start and how to fix it.
#[derive(Clone, Debug)]
pub struct ServerEntry {
    pub name: String,
    pub spec: Result<HttpServerSpec, (String, String)>,
}

#[derive(Clone, Debug)]
pub struct McpHttpConfig {
    pub servers: Vec<ServerEntry>,
    pub handshake_timeout_ms: u64,
    pub hosts: HostList,
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

fn class_names() -> String {
    Consequence::ALL.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
}

/// Parses `mcp.servers.<name>` when it is an HTTP row (has `url`); `None` leaves it to `mcp-stdio`.
pub fn server_spec(l: &Loadout, hosts: &HostList, name: &str, row: &Value) -> Option<Result<HttpServerSpec, (String, String)>> {
    let Value::Object(row) = row else { return None };
    let url = row.get("url")?;
    Some(parse_row(l, hosts, name, row, url))
}

fn parse_row(
    l: &Loadout,
    hosts: &HostList,
    name: &str,
    row: &serde_json::Map<String, Value>,
    url: &Value,
) -> Result<HttpServerSpec, (String, String)> {
    let key = |k: &str| format!("mcp.servers.{name}.{k}");
    let url_fix = format!("aho set {} https://<host>/mcp", key("url"));
    let Value::String(url) = url else {
        return Err((format!("`{}` must be a string; the server was not started", key("url")), url_fix));
    };
    let url = url.trim().to_string();
    if let Some(k) = ["cmd", "args", "env"].into_iter().find(|k| row.contains_key(*k)) {
        return Err((
            format!(
                "`{}` and `{}` are both set; `{k}` configures stdio servers and `url` HTTP servers, so the server was not started",
                key("url"),
                key(k)
            ),
            format!("remove `{k}` (or `url`) from `[mcp.servers.{name}]`"),
        ));
    }
    let parsed = aho_egress::parse_url(&url).map_err(|why| {
        (format!("`{}` is not a URL aho will connect to: {why}; the server was not started", key("url")), url_fix.clone())
    })?;
    let u = reqwest::Url::parse(&url)
        .map_err(|e| (format!("`{}` is not a URL ({e}); the server was not started", key("url")), url_fix.clone()))?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err((
            format!("`{}` carries user info; the server was not started. Put credentials in `bearer` or `headers`.", key("url")),
            format!("aho set {} cred:{name}", key("bearer")),
        ));
    }
    hosts.check_host(&parsed.host).map_err(|why| {
        let denied = hosts.deny.iter().any(|p| aho_egress::glob_match(p, &parsed.host));
        (
            format!("MCP server `{name}` was not started: {why}"),
            format!("aho explain {}", if denied { aho_egress::DENY_KEY } else { aho_egress::ALLOW_KEY }),
        )
    })?;
    let mut headers = Vec::new();
    let mut credential = None;
    match row.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(hs)) => {
            for (h, v) in hs {
                let k = format!("{}.{h}", key("headers"));
                let Some(raw) = v.as_str() else {
                    return Err((format!("`{k}` must be a string; the server was not started"), format!("aho set {k} <value>")));
                };
                if RESERVED_HEADERS.contains(&h.to_ascii_lowercase().as_str()) {
                    return Err((
                        format!("`{k}` sets a header aho manages itself; the server was not started"),
                        format!("remove `{h}` from `{}`", key("headers")),
                    ));
                }
                if credential.is_none() {
                    credential = raw.strip_prefix("cred:").map(str::to_string);
                }
                headers.push((h.clone(), secret_value(l, &k, raw)?));
            }
        }
        Some(_) => {
            return Err((
                format!("`{}` must be a table of strings; the server was not started", key("headers")),
                format!("aho set {}.<Header> <value>", key("headers")),
            ));
        }
    }
    let bearer = match row.get("bearer") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            if headers.iter().any(|(h, _)| h.eq_ignore_ascii_case("authorization")) {
                return Err((
                    format!("both `{}` and an `Authorization` header are set; the server was not started", key("bearer")),
                    format!("remove `Authorization` from `{}`", key("headers")),
                ));
            }
            if let Some(c) = raw.strip_prefix("cred:") {
                credential = Some(c.to_string());
            }
            Some(secret_value(l, &key("bearer"), raw)?)
        }
        Some(_) => {
            return Err((
                format!("`{}` must be a string (`cred:<name>`); the server was not started", key("bearer")),
                format!("aho set {} cred:{name}", key("bearer")),
            ));
        }
    };
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
    let spec = HttpServerSpec { name: name.to_string(), url, headers, bearer, credential, class, timeout_ms };
    spec.header_map()?;
    Ok(spec)
}

impl Config for McpHttpConfig {
    const PREFIX: &'static str = "mcp";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = vec![
            ConfigKey::new(
                "mcp.servers",
                Ty::Table,
                None,
                "MCP servers: stdio `mcp.servers.<name>.{cmd, args, env}` or Streamable HTTP `mcp.servers.<name>.{url, bearer, headers}`, plus `class` and `timeout_ms`; `env`/`bearer`/`headers` values may be `cred:<name>`; without `class` a server is quarantined.",
            ),
            ConfigKey::new(
                "mcp.handshake_timeout_ms",
                Ty::Int,
                Some(json!(DEFAULT_HANDSHAKE_MS)),
                "Time limit for a server to answer `initialize`.",
            ),
        ];
        keys.extend(aho_egress::config_keys());
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let handshake_timeout_ms = match l.get("mcp.handshake_timeout_ms") {
            None => DEFAULT_HANDSHAKE_MS,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::Type { key: "mcp.handshake_timeout_ms".into(), expected: "a positive integer" })?,
        };
        let hosts = HostList::from_loadout(l)?;
        let servers = l
            .table("mcp.servers")
            .into_iter()
            .filter_map(|(name, row)| server_spec(l, &hosts, &name, &row).map(|spec| ServerEntry { name, spec }))
            .collect();
        Ok(McpHttpConfig { servers, handshake_timeout_ms, hosts })
    }
}

/// The directory and status another MCP transport provided, if one built first.
pub struct McpHttpDeps {
    prior: Option<McpHandle>,
    prior_status: Option<Arc<Vec<ServerStatus>>>,
}

impl Deps for McpHttpDeps {
    /// `mcp`: the directory `mcp-stdio` provided, which this capability extends; the plan orders
    /// it after stdio. Resolving it is optional (without stdio there is nothing to extend).
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Custom("mcp")];

    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(McpHttpDeps { prior: r.handle::<McpHandle>().ok(), prior_status: r.service::<Arc<Vec<ServerStatus>>>(STATUS_SERVICE) })
    }
}

struct Registered {
    spec: ToolSpec,
    server: HttpServer,
}

pub struct McpHttp {
    tools: Vec<Registered>,
    directory: Arc<McpDirectory>,
    status: Arc<Vec<ServerStatus>>,
    servers: Arc<Vec<HttpServer>>,
}

fn failed(name: &str, class: Option<Consequence>, error: String, fix: Option<String>) -> ServerStatus {
    ServerStatus { name: name.to_string(), class, state: ServerState::Failed { error }, tools: vec![], fix }
}

/// One reqwest client for every server: connect timeout, keep-alive, and a redirect policy that
/// checks each hop against the egress list.
#[allow(clippy::disallowed_methods, reason = "authority: mcp-http-client")]
fn http_client(hosts: &HostList, connect: Duration) -> Result<reqwest::Client, String> {
    let hosts = hosts.clone();
    reqwest::Client::builder()
        .connect_timeout(connect)
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent(concat!("aho/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(format!("the MCP server redirected more than {MAX_REDIRECTS} times; aho stopped following"));
            }
            let host = attempt.url().host_str().unwrap_or("").to_string();
            match hosts.check_host(&host) {
                Ok(()) => attempt.follow(),
                Err(why) => attempt.error(format!("the MCP server redirected to a host the egress list refuses: {why}")),
            }
        }))
        .build()
        .map_err(|e| format!("could not build the MCP-over-HTTP client ({e}); no HTTP server was contacted"))
}

type Probe = Result<(HttpServer, Result<Vec<RemoteTool>, McpError>), ConnectError>;

/// Connects and lists every server concurrently; results come back in `specs` order.
async fn probe_all(
    specs: Vec<HttpServerSpec>,
    http: reqwest::Client,
    rt: Arc<IoRuntime>,
    table: Arc<ToolTable>,
    handshake: Duration,
) -> Vec<(HttpServerSpec, Probe)> {
    let tasks: Vec<_> = specs
        .into_iter()
        .map(|spec| {
            let (http, rt, table, spec2) = (http.clone(), rt.clone(), table.clone(), spec.clone());
            let task = tokio::spawn(async move {
                let server = HttpServer::connect(&spec2, http, rt, table, handshake).await?;
                let listed = server.list_tools_now().await;
                Ok((server, listed))
            });
            (spec, task)
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for (spec, task) in tasks {
        let probe = task.await.unwrap_or_else(|e| {
            Err(ConnectError {
                error: McpError::Transport(format!("probing MCP server `{}` stopped unexpectedly ({e})", spec.name)),
                fix: format!("aho explain mcp.servers.{}", spec.name),
            })
        });
        out.push((spec, probe));
    }
    out
}

impl McpHttp {
    fn start(deps: McpHttpDeps, cfg: McpHttpConfig) -> Result<McpHttp, BuildError> {
        let prior_status: Vec<ServerStatus> = deps.prior_status.map(|s| s.as_ref().clone()).unwrap_or_default();
        let taken: BTreeMap<String, String> = prior_status
            .iter()
            .filter(|s| s.state == ServerState::Ready)
            .flat_map(|s| s.tools.iter().map(move |t| (t.clone(), format!("server `{}`", s.name))))
            .collect();
        let table = Arc::new(ToolTable::new(taken));
        let mine: BTreeSet<String> = cfg.servers.iter().map(|e| e.name.clone()).collect();
        let (mut status, mut specs) = (Vec::new(), Vec::new());
        for entry in cfg.servers {
            match entry.spec {
                Ok(spec) => specs.push(spec),
                Err((error, fix)) => status.push(failed(&entry.name, None, error, Some(fix))),
            }
        }
        let mut running: Vec<Arc<dyn McpServer>> = deps.prior.map(|h| h.0.servers().to_vec()).unwrap_or_default();
        let (mut tools, mut live) = (Vec::new(), Vec::new());
        if !specs.is_empty() {
            let handshake = Duration::from_millis(cfg.handshake_timeout_ms);
            let rt = IoRuntime::start().map_err(BuildError::Probe)?;
            let http = http_client(&cfg.hosts, handshake).map_err(BuildError::Probe)?;
            let probes =
                rt.block(probe_all(specs, http, rt.clone(), table.clone(), handshake)).map_err(|e| BuildError::Probe(e.to_string()))?;
            for (spec, probe) in probes {
                let name = spec.name.clone();
                let (server, listed) = match probe {
                    Ok(p) => p,
                    Err(ConnectError { error, fix }) => {
                        status.push(failed(&name, spec.class, error.to_string(), Some(fix)));
                        continue;
                    }
                };
                let Some(class) = spec.class else {
                    // Quarantine: report what it would expose; dropping the server deletes its session.
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
                match table.install(&name, listed) {
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
                    }
                    Err(e @ McpError::NameConflict { .. }) => status.push(ServerStatus {
                        name: name.clone(),
                        class: Some(class),
                        state: ServerState::Conflict { error: e.to_string() },
                        tools: vec![],
                        fix: Some(format!("rename the server: move `mcp.servers.{name}` to a different name")),
                    }),
                    Err(e) => {
                        status.push(failed(
                            &name,
                            Some(class),
                            format!("listing its tools failed: {e}"),
                            Some(format!("aho explain mcp.servers.{name}")),
                        ));
                        continue;
                    }
                }
                server.go_live();
                running.push(Arc::new(server.clone()));
                live.push(server);
            }
        }
        // Rows `mcp-stdio` wrote for HTTP servers ("stdio only") are replaced by this capability's.
        status.extend(prior_status.into_iter().filter(|s| !mine.contains(&s.name)));
        status.sort_by(|a, b| a.name.cmp(&b.name));
        running.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(McpHttp { tools, directory: Arc::new(McpDirectory::new(running)), status: Arc::new(status), servers: Arc::new(live) })
    }
}

impl Capability for McpHttp {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = McpHttpDeps;
    type Config = McpHttpConfig;

    fn build(deps: McpHttpDeps, cfg: McpHttpConfig) -> Result<Self, BuildError> {
        McpHttp::start(deps, cfg)
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for t in &self.tools {
            let (server, normalized) = (t.server.clone(), t.spec.name.clone());
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let (server, normalized) = (server.clone(), normalized.clone());
                Box::pin(async move {
                    let raw = match server.raw_name(&normalized) {
                        Ok(raw) => raw,
                        Err(reason) => return ExecOutcome::err("MCP_TOOL_GONE", reason),
                    };
                    match server.call_with_progress(&raw, req.view.args.clone(), req.cancel.clone(), req.progress.clone()).await {
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
        r.service(SERVERS_SERVICE, self.servers.clone());
    }
}

declare_capability!(
    McpHttp,
    "mcp-http",
    kind = Kind::Provider,
    provides = [aho_mcp::SEAM],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::Custom("mcp")],
    doc = "MCP servers over Streamable HTTP: `mcp.servers.<name>.url`, bearer/headers from `cred:<name>`; tools `mcp__<server>__<tool>` under the server's class; unclassified servers quarantined; extends mcp-stdio's directory."
);
