//! `10` — MCP. Tools are `mcp__<server>__<tool>`, normalized lossless-or-hashed; generations
//! swap two-phase; a server without a declared class loads quarantined.
//!
//! * [`normalize`]: `mcp__<server>__<raw>` → `[A-Za-z0-9_-]{≤64}`. If that is lossy (a character
//!   replaced or the name cut), 12 hex of SHA-256 of `server\0raw` are appended. The raw name
//!   is never parsed back out; callers keep the `(normalized, raw)` pair.
//! * [`Generations`]: phase one builds a server's complete tool list against every other server's
//!   names; phase two installs it. A fetch failure leaves the previous generation in place; a
//!   name conflict rolls that server back to zero tools.
//! * [`McpHandle`] (`HandleKind::Custom("mcp")`) is the directory of running, classified servers;
//!   [`STATUS_SERVICE`] holds every configured server's [`ServerStatus`] (ready, quarantined,
//!   failed, conflict) with the fix.
//! * [`call_outcome`] / [`render_contents`] turn MCP payloads into model content: text passes,
//!   images decode, and binary blobs render as
//!   `[binary resource: N bytes; available to programmatic callers]` so bytes never
//!   enter model history (the full payload stays in the result's `value`).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use aho_cap::{CancelToken, ExecOutcome, Handle, HandleKind, SeamId};
use aho_types::*;
use serde_json::json;
use sha2::{Digest, Sha256};

pub const SEAM: SeamId = SeamId("mcp");
pub const PREFIX: &str = "mcp__";
/// Longest model-visible tool name.
pub const MAX_NAME: usize = 64;
/// Hex characters of the disambiguating digest on a lossy name.
pub const HASH_HEX: usize = 12;
/// Protocol revision aho requests in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Revisions a server may answer with; anything else fails the handshake.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Service holding every configured server's status: `Arc<Vec<ServerStatus>>`.
pub const STATUS_SERVICE: &str = "mcp.status";
/// Tool descriptions are untrusted server text (`04`); longer ones are cut here.
pub const MAX_DESCRIPTION: usize = 2048;

/// One configured server. `Debug` never prints environment values (they may hold credentials).
#[derive(Clone)]
pub struct ServerSpec {
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
    /// Added to the inherited environment; `cred:<name>` values are already resolved.
    pub env: Vec<(String, String)>,
    /// Required for the server's tools to be model-visible; otherwise `quarantined: unclassified`.
    pub class: Option<Consequence>,
    /// Per-request limit for this server.
    pub timeout_ms: u64,
}

impl fmt::Debug for ServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerSpec")
            .field("name", &self.name)
            .field("cmd", &self.cmd)
            .field("args", &self.args)
            .field("env", &self.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>())
            .field("class", &self.class)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// A tool as the server lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteTool {
    pub raw_name: String,
    pub title: Option<String>,
    pub description: String,
    pub schema: RootSchema,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResourceContents {
    Text { uri: String, mime: Option<String>, text: String },
    Blob { uri: String, mime: Option<String>, base64: String },
}

impl ResourceContents {
    pub fn to_json(&self) -> Value {
        match self {
            ResourceContents::Text { uri, mime, text } => json!({"uri": uri, "mimeType": mime, "text": text}),
            ResourceContents::Blob { uri, mime, base64 } => json!({"uri": uri, "mimeType": mime, "blob": base64}),
        }
    }
}

