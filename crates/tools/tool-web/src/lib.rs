//! `00` (Knowledge: `web_search`, `web_fetch`), `02` (both are `NetworkEgress`, treated like any
//! MCP tool of that class), `07` (the egress allow/deny list is enforced by aho on these tools;
//! under `world = host` that is policy only — `bash` can `curl` anything).
//!
//! Two capabilities, so each locks honestly on its own:
//! * [`WebFetch`] (`tool-web-fetch`, tool `web_fetch`): URL → readable text. HTML becomes markdown
//!   ([`html`]), other text types are decoded by charset, raster images come back as image blocks,
//!   other binaries are described, never dumped. `max_inline` spills long pages (10).
//! * [`WebSearch`] (`tool-web-search`, tool `web_search`): query → results through a real API
//!   ([`search`]: Brave Search with `Credential(brave)`, or a SearXNG instance at
//!   `web.search.searxng_url`). With neither configured it is `Locked` with the fix, while
//!   `web_fetch` stays active.
//!
//! Both hold only `Egress(Route::Sandbox)` — the bound world's network (under `host`, world-host
//! provides it with `enforced by: policy only`).
//!
//! The egress list (`egress.allow` / `egress.deny`, see `aho_egress::HostList`) is checked before
//! any request: `ToolDecl::check` is a plain `fn` with no configuration, so it only refuses URLs
//! aho will not fetch at all; the operator's list is enforced by a deny-only guard registered with
//! the tool (decision-chain step 4, same call, before dispatch), again in the tool body before
//! calling egress, and by the egress implementation on every redirect hop.

#![forbid(unsafe_code)]

pub mod content;
pub mod html;
pub mod search;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_egress::{EgressError, EgressHandle, HostList, HttpRequest, HttpResponse, Limits};
use aho_log::OpKind;
use aho_types::*;
use serde_json::json;

use crate::content::Kind as BodyKind;
use crate::search::{BRAVE_URL, Backend, Hit};

pub const FETCH: &str = "web_fetch";
pub const SEARCH: &str = "web_search";
/// Characters of converted text kept inline before the session spills the rest (10).
pub const MAX_INLINE: usize = 50_000;

// ───────────────────────────── declarations ─────────────────────────────

fn fetch_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "http(s) URL to fetch." },
            "raw": { "type": "boolean", "description": "Return the decoded body as-is instead of converting HTML to markdown." },
            "max_bytes": { "type": "integer", "minimum": 1, "description": "Read at most this many bytes (never more than the configured cap)." }
        },
        "required": ["url"]
    }))
}

fn search_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "What to search the web for." },
            "count": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Results wanted (default from config)." }
        },
        "required": ["query"]
    }))
}

/// The tool's own opinion (04 step 3): refuses what aho never fetches. The operator's host list
/// needs configuration and is enforced by the guard.
fn check_fetch(args: &Value, _: &PolicyView) -> Option<Decision> {
    match args.get("url").and_then(Value::as_str) {
        None => Some(Decision::Deny("web_fetch needs a `url` string; nothing was fetched.".into())),
        Some(url) => aho_egress::parse_url(url)
            .err()
            .map(|why| Decision::Deny(format!("`{url}` is not a URL aho will fetch: {why}. Nothing was sent."))),
    }
}

fn check_search(args: &Value, _: &PolicyView) -> Option<Decision> {
    match args.get("query").and_then(Value::as_str).map(str::trim) {
        Some(q) if !q.is_empty() => None,
        _ => Some(Decision::Deny("web_search needs a non-empty `query`; nothing was searched.".into())),
    }
}

fn fetch_call_card(args: &Value) -> Card {
    Card::Web { url: args.get("url").and_then(Value::as_str).unwrap_or("").to_string(), title: "fetching".into() }
}

fn fetch_result_card(args: &Value, value: &Value) -> Card {
    let url = value.get("final_url").or_else(|| args.get("url")).and_then(Value::as_str).unwrap_or("").to_string();
    let title = value.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    Card::Web { url, title }
}

fn search_call_card(args: &Value) -> Card {
    Card::Search { query: args.get("query").and_then(Value::as_str).unwrap_or("").to_string(), hits: Vec::new() }
}

