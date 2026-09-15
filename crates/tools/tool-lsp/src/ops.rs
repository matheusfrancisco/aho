//! The tool bodies: resolve a position (path + line + column, or a symbol name), ask the server,
//! render `path:line:col  snippet` rows, capped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::{CancelToken, ExecOutcome, ExecRequest};
use aho_fs::{DIFF_CONTEXT, unified_hunks};
use aho_types::ContentBlock;
use serde_json::{Value, json};

use crate::client::LspError;
use crate::config::{DIAGNOSTICS_KEY, READY_KEY, ServerConfig};
use crate::pool::{Pool, Server, Synced, language_id};
use crate::text::{self, Encoding, from_lsp_col, to_lsp_col, word_starts};

/// A snippet longer than this is clipped.
const SNIPPET: usize = 160;
/// Hover text beyond this is clipped.
const HOVER_MAX: usize = 8_000;
/// A diagnostics publish counts as final once no newer one arrived for this long.
const DIAGNOSTICS_SETTLE: Duration = Duration::from_millis(1_000);
/// Pause before re-asking a server that cancelled a diagnostics pull.
const PULL_RETRY: Duration = Duration::from_millis(200);

pub fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn lsp_err(e: LspError) -> ExecOutcome {
    match e {
        LspError::Cancelled => ExecOutcome::Cancelled,
        LspError::Timeout { .. } => ExecOutcome::err("LSP_TIMEOUT", e.to_string()),
        LspError::Server { .. } => ExecOutcome::err("LSP_SERVER_ERROR", e.to_string()),
        LspError::Exited(_) => ExecOutcome::err("LSP_SERVER_EXITED", e.to_string()),
        LspError::Io(_) => ExecOutcome::err("LSP_IO", e.to_string()),
    }
}

fn done(value: Value, text: String) -> ExecOutcome {
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn opt_str<'a>(args: &'a Value, k: &str) -> Result<Option<&'a str>, ExecOutcome> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(bad_args(format!("`{k}` must be a non-empty string."))),
    }
}

fn opt_pos(args: &Value, k: &str) -> Result<Option<usize>, ExecOutcome> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(n as usize)),
            _ => Err(bad_args(format!("`{k}` is 1-based: give an integer ≥ 1."))),
        },
    }
}

/// `a::b::name` / `a.b.name` → (`["a","b"]`, `"name"`).
fn split_symbol(s: &str) -> (Vec<&str>, &str) {
    let parts: Vec<&str> = s.split("::").flat_map(|p| p.split('.')).filter(|p| !p.is_empty()).collect();
    match parts.split_last() {
        Some((last, rest)) => (rest.to_vec(), last),
        None => (Vec::new(), s),
    }
}

/// A resolved request position.
struct Target {
    server: Arc<Server>,
    rel: String,
    uri: String,
    text: String,
    line: u32,
    units: u32,
    notes: Vec<String>,
}

impl Target {
    fn params(&self) -> Value {
        json!({"textDocument": {"uri": self.uri}, "position": {"line": self.line, "character": self.units}})
    }
    fn here(&self) -> String {
        let l = text::line(&self.text, self.line as usize).unwrap_or("");
        format!("{}:{}:{}", self.rel, self.line + 1, from_lsp_col(l, self.units, self.server.encoding) + 1)
    }
}

/// One rendered location.
struct Loc {
    path: String,
    line: usize,
    column: usize,
    snippet: String,
}

impl Loc {
    fn row(&self) -> String {
        if self.snippet.is_empty() {
            format!("{}:{}:{}", self.path, self.line, self.column)
        } else {
            format!("{}:{}:{}  {}", self.path, self.line, self.column, self.snippet)
        }
    }
    fn json(&self) -> Value {
        json!({"path": self.path, "line": self.line, "column": self.column, "snippet": self.snippet})
    }
}

/// Reads documents for snippets once per call.
struct Files<'a> {
    pool: &'a Pool,
    cache: HashMap<String, Option<String>>,
}