/// `tools/call` result as the server sent it.
#[derive(Clone, Debug, PartialEq)]
pub struct CallResult {
    /// MCP content blocks (`text`, `image`, `audio`, `resource`, `resource_link`).
    pub content: Vec<Value>,
    pub structured: Option<Value>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpError {
    Spawn(String),
    Handshake(String),
    NameConflict {
        server: String,
        name: String,
        with: String,
    },
    Transport(String),
    Rpc {
        code: i64,
        message: String,
    },
    Timeout {
        method: String,
        ms: u64,
    },
    Cancelled,
    /// The server process ended; the text is the tail of its stderr.
    Closed(String),
    /// The server did not advertise the capability (`tools`, `resources`).
    Unsupported(String),
}

impl McpError {
    pub fn code(&self) -> &'static str {
        match self {
            McpError::Spawn(_) => "MCP_SPAWN",
            McpError::Handshake(_) => "MCP_HANDSHAKE",
            McpError::NameConflict { .. } => "MCP_NAME_CONFLICT",
            McpError::Transport(_) => "MCP_TRANSPORT",
            McpError::Rpc { .. } => "MCP_RPC",
            McpError::Timeout { .. } => "MCP_TIMEOUT",
            McpError::Cancelled => "MCP_CANCELLED",
            McpError::Closed(_) => "MCP_CLOSED",
            McpError::Unsupported(_) => "MCP_UNSUPPORTED",
        }
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Spawn(m) | McpError::Handshake(m) | McpError::Transport(m) | McpError::Unsupported(m) => f.write_str(m),
            McpError::NameConflict { server, name, with } => write!(
                f,
                "server `{server}` offers a tool that becomes `{name}`, a name {with} already uses; none of `{server}`'s tools were registered. Rename one of the servers in `mcp.servers`."
            ),
            McpError::Rpc { code, message } => write!(f, "the MCP server answered with error {code}: {message}"),
            McpError::Timeout { method, ms } => write!(
                f,
                "the MCP server did not answer `{method}` within {ms} ms; the request was cancelled. Raise `timeout_ms` for this server or retry."
            ),
            McpError::Cancelled => f.write_str("the request was cancelled before the MCP server answered"),
            McpError::Closed(tail) if tail.is_empty() => {
                f.write_str("the MCP server process exited; its tools are unavailable until aho restarts it")
            }
            McpError::Closed(tail) => write!(f, "the MCP server process exited ({tail}); its tools are unavailable until aho restarts it"),
        }
    }
}

impl std::error::Error for McpError {}

/// A connected server. Implementations are transports (`mcp-stdio`).
pub trait McpServer: Send + Sync {
    fn name(&self) -> &str;
    /// The operator-declared class; the directory only holds classified servers.
    fn class(&self) -> Option<Consequence>;
    /// Server-provided usage notes from `initialize` (untrusted text).
    fn instructions(&self) -> Option<&str>;
    /// Every page of `tools/list`.
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>>;
    fn call(&self, raw_name: &str, args: Value, cancel: CancelToken) -> BoxFuture<'_, Result<CallResult, McpError>>;
    /// Every page of `resources/list`.
    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<Resource>, McpError>>;
    /// Every page of `resources/templates/list`.
    fn list_resource_templates(&self) -> BoxFuture<'_, Result<Vec<ResourceTemplate>, McpError>>;
    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ResourceContents>, McpError>>;
}

/// Running, classified servers, in `mcp.servers` name order.
#[derive(Default)]
pub struct McpDirectory {
    servers: Vec<Arc<dyn McpServer>>,
}

impl McpDirectory {
    pub fn new(servers: Vec<Arc<dyn McpServer>>) -> McpDirectory {
        McpDirectory { servers }
    }
    pub fn servers(&self) -> &[Arc<dyn McpServer>] {
        &self.servers
    }
    pub fn get(&self, name: &str) -> Option<&Arc<dyn McpServer>> {
        self.servers.iter().find(|s| s.name() == name)
    }
    pub fn names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name()).collect()
    }
}