fn search_result_card(args: &Value, value: &Value) -> Card {
    let hits = value
        .get("results")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().filter_map(|r| r.get("url").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    Card::Search { query: args.get("query").and_then(Value::as_str).unwrap_or("").to_string(), hits }
}

pub static FETCH_TOOLS: &[ToolDecl] = &[ToolDecl::new(
    FETCH,
    "Fetch a web page or file over http(s) and return it as readable text (HTML is converted to markdown). Long pages are spilled; hosts may be restricted by the operator's egress list.",
    Consequence::NetworkEgress,
)
.schema(fetch_schema)
.safe()
.cancel_on_interrupt()
.max_inline(Some(MAX_INLINE))
.check(check_fetch)
.present(Present { call: fetch_call_card, result: fetch_result_card })
.op(OpKind::Remote, true, false)];

pub static SEARCH_TOOLS: &[ToolDecl] = &[ToolDecl::new(
    SEARCH,
    "Search the web and return titles, URLs and snippets. Use web_fetch to read a result.",
    Consequence::NetworkEgress,
)
.schema(search_schema)
.safe()
.cancel_on_interrupt()
.max_inline(Some(MAX_INLINE))
.check(check_search)
.present(Present { call: search_call_card, result: search_result_card })
.op(OpKind::Remote, true, false)];

// ───────────────────────────── deps & config ─────────────────────────────

pub struct WebDeps {
    pub egress: EgressHandle,
}

impl Deps for WebDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Egress(Route::Sandbox)];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(WebDeps { egress: r.handle()? })
    }
}

fn int(l: &Loadout, key: &str, default: u64) -> Result<u64, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().filter(|n| *n > 0).ok_or_else(|| ConfigError::Type { key: key.to_string(), expected: "a positive integer" }),
    }
}

#[derive(Clone, Debug)]
pub struct FetchConfig {
    pub hosts: HostList,
    pub limits: Limits,
}

impl Config for FetchConfig {
    const PREFIX: &'static str = "web.fetch";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = aho_egress::config_keys();
        keys.extend([
            ConfigKey::new(
                "web.fetch.max_bytes",
                Ty::Int,
                Some(json!(aho_egress::DEFAULT_MAX_BODY_BYTES)),
                "Bytes of a response web_fetch reads; the rest is not downloaded and the result says it was cut.",
            ),
            ConfigKey::new(
                "web.fetch.timeout_ms",
                Ty::Int,
                Some(json!(aho_egress::DEFAULT_TIMEOUT_MS)),
                "Wall-clock limit for one web_fetch, redirects included.",
            ),
            ConfigKey::new(
                "web.fetch.max_redirects",
                Ty::Int,
                Some(json!(aho_egress::DEFAULT_MAX_REDIRECTS)),
                "Redirects web_fetch follows; each hop is checked against egress.allow/egress.deny.",
            ),
        ]);
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let limits = Limits {
            max_body_bytes: int(l, "web.fetch.max_bytes", aho_egress::DEFAULT_MAX_BODY_BYTES as u64)? as usize,
            timeout_ms: int(l, "web.fetch.timeout_ms", aho_egress::DEFAULT_TIMEOUT_MS)?,
            max_redirects: match l.get("web.fetch.max_redirects") {
                None | Some(Value::Null) => aho_egress::DEFAULT_MAX_REDIRECTS,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| ConfigError::Type { key: "web.fetch.max_redirects".into(), expected: "a non-negative integer" })?,
            },
        };
        Ok(FetchConfig { hosts: HostList::from_loadout(l)?, limits })
    }
}

/// `web.search.brave.key` (secret) or the named credential `credentials.brave`; `cred:<n>` indirects.
fn brave_key(l: &Loadout) -> Result<Option<String>, ConfigError> {
    fn read(l: &Loadout, key: &str, depth: u8) -> Result<Option<String>, ConfigError> {
        let raw = match l.secrets.get(key) {
            Some(s) => s.expose().to_string(),
            None => match l.get(key) {
                None | Some(Value::Null) => return Ok(None),
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(ConfigError::Type { key: key.to_string(), expected: "a string" }),
            },
        };
        match raw.strip_prefix("cred:") {
            Some(name) if depth == 0 => {
                read(l, &format!("credentials.{name}"), 1)?.map(Some).ok_or(ConfigError::MissingKey(format!("credentials.{name}")))
            }
            Some(_) => Err(ConfigError::Type { key: key.to_string(), expected: "a key, not another `cred:` reference" }),
            None if raw.is_empty() => Ok(None),
            None => Ok(Some(raw)),
        }
    }
    match read(l, "web.search.brave.key", 0)? {
        Some(k) => Ok(Some(k)),
        None => read(l, "credentials.brave", 1),
    }
}

#[derive(Clone, Debug)]
pub struct SearchConfig {
    pub hosts: HostList,
    pub limits: Limits,
    /// `None` when neither backend is configured (the plan then shows the capability Locked).
    pub backend: Option<Backend>,
    pub count: u32,
}