impl<'a> Files<'a> {
    fn new(pool: &'a Pool, known: Option<(&str, &str)>) -> Files<'a> {
        let mut cache = HashMap::new();
        if let Some((rel, text)) = known {
            cache.insert(rel.to_string(), Some(text.to_string()));
        }
        Files { pool, cache }
    }

    async fn text(&mut self, rel: &str) -> Option<String> {
        if let Some(t) = self.cache.get(rel) {
            return t.clone();
        }
        let t = self.pool.fs.read(rel, None).await.ok().map(|r| r.text);
        self.cache.insert(rel.to_string(), t.clone());
        t
    }

    /// `uri` + LSP position → workspace path, 1-based line and char column, the line's text.
    async fn loc(&mut self, uri: &str, pos: &Value, enc: Encoding) -> Loc {
        let line0 = pos.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let units = pos.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
        let (path, inside) = self.pool.display_uri(uri);
        let line_text = if inside { self.text(&path).await.and_then(|t| text::line(&t, line0).map(str::to_string)) } else { None };
        match line_text {
            Some(l) => Loc { column: from_lsp_col(&l, units, enc) + 1, snippet: clip(l.trim(), SNIPPET), path, line: line0 + 1 },
            None => Loc { column: units as usize + 1, snippet: String::new(), path, line: line0 + 1 },
        }
    }
}

/// `Location | Location[] | LocationLink[] | null` → (uri, start position) pairs.
fn locations(v: &Value) -> Vec<(String, Value)> {
    let one = |l: &Value| -> Option<(String, Value)> {
        if let Some(uri) = l.get("targetUri").and_then(Value::as_str) {
            let r = l.get("targetSelectionRange").or_else(|| l.get("targetRange"))?;
            return Some((uri.to_string(), r.get("start")?.clone()));
        }
        Some((l.get("uri")?.as_str()?.to_string(), l.pointer("/range/start")?.clone()))
    };
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

pub fn symbol_kind(k: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum member",
        "struct",
        "event",
        "operator",
        "type parameter",
    ];
    k.checked_sub(1).and_then(|i| KINDS.get(i as usize)).copied().unwrap_or("symbol")
}

fn severity(s: Option<u64>) -> &'static str {
    match s {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "diagnostic",
    }
}