#[derive(Clone)]
pub struct McpHandle(pub Arc<McpDirectory>);
impl Handle for McpHandle {
    const KIND: HandleKind = HandleKind::Custom("mcp");
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerState {
    /// Tools registered (model-visible under the server's class).
    Ready,
    /// Probed but hidden from the model (`unclassified`).
    Quarantined {
        reason: String,
    },
    Failed {
        error: String,
    },
    /// Running, but its tools were rolled back to zero.
    Conflict {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerStatus {
    pub name: String,
    pub class: Option<Consequence>,
    pub state: ServerState,
    /// Normalized names: registered (`Ready`) or what would be registered (`Quarantined`).
    pub tools: Vec<String>,
    pub fix: Option<String>,
}

/// `[A-Za-z0-9_-]{≤64}`; if lossy, append 12 hex of `sha256(server\0raw)`. Never parse back.
pub fn normalize(server: &str, raw: &str) -> String {
    let full = format!("{PREFIX}{server}__{raw}");
    let mut out: String = full.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if out == full && out.len() <= MAX_NAME {
        return out;
    }
    let mut hash = Sha256::new();
    hash.update(server.as_bytes());
    hash.update([0]);
    hash.update(raw.as_bytes());
    let digest = hash.finalize();
    out.truncate(MAX_NAME - HASH_HEX - 1);
    out.push('_');
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in &digest[..HASH_HEX / 2] {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}

/// One server's installed tool list: `(normalized, tool)` in server order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Generation {
    pub number: u64,
    pub tools: Vec<(String, RemoteTool)>,
}

/// Every server's current generation.
#[derive(Clone, Debug, Default)]
pub struct Generations {
    servers: BTreeMap<String, Generation>,
}

impl Generations {
    pub fn current(&self, server: &str) -> Option<&Generation> {
        self.servers.get(server)
    }

    /// Which server a normalized name belongs to, and its raw tool.
    pub fn resolve(&self, normalized: &str) -> Option<(&str, &RemoteTool)> {
        self.servers.iter().find_map(|(s, g)| g.tools.iter().find(|(n, _)| n == normalized).map(|(_, t)| (s.as_str(), t)))
    }

    /// Phase one: the full generation, checked against itself and every other server's names.
    pub fn build(&self, server: &str, tools: Vec<RemoteTool>) -> Result<Vec<(String, RemoteTool)>, McpError> {
        let mut taken: BTreeMap<&str, String> = BTreeMap::new();
        for (other, g) in &self.servers {
            if other != server {
                for (n, _) in &g.tools {
                    taken.insert(n.as_str(), format!("server `{other}`"));
                }
            }
        }
        let mut mine: BTreeSet<String> = BTreeSet::new();
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            let name = normalize(server, &t.raw_name);
            if let Some(with) = taken.get(name.as_str()) {
                return Err(McpError::NameConflict { server: server.to_string(), name, with: with.clone() });
            }
            if !mine.insert(name.clone()) {
                return Err(McpError::NameConflict { server: server.to_string(), name, with: "another of its own tools".into() });
            }
            out.push((name, t));
        }
        Ok(out)
    }

    /// Phase two. A failed fetch keeps the previous generation; a conflict installs an empty one.
    pub fn swap(&mut self, server: &str, fetched: Result<Vec<RemoteTool>, McpError>) -> Result<&Generation, McpError> {
        let tools = fetched?;
        let number = self.servers.get(server).map(|g| g.number + 1).unwrap_or(1);
        match self.build(server, tools) {
            Ok(tools) => {
                self.servers.insert(server.to_string(), Generation { number, tools });
                self.servers.get(server).ok_or_else(|| McpError::Transport(format!("generation for `{server}` vanished")))
            }
            Err(e) => {
                self.servers.insert(server.to_string(), Generation { number, tools: Vec::new() });
                Err(e)
            }
        }
    }
}

/// Decoded byte count, without allocating the binary body. Invalid base64 never claims a size.
pub fn binary_placeholder(base64: &str) -> String {
    match visit_base64(base64, |_| {}) {
        Some(len) => format!("[binary resource: {len} bytes; available to programmatic callers]"),
        None => "[binary resource: invalid base64; available to programmatic callers]".to_string(),
    }
}

/// Standard or URL-safe base64; padding optional, ASCII whitespace ignored.
/// Rejects misplaced padding, impossible lengths, and nonzero unused trailing bits.
pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len().saturating_mul(3) / 4);
    visit_base64(s, |bytes| out.extend_from_slice(bytes))?;
    Some(out)
}