impl Config for SearchConfig {
    const PREFIX: &'static str = "web.search";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = aho_egress::config_keys();
        keys.extend([
            ConfigKey::new(
                "web.search.provider",
                Ty::String,
                Some(json!("auto")),
                "`auto` (Brave if a key is set, else SearXNG if web.search.searxng_url is set), `brave`, or `searxng`.",
            ),
            ConfigKey::new(
                "web.search.searxng_url",
                Ty::String,
                None,
                "Base URL of a SearXNG instance with the `json` output format enabled (e.g. `https://searx.example.org`).",
            ),
            ConfigKey::new(
                "web.search.brave.key",
                Ty::String,
                None,
                "Brave Search API key, or `cred:brave`; `aho credential set brave` stores it as credentials.brave, which is read too.",
            )
            .secret(),
            ConfigKey::new("web.search.brave_url", Ty::String, Some(json!(BRAVE_URL)), "Brave Search web endpoint."),
            ConfigKey::new("web.search.count", Ty::Int, Some(json!(10)), "Results web_search returns when the call does not say (1–20)."),
            ConfigKey::new("web.search.timeout_ms", Ty::Int, Some(json!(15_000)), "Wall-clock limit for one web_search."),
            ConfigKey::new("web.search.max_bytes", Ty::Int, Some(json!(2 * 1024 * 1024)), "Largest search API response read."),
        ]);
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let text = |key: &str| -> Result<Option<String>, ConfigError> {
            match l.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
                Some(_) => Err(ConfigError::Type { key: key.to_string(), expected: "a string" }),
            }
        };
        let brave_url = text("web.search.brave_url")?.unwrap_or_else(|| BRAVE_URL.to_string());
        let key = brave_key(l)?;
        let searxng = text("web.search.searxng_url")?;
        let backend = match text("web.search.provider")?.as_deref().unwrap_or("auto") {
            "auto" => match (key, searxng) {
                (Some(key), _) => Some(Backend::Brave { url: brave_url, key }),
                (None, Some(url)) => Some(Backend::Searxng { url }),
                (None, None) => None,
            },
            "brave" => Some(Backend::Brave { url: brave_url, key: key.ok_or(ConfigError::MissingKey("credentials.brave".into()))? }),
            "searxng" => Some(Backend::Searxng { url: searxng.ok_or(ConfigError::MissingKey("web.search.searxng_url".into()))? }),
            _ => return Err(ConfigError::Type { key: "web.search.provider".into(), expected: "`auto`, `brave` or `searxng`" }),
        };
        let count = int(l, "web.search.count", 10)?.min(search::BRAVE_MAX_COUNT as u64) as u32;
        let limits = Limits {
            max_body_bytes: int(l, "web.search.max_bytes", 2 * 1024 * 1024)? as usize,
            timeout_ms: int(l, "web.search.timeout_ms", 15_000)?,
            max_redirects: 2,
        };
        Ok(SearchConfig { hosts: HostList::from_loadout(l)?, limits, backend, count })
    }
}

// ───────────────────────────── shared ─────────────────────────────

/// Races `f` against the call's cancellation token.
async fn cancellable<F: Future>(cancel: &CancelToken, f: F) -> Option<F::Output> {
    let mut f = std::pin::pin!(f);
    loop {
        tokio::select! {
            out = &mut f => return Some(out),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if cancel.is_cancelled() {
                    return None;
                }
            }
        }
    }
}

fn egress_outcome(e: &EgressError) -> ExecOutcome {
    let code = match e {
        EgressError::Denied(_) => "EGRESS_DENIED",
        EgressError::InvalidUrl { .. } => "BAD_URL",
        EgressError::TooManyRedirects { .. } => "TOO_MANY_REDIRECTS",
        EgressError::Timeout { .. } => "TIMEOUT",
        EgressError::Connect { .. } => "UNREACHABLE",
        EgressError::Unsupported(_) => "UNSUPPORTED",
    };
    ExecOutcome::err(code, e.to_string())
}

fn tool_exec<T: Clone + Send + Sync + 'static>(me: T, run: fn(T, ExecRequest) -> BoxFuture<'static, ExecOutcome>) -> ToolExec {
    Arc::new(move |req: ExecRequest| run(me.clone(), req))
}

// ───────────────────────────── web_fetch ─────────────────────────────

#[derive(Clone)]
pub struct WebFetch {
    egress: EgressHandle,
    cfg: Arc<FetchConfig>,
}