/// Hover `contents` (MarkupContent | MarkedString | MarkedString[]) → text.
fn hover_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(hover_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n"),
        Value::Object(o) => match (o.get("language").and_then(Value::as_str), o.get("value").and_then(Value::as_str)) {
            (Some(lang), Some(v)) => format!("```{lang}\n{v}\n```"),
            (None, Some(v)) => v.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// One outline row: name, containers, kind, start position, and its document when the server
/// said (`SymbolInformation`; hierarchical outlines are in the requested document).
type Sym = (String, Vec<String>, u64, Value, Option<String>);

/// Flattened document outline or workspace symbol list.
fn outline(v: &Value, parents: &[String], out: &mut Vec<Sym>) {
    let Some(items) = v.as_array() else { return };
    for s in items {
        let Some(name) = s.get("name").and_then(Value::as_str) else { continue };
        let kind = s.get("kind").and_then(Value::as_u64).unwrap_or(0);
        if let Some(start) = s.pointer("/selectionRange/start") {
            out.push((name.to_string(), parents.to_vec(), kind, start.clone(), None));
            let mut next = parents.to_vec();
            next.push(name.to_string());
            if let Some(children) = s.get("children") {
                outline(children, &next, out);
            }
        } else if let (Some(uri), Some(start)) = (s.pointer("/location/uri").and_then(Value::as_str), s.pointer("/location/range/start")) {
            let container: Vec<String> =
                s.get("containerName").and_then(Value::as_str).filter(|c| !c.is_empty()).map(|c| vec![c.to_string()]).unwrap_or_default();
            out.push((name.to_string(), container, kind, start.clone(), Some(uri.to_string())));
        }
    }
}

fn qualifier_matches(want: &[&str], containers: &[String]) -> bool {
    if want.is_empty() {
        return true;
    }
    let chain: Vec<&str> = containers.iter().flat_map(|c| c.split("::")).flat_map(|c| c.split('.')).map(|c| c.trim()).collect();
    chain.len() >= want.len() && chain[chain.len() - want.len()..].iter().zip(want).all(|(a, b)| a == b || a.ends_with(&format!(" {b}")))
}

impl Pool {
    fn not_ready_note(&self) -> String {
        format!(
            "the language server was still indexing after {} ms; results may be incomplete. Retry, or raise {READY_KEY}.",
            self.cfg.ready_timeout_ms
        )
    }

    async fn ready(&self, s: &Server, cancel: &CancelToken, notes: &mut Vec<String>) -> Result<(), LspError> {
        if !s.client.wait_ready(Duration::from_millis(self.cfg.ready_timeout_ms), cancel).await? {
            notes.push(self.not_ready_note());
        }
        Ok(())
    }

    async fn request(&self, s: &Server, method: &str, params: Value, cancel: &CancelToken) -> Result<Value, ExecOutcome> {
        s.client.request(method, params, self.request_timeout(), cancel).await.map_err(lsp_err)
    }

    /// Server + synced document for a workspace path the model gave.
    async fn open(&self, path: &str, cancel: &CancelToken) -> Result<(Arc<Server>, String, Synced), ExecOutcome> {
        let rel = self.rel(path).map_err(bad_args)?;
        let cfg = self.language_for(&rel).map_err(|e| ExecOutcome::err("LSP_NO_SERVER", e))?;
        let root = self.root_for(cfg, &rel).await;
        let server = self.server(cfg, &root, cancel).await.map_err(lsp_err)?;
        let synced =
            server.sync(self.fs.as_ref(), &rel, &language_id(&cfg.language, &rel)).await.map_err(|e| ExecOutcome::err("LSP_IO", e))?;
        Ok((server, rel, synced))
    }

    fn workspace_language(&self, args: &Value) -> Result<&ServerConfig, ExecOutcome> {
        let names = || self.cfg.servers.iter().map(|s| s.language.as_str()).collect::<Vec<_>>().join(", ");
        match opt_str(args, "language")? {
            Some(l) => self
                .cfg
                .language(l)
                .ok_or_else(|| bad_args(format!("no language server is configured for `{l}`; configured: {}.", names()))),
            None if self.cfg.servers.len() == 1 => Ok(&self.cfg.servers[0]),
            None => Err(bad_args(format!("several languages are configured ({}); give `language` or a `path`.", names()))),
        }
    }

    /// Running servers for the language, or one started at the workspace root.
    async fn workspace_servers(&self, cfg: &ServerConfig, cancel: &CancelToken) -> Result<Vec<Arc<Server>>, ExecOutcome> {
        let running = self.running(&cfg.language).await;
        if !running.is_empty() {
            return Ok(running);
        }
        let root = self.root_for(cfg, "").await;
        Ok(vec![self.server(cfg, &root, cancel).await.map_err(lsp_err)?])
    }

    /// `path` + `line` [+ `column` | `symbol`], `path` + `symbol`, or `symbol` alone.
    async fn target(&self, args: &Value, cancel: &CancelToken) -> Result<Target, ExecOutcome> {
        let path = opt_str(args, "path")?;
        let symbol = opt_str(args, "symbol")?;
        let line = opt_pos(args, "line")?;
        let column = opt_pos(args, "column")?;
        let Some(path) = path else {
            let Some(symbol) = symbol else {
                return Err(bad_args("give `path` with `line` (and `column`), or a `symbol` name."));
            };
            return self.workspace_target(args, symbol, cancel).await;
        };
        let (server, rel, synced) = self.open(path, cancel).await?;
        let mut notes = Vec::new();
        self.ready(&server, cancel, &mut notes).await.map_err(lsp_err)?;
        let enc = server.encoding;
        let (line0, ch0) = match (line, symbol) {
            (Some(l), _) => {
                let Some(lt) = text::line(&synced.text, l - 1) else {
                    let n = synced.text.split('\n').count();
                    return Err(bad_args(format!("`{rel}` has {n} lines; line {l} does not exist.")));
                };
                let ch = match (column, symbol) {
                    (Some(c), _) => c - 1,
                    (None, Some(s)) => {
                        let (_, name) = split_symbol(s);
                        *word_starts(lt, name).first().ok_or_else(|| bad_args(format!("`{name}` does not occur on {rel}:{l}.")))?
                    }
                    (None, None) => lt.chars().take_while(|c| c.is_whitespace()).count(),
                };
                (l - 1, ch)
            }
            (None, Some(s)) => self.find_in_document(&server, &rel, &synced, s, cancel, &mut notes).await?,
            (None, None) => return Err(bad_args("give `line` (and `column`) or a `symbol` together with `path`.")),
        };
        let lt = text::line(&synced.text, line0).unwrap_or("");
        Ok(Target { units: to_lsp_col(lt, ch0, enc), line: line0 as u32, server, rel, uri: synced.uri, text: synced.text, notes })
    }

    /// The named symbol in one document's outline; else its first whole-word occurrence.
    async fn find_in_document(
        &self,
        server: &Server,
        rel: &str,
        synced: &Synced,
        symbol: &str,
        cancel: &CancelToken,
        notes: &mut Vec<String>,
    ) -> Result<(usize, usize), ExecOutcome> {
        let (quals, name) = split_symbol(symbol);
        if server.supports("documentSymbolProvider") {
            let v = self.request(server, "textDocument/documentSymbol", json!({"textDocument": {"uri": synced.uri}}), cancel).await?;
            let mut flat = Vec::new();
            outline(&v, &[], &mut flat);
            let hits: Vec<_> = flat.iter().filter(|(n, c, ..)| n == name && qualifier_matches(&quals, c)).collect();
            if let Some((_, _, _, start, _)) = hits.first() {
                if hits.len() > 1 {
                    notes.push(format!("`{symbol}` names {} symbols in {rel}; used the first.", hits.len()));
                }
                let l0 = start.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
                let lt = text::line(&synced.text, l0).unwrap_or("");
                let units = start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
                return Ok((l0, from_lsp_col(lt, units, server.encoding)));
            }
        }
        for (i, l) in synced.text.split('\n').enumerate() {
            if let Some(c) = word_starts(l, name).first() {
                notes.push(format!("`{symbol}` is not in {rel}'s outline; used its first occurrence."));
                return Ok((i, *c));
            }
        }
        Err(bad_args(format!("`{name}` does not occur in {rel}. Check the name, or search with lsp_symbols.")))
    }

    async fn workspace_target(&self, args: &Value, symbol: &str, cancel: &CancelToken) -> Result<Target, ExecOutcome> {
        let cfg = self.workspace_language(args)?;
        let (quals, name) = split_symbol(symbol);
        let mut notes = Vec::new();
        let mut hits = Vec::new();
        for server in self.workspace_servers(cfg, cancel).await? {
            self.ready(&server, cancel, &mut notes).await.map_err(lsp_err)?;
            let v = self.request(&server, "workspace/symbol", json!({"query": name}), cancel).await?;
            let mut flat = Vec::new();
            outline(&v, &[], &mut flat);
            for (n, containers, _, start, uri) in flat {
                let Some(uri) = uri else { continue };
                if n == name && qualifier_matches(&quals, &containers) {
                    let (_, inside) = self.display_uri(&uri);
                    hits.push((!inside, uri, start, server.clone()));
                }
            }
        }
        hits.sort_by_key(|h| h.0);
        let Some((outside, uri, start, server)) = hits.first().cloned() else {
            return Err(bad_args(format!(
                "no symbol named `{symbol}` was found in the workspace. Check the spelling, or give `path` and `line`."
            )));
        };
        if outside {
            return Err(bad_args(format!("`{symbol}` is only defined outside the workspace ({uri}); give a `path` and `line` inside it.")));
        }
        if hits.len() > 1 {
            notes.push(format!("`{symbol}` matched {} symbols; used the first in the workspace.", hits.len()));
        }
        let (rel, _) = self.display_uri(&uri);
        let synced =
            server.sync(self.fs.as_ref(), &rel, &language_id(&cfg.language, &rel)).await.map_err(|e| ExecOutcome::err("LSP_IO", e))?;
        let l0 = start.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let lt = text::line(&synced.text, l0).unwrap_or("");
        // Workspace symbols point at the whole item (`pub fn …`); aim at the name itself.
        let from = from_lsp_col(lt, start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32, server.encoding);
        let (line0, ch0) = (l0..l0 + 20)
            .find_map(|i| {
                let l = text::line(&synced.text, i)?;
                let skip = if i == l0 { from } else { 0 };
                word_starts(l, name).into_iter().find(|c| *c >= skip).map(|c| (i, c))
            })
            .unwrap_or((l0, from));
        let lt = text::line(&synced.text, line0).unwrap_or("");
        Ok(Target {
            units: to_lsp_col(lt, ch0, server.encoding),
            line: line0 as u32,
            server,
            rel,
            uri: synced.uri,
            text: synced.text,
            notes,
        })
    }

    async fn render_locations(&self, t: &Target, found: Vec<(String, Value)>, heading: &str) -> (Value, String) {
        let mut files = Files::new(self, Some((&t.rel, &t.text)));
        let total = found.len();
        let mut rows = Vec::new();
        for (uri, pos) in found.iter().take(self.cfg.max_results) {
            rows.push(files.loc(uri, pos, t.server.encoding).await);
        }
        let mut text = format!("{heading} {} ({total}):\n", t.here());
        for r in &rows {
            text.push_str(&r.row());
            text.push('\n');
        }
        if total > rows.len() {
            text.push_str(&format!("… {} more (raise caps.tool-lsp.max_results to see them)\n", total - rows.len()));
        }
        push_notes(&mut text, &t.notes);
        let value = json!({"at": t.here(), "total": total, "locations": rows.iter().map(Loc::json).collect::<Vec<_>>(), "notes": t.notes});
        (value, text)
    }

    pub async fn definition(&self, req: &ExecRequest) -> ExecOutcome {
        let t = match self.target(&req.view.args, &req.cancel).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let v = match self.request(&t.server, "textDocument/definition", t.params(), &req.cancel).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (value, text) = self.render_locations(&t, locations(&v), "definition of").await;
        done(value, text)
    }

    pub async fn references(&self, req: &ExecRequest) -> ExecOutcome {
        let t = match self.target(&req.view.args, &req.cancel).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let include = req.view.args.get("include_declaration").and_then(Value::as_bool).unwrap_or(true);
        let mut params = t.params();
        params["context"] = json!({"includeDeclaration": include});
        let v = match self.request(&t.server, "textDocument/references", params, &req.cancel).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (value, text) = self.render_locations(&t, locations(&v), "references to").await;
        done(value, text)
    }

    pub async fn hover(&self, req: &ExecRequest) -> ExecOutcome {
        let t = match self.target(&req.view.args, &req.cancel).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let v = match self.request(&t.server, "textDocument/hover", t.params(), &req.cancel).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let contents = clip(hover_text(v.get("contents").unwrap_or(&Value::Null)).trim(), HOVER_MAX);
        let mut text =
            if contents.is_empty() { format!("{}: no hover information.\n", t.here()) } else { format!("{}:\n{contents}\n", t.here()) };
        push_notes(&mut text, &t.notes);
        done(json!({"at": t.here(), "contents": contents, "notes": t.notes}), text)
    }

    pub async fn diagnostics(&self, req: &ExecRequest) -> ExecOutcome {
        let Some(path) = (match opt_str(&req.view.args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        }) else {
            return bad_args("`path` is required: the file whose diagnostics to report.");
        };
        let (server, rel, synced) = match self.open(path, &req.cancel).await {
            Ok(x) => x,
            Err(e) => return e,
        };
        let mut notes = Vec::new();
        if let Err(e) = self.ready(&server, &req.cancel, &mut notes).await {
            return lsp_err(e);
        }
        // Pull (`textDocument/diagnostic`) where the server offers it: it answers for exactly the
        // content it has. rust-analyzer, for one, stops pushing once it has offered pull.
        let items = if server.supports("diagnosticProvider") {
            match self.pull_diagnostics(&server, &synced.uri, &req.cancel).await {
                Ok(items) => items,
                Err(e) => return e,
            }
        } else {
            match self.pushed_diagnostics(&server, &synced, &req.cancel, &mut notes).await {
                Ok(Some(items)) => items,
                Ok(None) => {
                    let text = format!(
                        "{rel}: the server published no diagnostics for the current content within {} ms. Retry, or raise {DIAGNOSTICS_KEY}.\n",
                        self.cfg.diagnostics_wait_ms
                    );
                    return done(json!({"path": rel, "published": false, "total": 0, "diagnostics": [], "notes": notes}), text);
                }
                Err(e) => return e,
            }
        };
        let mut files = Files::new(self, Some((&rel, &synced.text)));
        let total = items.len();
        let mut rows = Vec::new();
        let mut text = format!("{rel}: {total} diagnostic{}\n", if total == 1 { "" } else { "s" });
        for d in items.iter().take(self.cfg.max_results) {
            let start = d.pointer("/range/start").cloned().unwrap_or(json!({"line": 0, "character": 0}));
            let loc = files.loc(&synced.uri, &start, server.encoding).await;
            let sev = severity(d.get("severity").and_then(Value::as_u64));
            let code = match d.get("code") {
                Some(Value::String(s)) => format!(" [{s}]"),
                Some(Value::Number(n)) => format!(" [{n}]"),
                _ => String::new(),
            };
            let source = d.get("source").and_then(Value::as_str).map(|s| format!(" ({s})")).unwrap_or_default();
            let message = d.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            text.push_str(&format!(
                "{}:{}:{} {sev}{code}: {}{source}\n",
                loc.path,
                loc.line,
                loc.column,
                clip(&message.replace('\n', " "), 400)
            ));
            if !loc.snippet.is_empty() {
                text.push_str(&format!("    {}\n", loc.snippet));
            }
            rows.push(json!({"path": loc.path, "line": loc.line, "column": loc.column, "severity": sev, "code": d.get("code"), "source": d.get("source"), "message": message}));
        }
        if total > rows.len() {
            text.push_str(&format!("… {} more (raise caps.tool-lsp.max_results to see them)\n", total - rows.len()));
        }
        push_notes(&mut text, &notes);
        done(json!({"path": rel, "published": true, "total": total, "diagnostics": rows, "notes": notes}), text)
    }

    /// `textDocument/diagnostic`, retried while the server reports the content as still moving.
    async fn pull_diagnostics(&self, server: &Server, uri: &str, cancel: &CancelToken) -> Result<Vec<Value>, ExecOutcome> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.cfg.diagnostics_wait_ms);
        loop {
            match server
                .client
                .request("textDocument/diagnostic", json!({"textDocument": {"uri": uri}}), self.request_timeout(), cancel)
                .await
            {
                Ok(v) => return Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
                // ServerCancelled / ContentModified: the server asks to be asked again.
                Err(LspError::Server { code: -32802 | -32801, .. }) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(PULL_RETRY).await;
                }
                Err(e) => return Err(lsp_err(e)),
            }
        }
    }

    /// The last `publishDiagnostics` for this content once it settled; `None` if none came.
    async fn pushed_diagnostics(
        &self,
        server: &Server,
        synced: &Synced,
        cancel: &CancelToken,
        notes: &mut Vec<String>,
    ) -> Result<Option<Vec<Value>>, ExecOutcome> {
        let current =
            |p: &crate::client::Published| synced.changed_at.is_none_or(|at| p.at >= at) && p.version.is_none_or(|v| v >= synced.version);
        let fresh = |c: &crate::client::Client| {
            c.diagnostics(&synced.uri).is_some_and(|p| current(&p) && p.at.elapsed() >= DIAGNOSTICS_SETTLE && c.is_ready())
        };
        let wait = Duration::from_millis(self.cfg.diagnostics_wait_ms);
        let settled = server.client.wait_until(wait, cancel, fresh).await.map_err(lsp_err)?;
        let Some(p) = server.client.diagnostics(&synced.uri).filter(|p| current(p)) else { return Ok(None) };
        if !settled {
            notes.push("the server was still publishing diagnostics at the deadline; this may not be the final list.".into());
        }
        Ok(Some(p.items))
    }

    pub async fn symbols(&self, req: &ExecRequest) -> ExecOutcome {
        let args = &req.view.args;
        let (path, query) = match (opt_str(args, "path"), opt_str(args, "query")) {
            (Ok(p), Ok(q)) => (p, q),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        let mut notes = Vec::new();
        let (flat, enc, known) = if let Some(path) = path {
            let (server, rel, synced) = match self.open(path, &req.cancel).await {
                Ok(x) => x,
                Err(e) => return e,
            };
            if let Err(e) = self.ready(&server, &req.cancel, &mut notes).await {
                return lsp_err(e);
            }
            let v =
                match self.request(&server, "textDocument/documentSymbol", json!({"textDocument": {"uri": synced.uri}}), &req.cancel).await
                {
                    Ok(v) => v,
                    Err(e) => return e,
                };
            let mut flat = Vec::new();
            outline(&v, &[], &mut flat);
            let flat: Vec<_> = flat.into_iter().map(|(n, c, k, s, u)| (n, c, k, s, u.unwrap_or_else(|| synced.uri.clone()))).collect();
            (flat, server.encoding, Some((rel, synced.text)))
        } else {
            let Some(q) = query else {
                return bad_args("give `path` (document outline) or `query` (workspace symbol search), or both.");
            };
            let cfg = match self.workspace_language(args) {
                Ok(c) => c,
                Err(e) => return e,
            };
            let servers = match self.workspace_servers(cfg, &req.cancel).await {
                Ok(s) => s,
                Err(e) => return e,
            };
            let mut flat = Vec::new();
            let mut enc = Encoding::Utf16;
            for server in servers {
                if let Err(e) = self.ready(&server, &req.cancel, &mut notes).await {
                    return lsp_err(e);
                }
                let v = match self.request(&server, "workspace/symbol", json!({"query": q}), &req.cancel).await {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                enc = server.encoding;
                let mut part = Vec::new();
                outline(&v, &[], &mut part);
                flat.extend(part.into_iter().filter_map(|(n, c, k, s, u)| Some((n, c, k, s, u?))));
            }
            (flat, enc, None)
        };
        let needle = query.map(str::to_lowercase);
        let hits: Vec<_> =
            flat.into_iter().filter(|(n, ..)| needle.as_ref().is_none_or(|q| path.is_none() || n.to_lowercase().contains(q))).collect();
        let mut files = Files::new(self, known.as_ref().map(|(r, t)| (r.as_str(), t.as_str())));
        let total = hits.len();
        let mut rows = Vec::new();
        let scope = match (path, query) {
            (Some(p), Some(q)) => format!("symbols matching `{q}` in {p}"),
            (Some(p), None) => format!("outline of {p}"),
            (None, q) => format!("workspace symbols matching `{}`", q.unwrap_or("")),
        };
        let mut text = format!("{scope} ({total}):\n");
        for (name, containers, kind, start, uri) in hits.iter().take(self.cfg.max_results) {
            let loc = files.loc(uri, start, enc).await;
            let indent = if path.is_some() { "  ".repeat(containers.len()) } else { String::new() };
            let container =
                if path.is_none() && !containers.is_empty() { format!(" (in {})", containers.join("::")) } else { String::new() };
            text.push_str(&format!("{indent}{} {name}{container}  {}:{}:{}\n", symbol_kind(*kind), loc.path, loc.line, loc.column));
            rows.push(json!({"name": name, "kind": symbol_kind(*kind), "container": containers.join("::"), "path": loc.path, "line": loc.line, "column": loc.column}));
        }
        if total > rows.len() {
            text.push_str(&format!("… {} more (raise caps.tool-lsp.max_results to see them)\n", total - rows.len()));
        }
        push_notes(&mut text, &notes);
        done(json!({"total": total, "symbols": rows, "notes": notes}), text)
    }

    pub async fn rename_preview(&self, req: &ExecRequest) -> ExecOutcome {
        let new_name = match opt_str(&req.view.args, "new_name") {
            Ok(Some(n)) => n.to_string(),
            Ok(None) => return bad_args("`new_name` is required: the name to preview renaming to."),
            Err(e) => return e,
        };
        let t = match self.target(&req.view.args, &req.cancel).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mut params = t.params();
        params["newName"] = json!(new_name);
        let v = match self.request(&t.server, "textDocument/rename", params, &req.cancel).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (edits, resource_ops) = workspace_edits(&v);
        let mut files = Files::new(self, Some((&t.rel, &t.text)));
        let mut text = format!("rename preview at {} → `{new_name}` (nothing was written):\n", t.here());
        let mut out = Vec::new();
        let mut total_edits = 0;
        for (uri, list) in &edits {
            total_edits += list.len();
            let (path, inside) = self.display_uri(uri);
            let before = if inside { files.text(&path).await } else { None };
            let Some(before) = before else {
                text.push_str(&format!("{path}: {} edit(s) outside the workspace (not previewed)\n", list.len()));
                out.push(json!({"path": path, "edits": list.len(), "previewed": false}));
                continue;
            };
            match text::apply_edits(&before, list, t.server.encoding) {
                Ok(after) => {
                    let hunks = unified_hunks(&before, &after, DIFF_CONTEXT);
                    text.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
                    for h in &hunks {
                        text.push_str(h);
                        if !h.ends_with('\n') {
                            text.push('\n');
                        }
                    }
                    out.push(json!({"path": path, "edits": list.len(), "previewed": true, "hunks": hunks}));
                }
                Err(e) => {
                    text.push_str(&format!("{path}: the server's edits could not be applied to the current file ({e})\n"));
                    out.push(json!({"path": path, "edits": list.len(), "previewed": false, "error": e}));
                }
            }
        }
        if edits.is_empty() && resource_ops.is_empty() {
            text.push_str("the server proposed no edits.\n");
        }
        for op in &resource_ops {
            text.push_str(&format!("would also {op}\n"));
        }
        text.push_str(&format!("{total_edits} edit(s) in {} file(s).\n", edits.len()));
        push_notes(&mut text, &t.notes);
        done(
            json!({"at": t.here(), "new_name": new_name, "files": out, "resource_ops": resource_ops, "written": false, "notes": t.notes}),
            text,
        )
    }
}

type Edit = (u32, u32, u32, u32, String);

/// WorkspaceEdit → per-document text edits (sorted by uri) and described file operations.
fn workspace_edits(v: &Value) -> (Vec<(String, Vec<Edit>)>, Vec<String>) {
    let edit = |e: &Value| -> Option<Edit> {
        let n = |p: &str| e.pointer(p).and_then(Value::as_u64).map(|x| x as u32);
        Some((
            n("/range/start/line")?,
            n("/range/start/character")?,
            n("/range/end/line")?,
            n("/range/end/character")?,
            e.get("newText")?.as_str()?.to_string(),
        ))
    };
    let mut docs: std::collections::BTreeMap<String, Vec<Edit>> = std::collections::BTreeMap::new();
    let mut ops = Vec::new();
    if let Some(changes) = v.get("changes").and_then(Value::as_object) {
        for (uri, list) in changes {
            docs.entry(uri.clone()).or_default().extend(list.as_array().into_iter().flatten().filter_map(edit));
        }
    }
    for c in v.get("documentChanges").and_then(Value::as_array).into_iter().flatten() {
        match c.get("kind").and_then(Value::as_str) {
            Some("create") => ops.push(format!("create {}", c.get("uri").and_then(Value::as_str).unwrap_or("?"))),
            Some("rename") => ops.push(format!(
                "rename {} → {}",
                c.get("oldUri").and_then(Value::as_str).unwrap_or("?"),
                c.get("newUri").and_then(Value::as_str).unwrap_or("?")
            )),
            Some("delete") => ops.push(format!("delete {}", c.get("uri").and_then(Value::as_str).unwrap_or("?"))),
            _ => {
                if let Some(uri) = c.pointer("/textDocument/uri").and_then(Value::as_str) {
                    docs.entry(uri.to_string())
                        .or_default()
                        .extend(c.get("edits").and_then(Value::as_array).into_iter().flatten().filter_map(edit));
                }
            }
        }
    }
    (docs.into_iter().collect(), ops)
}

fn push_notes(text: &mut String, notes: &[String]) {
    for n in notes {
        text.push_str(&format!("note: {n}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_definition_shape_yields_uri_and_start() {
        let loc = json!({"uri": "file:///a.rs", "range": {"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 6}}});
        let link = json!({"targetUri": "file:///b.rs", "targetRange": {"start": {"line": 0, "character": 0}},
                          "targetSelectionRange": {"start": {"line": 4, "character": 7}}});
        assert_eq!(locations(&loc), vec![("file:///a.rs".into(), json!({"line": 1, "character": 3}))]);
        assert_eq!(locations(&json!([link])), vec![("file:///b.rs".into(), json!({"line": 4, "character": 7}))]);
        assert!(locations(&Value::Null).is_empty());
    }

    #[test]
    fn hover_contents_render_from_every_shape() {
        assert_eq!(hover_text(&json!({"kind": "markdown", "value": "```rust\nfn add()\n```"})), "```rust\nfn add()\n```");
        assert_eq!(hover_text(&json!([{"language": "rust", "value": "fn a()"}, "docs"])), "```rust\nfn a()\n```\n\ndocs");
    }

    #[test]
    fn outlines_flatten_with_containers_and_qualified_names_match_the_tail() {
        let v = json!([{"name": "Pool", "kind": 23, "selectionRange": {"start": {"line": 2, "character": 11}},
                        "children": [{"name": "new", "kind": 6, "selectionRange": {"start": {"line": 5, "character": 11}}}]}]);
        let mut flat = Vec::new();
        outline(&v, &[], &mut flat);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[1].1, vec!["Pool".to_string()]);
        let (q, n) = split_symbol("Pool::new");
        assert_eq!((q.clone(), n), (vec!["Pool"], "new"));
        assert!(qualifier_matches(&q, &flat[1].1));
        assert!(!qualifier_matches(&["Other"], &flat[1].1));
        // rust-analyzer's workspace symbols name impl containers `impl Pool`.
        assert!(qualifier_matches(&q, &["impl Pool".to_string()]));
        assert_eq!(symbol_kind(12), "function");
        assert_eq!(symbol_kind(0), "symbol");
    }

    #[test]
    fn workspace_edits_merge_both_shapes_and_describe_file_operations() {
        let v = json!({
            "changes": {"file:///a.rs": [{"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "sum"}]},
            "documentChanges": [
                {"textDocument": {"uri": "file:///b.rs", "version": 1},
                 "edits": [{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "sum"}]},
                {"kind": "rename", "oldUri": "file:///c.rs", "newUri": "file:///d.rs"}
            ]
        });
        let (docs, ops) = workspace_edits(&v);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].1[0], (0, 3, 0, 6, "sum".to_string()));
        assert_eq!(ops, vec!["rename file:///c.rs → file:///d.rs".to_string()]);
    }
}