fn visit_base64(s: &str, mut emit: impl FnMut(&[u8])) -> Option<usize> {
    fn val(b: u8) -> Option<u8> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut input = s.bytes().filter(|b| !b.is_ascii_whitespace());
    let mut len = 0;
    loop {
        let Some(a) = input.next() else { return Some(len) };
        let a = val(a)?;
        let b = val(input.next()?)?;
        let c = input.next();
        let d = if c.is_some() { input.next() } else { None };
        let (c, d, count) = match (c, d) {
            (None, None) | (Some(b'='), Some(b'=')) if b & 15 == 0 => (0, 0, 1),
            (Some(c), None | Some(b'=')) => {
                let c = val(c)?;
                if c & 3 != 0 {
                    return None;
                }
                (c, 0, 2)
            }
            (Some(c), Some(d)) => (val(c)?, val(d)?, 3),
            _ => return None,
        };
        let bytes = [(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d];
        emit(&bytes[..count]);
        len += count;
        if count < 3 {
            return input.next().is_none().then_some(len);
        }
    }
}

fn content_block(v: &Value) -> ContentBlock {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    match s("type") {
        Some("text") => ContentBlock::text(s("text").unwrap_or("")),
        Some("image") => {
            let data = s("data").unwrap_or("");
            match decode_base64(data) {
                Some(bytes) => ContentBlock::Image { media_type: s("mimeType").unwrap_or("image/png").to_string(), data: bytes },
                None => ContentBlock::text(binary_placeholder(data)),
            }
        }
        Some("audio") => ContentBlock::text(binary_placeholder(s("data").unwrap_or(""))),
        Some("resource") => {
            let r = v.get("resource").cloned().unwrap_or(Value::Null);
            match (r.get("text").and_then(Value::as_str), r.get("blob").and_then(Value::as_str)) {
                (Some(t), _) => ContentBlock::text(t),
                (None, Some(b)) => ContentBlock::text(binary_placeholder(b)),
                _ => ContentBlock::text(format!("[resource {}]", r.get("uri").and_then(Value::as_str).unwrap_or("?"))),
            }
        }
        Some("resource_link") => {
            ContentBlock::text(format!("[resource link: {} ({})]", s("uri").unwrap_or("?"), s("name").unwrap_or("unnamed")))
        }
        other => ContentBlock::text(format!("[unsupported MCP content type {}]", other.unwrap_or("(none)"))),
    }
}

/// `isError` → `Err{MCP_TOOL_ERROR}` with the server's text; otherwise model content plus the
/// full result (`content`, `structuredContent`) as the canonical value.
pub fn call_outcome(r: &CallResult) -> ExecOutcome {
    let mut blocks: Vec<ContentBlock> = r.content.iter().map(content_block).collect();
    if r.is_error {
        let text: Vec<&str> = blocks.iter().filter_map(ContentBlock::as_text).collect();
        let reason = if text.is_empty() { "the MCP tool reported an error without details".to_string() } else { text.join("\n") };
        return ExecOutcome::err("MCP_TOOL_ERROR", reason);
    }
    if blocks.is_empty()
        && let Some(s) = &r.structured
    {
        blocks.push(ContentBlock::text(s.to_string()));
    }
    let mut value = json!({ "content": r.content });
    if let Some(s) = &r.structured {
        value["structuredContent"] = s.clone();
    }
    ExecOutcome::Done { value, content: blocks, meta: None }
}

/// Model-visible text for `resources/read`: text bodies verbatim, blobs as placeholders.
pub fn render_contents(contents: &[ResourceContents]) -> String {
    let mut out = Vec::new();
    for c in contents {
        match c {
            ResourceContents::Text { uri, mime, text } => {
                out.push(format!("{uri}{}\n{text}", mime.as_ref().map(|m| format!(" ({m})")).unwrap_or_default()))
            }
            ResourceContents::Blob { uri, mime, base64 } => {
                out.push(format!("{uri}{}\n{}", mime.as_ref().map(|m| format!(" ({m})")).unwrap_or_default(), binary_placeholder(base64)))
            }
        }
    }
    if out.is_empty() { "(the resource is empty)".to_string() } else { out.join("\n\n") }
}

/// Server text is untrusted: cut long descriptions on a char boundary.
pub fn clip_description(s: &str) -> String {
    match s.char_indices().nth(MAX_DESCRIPTION) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(raw: &str) -> RemoteTool {
        RemoteTool { raw_name: raw.into(), title: None, description: String::new(), schema: RootSchema(json!({"type": "object"})) }
    }

    #[test]
    fn normalize_is_lossless_when_it_can_be_and_hashed_when_not() {
        assert_eq!(normalize("github", "create_issue"), "mcp__github__create_issue");
        assert_eq!(normalize("git-hub", "list-repos"), "mcp__git-hub__list-repos");
        let dotted = normalize("fs", "read.file");
        assert_eq!(dotted, "mcp__fs__read_file_a70c27002fc8", "12 hex of sha256(\"fs\\0read.file\")");
        assert_ne!(dotted, normalize("fs", "read file"), "different raw names that sanitize alike stay distinct");
        assert_ne!(dotted, "mcp__fs__read_file", "a lossy name never equals the lossless spelling");
        let long = normalize("server", &"x".repeat(100));
        assert_eq!(long.len(), MAX_NAME);
        assert!(long.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        let exact = format!("t{}", "y".repeat(MAX_NAME - "mcp__s__".len() - 1));
        assert_eq!(normalize("s", &exact).len(), MAX_NAME, "exactly 64 is lossless");
        assert_eq!(normalize("s", &exact), format!("mcp__s__{exact}"));
    }

    #[test]
    fn swap_is_two_phase_and_a_conflict_rolls_the_server_back() {
        let mut g = Generations::default();
        assert_eq!(g.swap("a", Ok(vec![tool("x"), tool("y")])).unwrap().tools.len(), 2);
        // fetch failure keeps the previous generation
        assert!(g.swap("a", Err(McpError::Transport("down".into()))).is_err());
        assert_eq!(g.current("a").unwrap().tools.len(), 2);
        // `a__b` + `c` collides with `a` + `b__c`
        g.swap("a", Ok(vec![tool("b__c")])).unwrap();
        let e = g.swap("a__b", Ok(vec![tool("ok"), tool("c")])).unwrap_err();
        assert!(matches!(&e, McpError::NameConflict { server, name, .. } if server == "a__b" && name == "mcp__a__b__c"), "{e:?}");
        assert!(g.current("a__b").unwrap().tools.is_empty(), "conflict → zero tools, not a partial list");
        assert_eq!(g.resolve("mcp__a__b__c").map(|(s, t)| (s, t.raw_name.as_str())), Some(("a", "b__c")));
        // duplicates inside one server
        assert!(matches!(g.swap("d", Ok(vec![tool("t"), tool("t")])), Err(McpError::NameConflict { .. })));
        // a server may replace its own names
        assert_eq!(g.swap("a", Ok(vec![tool("b__c"), tool("z")])).unwrap().number, 3, "1, (failed fetch keeps 1), 2, 3");
    }

    #[test]
    fn content_rendering_keeps_bytes_out_of_model_text() {
        let r = CallResult {
            content: vec![
                json!({"type": "text", "text": "hi"}),
                json!({"type": "image", "data": "aGk=", "mimeType": "image/gif"}),
                json!({"type": "resource", "resource": {"uri": "x://b", "blob": "AAAA"}}),
                json!({"type": "resource_link", "uri": "x://l", "name": "l"}),
            ],
            structured: Some(json!({"n": 1})),
            is_error: false,
        };
        let ExecOutcome::Done { value, content, .. } = call_outcome(&r) else { panic!() };
        assert_eq!(content[0], ContentBlock::text("hi"));
        assert_eq!(content[1], ContentBlock::Image { media_type: "image/gif".into(), data: b"hi".to_vec() });
        assert_eq!(content[2], ContentBlock::text(binary_placeholder("AAAA")));
        assert_eq!(content[3], ContentBlock::text("[resource link: x://l (l)]"));
        assert_eq!(value["structuredContent"], json!({"n": 1}));
        assert_eq!(value["content"][2]["resource"]["blob"], "AAAA", "programmatic callers keep the bytes");

        let e = call_outcome(&CallResult { content: vec![json!({"type": "text", "text": "boom"})], structured: None, is_error: true });
        assert_eq!(e, ExecOutcome::err("MCP_TOOL_ERROR", "boom"));

        let text = render_contents(&[
            ResourceContents::Text { uri: "m://a".into(), mime: Some("text/plain".into()), text: "hello".into() },
            ResourceContents::Blob { uri: "m://b".into(), mime: None, base64: "iVBORw0KGgo=".into() },
        ]);
        assert!(text.contains(&binary_placeholder("iVBORw0KGgo=")));
    }

    #[test]
    fn base64_round_trips_and_rejects_garbage() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("iVBORw0KGgo=").unwrap(), [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(decode_base64("a$==").is_none());
        assert!(decode_base64("a").is_none());
    }

    #[test]
    fn binary_placeholder_counts_decoded_bytes_at_quantum_boundaries() {
        for (encoded, size) in
            [("", 0), ("AA==", 1), ("AAA=", 2), ("AAAA", 3), ("AAAAAA==", 4), ("AA", 1), ("AAA", 2), (" A A = = \n", 1), ("-_8=", 2)]
        {
            assert_eq!(binary_placeholder(encoded), format!("[binary resource: {size} bytes; available to programmatic callers]"));
            assert_eq!(decode_base64(encoded).unwrap().len(), size);
        }
        for encoded in ["A", "AA=", "A===", "AAAA=", "AA==AAAA", "AB==", "AAB=", "AA$="] {
            assert!(decode_base64(encoded).is_none(), "{encoded}");
            assert_eq!(binary_placeholder(encoded), "[binary resource: invalid base64; available to programmatic callers]");
        }
        let raw = "AA==AAAA";
        let result =
            CallResult { content: vec![json!({"type": "audio", "data": raw, "mimeType": "audio/wav"})], structured: None, is_error: false };
        let ExecOutcome::Done { value, content, .. } = call_outcome(&result) else { panic!() };
        assert_eq!(content, vec![ContentBlock::text(binary_placeholder(raw))]);
        assert_eq!(value["content"][0]["data"], raw);
    }

    #[test]
    fn descriptions_are_clipped_on_char_boundaries() {
        let long = "é".repeat(MAX_DESCRIPTION + 10);
        let c = clip_description(&long);
        assert_eq!(c.chars().count(), MAX_DESCRIPTION + 1);
        assert_eq!(clip_description("short"), "short");
    }
}