impl WebFetch {
    pub fn new(egress: EgressHandle, cfg: FetchConfig) -> WebFetch {
        WebFetch { egress, cfg: Arc::new(cfg) }
    }

    /// The operator's list, applied in the decision chain before dispatch. `Some` = refusal.
    pub fn guard(&self, view: &ExecView) -> Option<String> {
        if view.tool != FETCH {
            return None;
        }
        let url = view.args.get("url").and_then(Value::as_str)?;
        self.cfg.hosts.check_url(url).err()
    }

    pub async fn fetch(&self, args: &Value, cancel: &CancelToken) -> ExecOutcome {
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return ExecOutcome::err("BAD_ARGS", "web_fetch needs a `url` string; nothing was fetched.");
        };
        if let Err(why) = self.cfg.hosts.check_url(url) {
            return ExecOutcome::err("EGRESS_DENIED", why);
        }
        let mut limits = self.cfg.limits;
        if let Some(n) = args.get("max_bytes").and_then(Value::as_u64) {
            limits.max_body_bytes = limits.max_body_bytes.min(usize::try_from(n).unwrap_or(usize::MAX).max(1));
        }
        let req = HttpRequest::get(url)
            .header("accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5")
            .limits(limits)
            .hosts(self.cfg.hosts.clone());
        let resp = match cancellable(cancel, self.egress.0.fetch(req)).await {
            None => return ExecOutcome::Cancelled,
            Some(Err(e)) => return egress_outcome(&e),
            Some(Ok(r)) => r,
        };
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        render_page(&resp, raw, limits.max_body_bytes)
    }
}

/// Turns a response into model content: header lines, then the converted body.
pub fn render_page(resp: &HttpResponse, raw: bool, cap: usize) -> ExecOutcome {
    let media = resp.media_type();
    let kind = content::classify(media.as_deref(), &resp.body);
    let shown_type = media.clone().unwrap_or_else(|| "unknown".into());
    if !resp.is_success() {
        let (text, _) = content::decode(&resp.body, resp.charset().as_deref());
        let excerpt: String = match kind {
            BodyKind::Html => html::to_markdown(&text, url::Url::parse(&resp.final_url).ok().as_ref()).1,
            BodyKind::Text => text,
            _ => String::new(),
        }
        .chars()
        .take(500)
        .collect();
        return ExecOutcome::err(
            &format!("HTTP_{}", resp.status),
            format!("{} answered HTTP {}; nothing else was fetched. Response excerpt: {}", resp.final_url, resp.status, excerpt.trim()),
        );
    }
    let mut title = None;
    let mut notes: Vec<String> = Vec::new();
    if resp.truncated {
        notes.push(format!("only the first {cap} bytes were read (web.fetch.max_bytes)"));
    }
    let body: Vec<ContentBlock> = match &kind {
        BodyKind::Html | BodyKind::Text => {
            let charset = resp.charset().or_else(|| if kind == BodyKind::Html { content::meta_charset(&resp.body) } else { None });
            let (text, note) = content::decode(&resp.body, charset.as_deref());
            notes.extend(note);
            let text = if kind == BodyKind::Html && !raw {
                let (t, md) = html::to_markdown(&text, url::Url::parse(&resp.final_url).ok().as_ref());
                title = t;
                md
            } else {
                text
            };
            vec![ContentBlock::text(text)]
        }
        BodyKind::Image(m) if !resp.truncated => vec![ContentBlock::Image { media_type: m.clone(), data: resp.body.clone() }],
        BodyKind::Image(m) | BodyKind::Binary(m) => {
            vec![ContentBlock::text(format!("[binary content: {m}, {} bytes read; not shown as text]", resp.body.len()))]
        }
    };
    let mut head = format!("URL: {}\nContent-Type: {shown_type}\n", resp.final_url);
    if let Some(t) = &title {
        head.push_str(&format!("Title: {t}\n"));
    }
    if resp.redirects > 0 {
        head.push_str(&format!("Redirects: {}\n", resp.redirects));
    }
    for n in &notes {
        head.push_str(&format!("Note: {n}\n"));
    }
    let mut content = vec![ContentBlock::text(head)];
    content.extend(body);
    let kind_name = match kind {
        BodyKind::Html => "html",
        BodyKind::Text => "text",
        BodyKind::Image(_) => "image",
        BodyKind::Binary(_) => "binary",
    };
    let value = json!({
        "url": resp.final_url,
        "final_url": resp.final_url,
        "status": resp.status,
        "content_type": media,
        "kind": kind_name,
        "title": title,
        "bytes": resp.body.len(),
        "truncated": resp.truncated,
        "redirects": resp.redirects,
    });
    ExecOutcome::Done { value, content, meta: None }
}

impl Capability for WebFetch {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = WebDeps;
    type Config = FetchConfig;

    fn build(d: WebDeps, cfg: FetchConfig) -> Result<Self, BuildError> {
        Ok(WebFetch::new(d.egress, cfg))
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let me = self.clone();
        r.guard(Arc::new(move |v: &ExecView| me.guard(v)));
        r.tool(&FETCH_TOOLS[0], tool_exec(self.clone(), |me, req| Box::pin(async move { me.fetch(&req.view.args, &req.cancel).await })));
    }
}

declare_capability!(
    WebFetch,
    "tool-web-fetch",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_egress::SEAM)],
    tools = FETCH_TOOLS,
    doc = "web_fetch: a URL as readable text through the bound world's network, within the operator's egress.allow/egress.deny list."
);

// ───────────────────────────── web_search ─────────────────────────────

#[derive(Clone)]
pub struct WebSearch {
    egress: EgressHandle,
    backend: Backend,
    cfg: Arc<SearchConfig>,
}

impl WebSearch {
    pub fn new(egress: EgressHandle, backend: Backend, cfg: SearchConfig) -> WebSearch {
        WebSearch { egress, backend, cfg: Arc::new(cfg) }
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The search endpoint's host must pass the operator's list too.
    pub fn guard(&self, view: &ExecView) -> Option<String> {
        if view.tool != SEARCH {
            return None;
        }
        match self.backend.endpoint("probe", 1) {
            Ok(url) => self.cfg.hosts.check_url(&url).err(),
            Err(e) => Some(e),
        }
    }

    pub async fn search(&self, args: &Value, cancel: &CancelToken) -> ExecOutcome {
        let query = args.get("query").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if query.is_empty() {
            return ExecOutcome::err("BAD_ARGS", "web_search needs a non-empty `query`; nothing was searched.");
        }
        let count =
            args.get("count").and_then(Value::as_u64).map(|n| n.clamp(1, search::BRAVE_MAX_COUNT as u64) as u32).unwrap_or(self.cfg.count);
        let req = match self.backend.request(query, count, self.cfg.limits, self.cfg.hosts.clone()) {
            Ok(r) => r,
            Err(e) => return ExecOutcome::err("BAD_CONFIG", e),
        };
        if let Err(why) = self.cfg.hosts.check_url(&req.url) {
            return ExecOutcome::err("EGRESS_DENIED", why);
        }
        let resp = match cancellable(cancel, self.egress.0.fetch(req)).await {
            None => return ExecOutcome::Cancelled,
            Some(Err(e)) => return egress_outcome(&e),
            Some(Ok(r)) => r,
        };
        match self.backend.parse(&resp, count) {
            Ok(hits) => {
                let text = search::render(query, self.backend.name(), &hits);
                let results: Vec<Value> =
                    hits.iter().map(|Hit { title, url, snippet }| json!({"title": title, "url": url, "snippet": snippet})).collect();
                let value = json!({ "query": query, "backend": self.backend.name(), "results": results });
                ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
            }
            Err(e) => ExecOutcome::err("SEARCH_FAILED", e),
        }
    }
}

impl Capability for WebSearch {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = WebDeps;
    type Config = SearchConfig;

    fn build(d: WebDeps, cfg: SearchConfig) -> Result<Self, BuildError> {
        let backend = cfg.backend.clone().ok_or(BuildError::Unsupported(
            "no web search backend is configured: run `aho credential set brave` (Brave Search API key) or `aho set web.search.searxng_url <instance URL>`",
        ))?;
        Ok(WebSearch::new(d.egress, backend, cfg))
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let me = self.clone();
        r.guard(Arc::new(move |v: &ExecView| me.guard(v)));
        r.tool(&SEARCH_TOOLS[0], tool_exec(self.clone(), |me, req| Box::pin(async move { me.search(&req.view.args, &req.cancel).await })));
    }
}

declare_capability!(
    WebSearch,
    "tool-web-search",
    kind = Kind::Tool,
    provides = [],
    requires = [
        Requirement::Seam(aho_egress::SEAM),
        Requirement::AnyOf(&[Requirement::Credential("brave"), Requirement::Setting { key: "web.search.searxng_url", pred: Pred::Set }]),
    ],
    tools = SEARCH_TOOLS,
    unlock = Fix::cli("aho credential set brave"),
    doc = "web_search: query → results via the Brave Search API (credential `brave`) or a SearXNG instance (web.search.searxng_url)."
);
