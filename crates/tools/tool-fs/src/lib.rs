//! The fs tools: `read`, `read_image`, `glob`, `grep`, `write`, `edit`,
//! `str_replace_editor`. They run in whatever world is bound (FsRead + FsWrite handles).
//!
//! Read ledger: every read records `path → version`. `write`/`edit` of an existing file that was
//! not read in this session is refused with `FS_NOT_OBSERVED`; a file that changed since it was
//! read is refused with `FS_STALE_VERSION` (the world compares the version atomically).
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aho_cap::*;
use aho_fs::{FileVersion, FsError, FsRead, FsWrite, MAX_HITS, lexical_rel};
use aho_log::OpKind;
use aho_types::*;
use serde_json::json;

/// Lines returned by `read` when no `limit` is given.
pub const DEFAULT_READ_LINES: u32 = 2000;
/// Largest image `read_image` inlines.
pub const MAX_IMAGE_BYTES: usize = 5 << 20;

/// Exactly the handles this tool holds; reflected into the manifest as "tool-fs can: fs-read, fs-write".
pub struct FsDeps {
    pub read: FsRead,
    pub write: FsWrite,
}

impl Deps for FsDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsRead, HandleKind::FsWrite];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(FsDeps { read: r.handle()?, write: r.handle()? })
    }
}

/// `path → version last read or written in this session`, keyed by workspace-relative path.
#[derive(Clone, Default)]
pub struct ReadLedger(Arc<Mutex<BTreeMap<String, FileVersion>>>);

impl ReadLedger {
    pub fn get(&self, key: &str) -> Option<FileVersion> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(key).copied()
    }
    pub fn record(&self, key: String, v: FileVersion) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(key, v);
    }
    /// Forget everything (compaction clears the ledger).
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

#[derive(Clone)]
pub struct ToolFs {
    read: FsRead,
    write: FsWrite,
    ledger: ReadLedger,
}

impl Capability for ToolFs {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = FsDeps;
    type Config = ();

    fn build(deps: FsDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolFs { read: deps.read, write: deps.write, ledger: ReadLedger::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let me = self.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move { me.run(name, &req.view.args).await })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolFs,
    "tool-fs",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_fs::SEAM)],
    tools = TOOLS,
    doc = "File tools with a read-before-write ledger, version-checked edits, and applied-diff cards."
);

// ───────────────────────────── declarations ─────────────────────────────

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "read",
        "Read a text file. Returns numbered lines; `offset`/`limit` select a line window (default: the first 2000 lines).",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_read)
    .max_inline(None)
    .op(OpKind::Io, true, false)
    .present(PRESENT),
    ToolDecl::new("read_image", "Read a PNG, JPEG, GIF or WebP image and show it.", Consequence::ReadOnly)
        .safe()
        .schema(schema_read_image)
        .max_inline(None)
        .op(OpKind::Io, true, false)
        .present(PRESENT),
    ToolDecl::new(
        "glob",
        "Find files by glob pattern (`**/*.rs`), relative to the workspace or `path`. Honors .gitignore; sorted; at most 500.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_glob)
    .op(OpKind::Io, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "grep",
        "Search file contents with a regular expression. Honors .gitignore; `include` filters file names; at most 500 hits.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_grep)
    .op(OpKind::Io, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "write",
        "Create or overwrite a file with `content`. An existing file must have been read first.",
        Consequence::WorkspaceWrite,
    )
    .schema(schema_write)
    .op(OpKind::Io, false, false)
    .present(PRESENT),
    ToolDecl::new(
        "edit",
        "Replace `old_string` with `new_string` in a file you have read. `old_string` must be unique unless `replace_all`.",
        Consequence::WorkspaceWrite,
    )
    .schema(schema_edit)
    .op(OpKind::Io, false, false)
    .present(PRESENT),
    ToolDecl::new(
        "str_replace_editor",
        "Text editor: `view` a file or directory, `create` a file, `str_replace` unique text, or `insert` after a line.",
        Consequence::WorkspaceWrite,
    )
    .concurrency(editor_concurrency)
    .schema(schema_editor)
    .op(OpKind::Io, false, false)
    .present(PRESENT),
];

fn editor_concurrency(args: &Value) -> Concurrency {
    if args.get("command").and_then(Value::as_str) == Some("view") { Concurrency::Safe } else { Concurrency::Exclusive }
}

fn object(props: Value, required: &[&str]) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": props, "required": required, "additionalProperties": false}))
}

fn schema_read(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "path": {"type": "string", "description": "File path, relative to the workspace root or absolute inside it."},
            "offset": {"type": "integer", "minimum": 1, "description": "First line to return (1-based)."},
            "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return."}
        }),
        &["path"],
    )
}
fn schema_read_image(_: &BoundWorld) -> RootSchema {
    object(json!({"path": {"type": "string", "description": "Image file path."}}), &["path"])
}
fn schema_glob(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "pattern": {"type": "string", "description": "Glob; `*` stays within a directory, `**` crosses directories."},
            "path": {"type": "string", "description": "Directory to search from (default: workspace root)."}
        }),
        &["pattern"],
    )
}
fn schema_grep(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax), matched per line."},
            "path": {"type": "string", "description": "File or directory to search (default: workspace root)."},
            "include": {"type": "string", "description": "Only files matching this glob, e.g. `*.rs`."}
        }),
        &["pattern"],
    )
}
fn schema_write(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "path": {"type": "string", "description": "File to create or overwrite; parent directories are created."},
            "content": {"type": "string", "description": "The complete new file content."}
        }),
        &["path", "content"],
    )
}
fn schema_edit(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "path": {"type": "string"},
            "old_string": {"type": "string", "description": "Exact text to replace, including whitespace."},
            "new_string": {"type": "string", "description": "Replacement text."},
            "replace_all": {"type": "boolean", "default": false, "description": "Replace every occurrence."}
        }),
        &["path", "old_string", "new_string"],
    )
}
fn schema_editor(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert"]},
            "path": {"type": "string"},
            "file_text": {"type": "string", "description": "`create`: the file content."},
            "old_str": {"type": "string", "description": "`str_replace`: exact text to replace (must be unique)."},
            "new_str": {"type": "string", "description": "`str_replace`/`insert`: the new text."},
            "insert_line": {"type": "integer", "minimum": 0, "description": "`insert`: insert after this line (0 = at the top)."},
            "view_range": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2,
                           "description": "`view`: [first, last] lines, 1-based; last = -1 for the end."}
        }),
        &["command", "path"],
    )
}

// ───────────────────────────── presentation ─────────────────────────────

const PRESENT: Present = Present { call: present_call, result: present_result };

fn present_call(args: &Value) -> Card {
    if let Some(p) = args.get("pattern").and_then(Value::as_str) {
        return Card::Search { query: p.to_string(), hits: vec![] };
    }
    let path = args.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let mutates = ["content", "old_string", "old_str", "file_text", "new_str"].iter().any(|k| args.get(k).is_some());
    if mutates {
        return Card::Diff { path, hunks: vec![] };
    }
    let first = args.get("offset").and_then(Value::as_u64).map(|n| n as u32);
    let limit = args.get("limit").and_then(Value::as_u64).map(|n| n as u32);
    let range = match (first, limit) {
        (None, None) => None,
        (f, l) => {
            let f = f.unwrap_or(1).max(1);
            Some((f, f.saturating_add(l.unwrap_or(DEFAULT_READ_LINES).max(1) - 1)))
        }
    };
    Card::Read { path, range }
}

/// The card for a result value; also what the tools return as `meta`.
fn present_result(_args: &Value, v: &Value) -> Card {
    card_for(v)
}

fn card_for(v: &Value) -> Card {
    let path = v.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(h) = v.get("hunks").and_then(Value::as_array) {
        return Card::Diff { path, hunks: h.iter().filter_map(|x| x.as_str().map(str::to_string)).collect() };
    }
    if let Some(files) = v.get("files").and_then(Value::as_array) {
        let query = v.get("pattern").and_then(Value::as_str).unwrap_or("").to_string();
        return Card::Search { query, hits: files.iter().filter_map(|x| x.as_str().map(str::to_string)).collect() };
    }
    if let Some(hits) = v.get("hits").and_then(Value::as_array) {
        let query = v.get("pattern").and_then(Value::as_str).unwrap_or("").to_string();
        let hits = hits
            .iter()
            .map(|h| {
                format!(
                    "{}:{}: {}",
                    h.get("path").and_then(Value::as_str).unwrap_or(""),
                    h.get("line").and_then(Value::as_u64).unwrap_or(0),
                    h.get("text").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        return Card::Search { query, hits };
    }
    let range = match (v.get("first").and_then(Value::as_u64), v.get("last").and_then(Value::as_u64)) {
        (Some(a), Some(b)) => Some((a as u32, b as u32)),
        _ => None,
    };
    Card::Read { path, range }
}

// ───────────────────────────── execution ─────────────────────────────

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn fs_err(e: FsError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ExecOutcome> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| bad_args(format!("`{key}` is required and must be a string; nothing was done.")))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad_args(format!("`{key}` must be a string; nothing was done."))),
    }
}

fn opt_u32(args: &Value, key: &str) -> Result<Option<u32>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| bad_args(format!("`{key}` must be a non-negative integer; nothing was done."))),
    }
}

fn done(value: Value, content: Vec<ContentBlock>) -> ExecOutcome {
    let meta = Some(card_for(&value));
    ExecOutcome::Done { value, content, meta }
}

fn numbered(text: &str, first: u32) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{:>6}\t{line}\n", first as usize + i));
    }
    out
}

fn image_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn lines_of(n: usize) -> String {
    if n == 1 { "1 line".into() } else { format!("{n} lines") }
}

impl ToolFs {
    /// The ledger this instance keeps (a forked child may be seeded from it).
    pub fn ledger(&self) -> &ReadLedger {
        &self.ledger
    }

    fn key(&self, path: &str) -> String {
        lexical_rel(&self.read.0.root(), path).unwrap_or_else(|_| path.to_string())
    }

    /// Execute one of `TOOLS` by name.
    pub async fn run(&self, tool: &str, args: &Value) -> ExecOutcome {
        let r = match tool {
            "read" => self.read_tool(args).await,
            "read_image" => self.read_image(args).await,
            "glob" => self.glob(args).await,
            "grep" => self.grep(args).await,
            "write" => self.write_tool(args).await,
            "edit" => self.edit_tool(args).await,
            "str_replace_editor" => self.editor(args).await,
            other => Err(bad_args(format!("tool-fs has no tool `{other}`."))),
        };
        r.unwrap_or_else(|e| e)
    }

    async fn read_range(&self, path: &str, first: u32, last: Option<u32>) -> Result<ExecOutcome, ExecOutcome> {
        let last = last.unwrap_or(u32::MAX);
        let r = self.read.0.read(path, Some((first, last))).await.map_err(fs_err)?;
        let shown_last = if r.total_lines == 0 { 0 } else { last.min(r.total_lines) };
        let key = self.key(path);
        self.ledger.record(key.clone(), r.version);
        let mut content = if r.text.is_empty() { format!("({key} is empty)\n") } else { numbered(&r.text, first) };
        if r.partial {
            content.push_str(&format!("(lines {first}–{shown_last} of {}; pass offset/limit for more)\n", r.total_lines));
        }
        let value = json!({
            "path": key, "version": r.version.to_hex(), "total_lines": r.total_lines,
            "first": first, "last": shown_last, "partial": r.partial, "text": r.text,
        });
        Ok(done(value, vec![ContentBlock::text(content)]))
    }

    async fn read_tool(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let path = req_str(args, "path")?;
        let first = opt_u32(args, "offset")?.unwrap_or(1).max(1);
        let limit = opt_u32(args, "limit")?.unwrap_or(DEFAULT_READ_LINES).max(1);
        self.read_range(path, first, Some(first.saturating_add(limit - 1))).await
    }

    async fn read_image(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let path = req_str(args, "path")?;
        let bytes = self.read.0.read_bytes(path).await.map_err(fs_err)?;
        let key = self.key(path);
        let Some(media_type) = image_type(&bytes) else {
            return Err(ExecOutcome::err(
                "FS_NOT_AN_IMAGE",
                format!("`{key}` is not a PNG, JPEG, GIF or WebP image; nothing was shown. Use read for text."),
            ));
        };
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ExecOutcome::err(
                "FS_IMAGE_TOO_LARGE",
                format!(
                    "`{key}` is {} bytes, over the {MAX_IMAGE_BYTES}-byte image limit; nothing was shown. Downscale it first.",
                    bytes.len()
                ),
            ));
        }
        let value = json!({"path": key, "media_type": media_type, "bytes": bytes.len(), "version": FileVersion::of(&bytes).to_hex()});
        let content = vec![
            ContentBlock::text(format!("{key} ({media_type}, {} bytes)", bytes.len())),
            ContentBlock::Image { media_type: media_type.to_string(), data: bytes },
        ];
        Ok(ExecOutcome::Done { value, content, meta: Some(Card::Read { path: key, range: None }) })
    }

    async fn glob(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let pattern = req_str(args, "pattern")?;
        let base = opt_str(args, "path")?;
        let files = self.read.0.glob(pattern, base).await.map_err(fs_err)?;
        let truncated = files.len() >= MAX_HITS;
        let mut text = if files.is_empty() { format!("No files match `{pattern}`.") } else { files.join("\n") };
        if truncated {
            text.push_str(&format!("\n(first {MAX_HITS} matches; narrow the pattern to see the rest)"));
        }
        let value = json!({"pattern": pattern, "path": base.unwrap_or("."), "files": files, "truncated": truncated});
        Ok(done(value, vec![ContentBlock::text(text)]))
    }

    async fn grep(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let pattern = req_str(args, "pattern")?;
        let base = opt_str(args, "path")?;
        let include = opt_str(args, "include")?;
        let hits = self.read.0.grep(pattern, base, include).await.map_err(fs_err)?;
        let truncated = hits.len() >= MAX_HITS;
        let mut text = if hits.is_empty() {
            format!("No lines match `{pattern}`.")
        } else {
            hits.iter().map(|h| format!("{}:{}: {}", h.path, h.line, h.text)).collect::<Vec<_>>().join("\n")
        };
        if truncated {
            text.push_str(&format!("\n(first {MAX_HITS} hits; narrow the pattern or path to see the rest)"));
        }
        let hits_json: Vec<Value> = hits.iter().map(|h| json!({"path": h.path, "line": h.line, "text": h.text})).collect();
        let value = json!({"pattern": pattern, "path": base.unwrap_or("."), "include": include, "hits": hits_json, "truncated": truncated});
        Ok(done(value, vec![ContentBlock::text(text)]))
    }

    /// The version to hand the world for a mutation of `path`: `None` for a new file, the ledger
    /// version for an existing one, `FS_NOT_OBSERVED` if it exists but was never read.
    async fn expected_version(&self, path: &str, key: &str) -> Result<Option<FileVersion>, ExecOutcome> {
        match self.read.0.stat(path).await {
            Ok(s) if s.is_dir => Err(fs_err(FsError::Invalid(format!("`{key}` is a directory; nothing was written.")))),
            Ok(_) => match self.ledger.get(key) {
                Some(v) => Ok(Some(v)),
                None => Err(fs_err(FsError::NotObserved(key.to_string()))),
            },
            Err(FsError::NotFound(_)) => Ok(None),
            Err(e) => Err(fs_err(e)),
        }
    }

    fn applied(&self, d: aho_fs::AppliedDiff, text: &str) -> ExecOutcome {
        self.ledger.record(d.path.clone(), d.new_version);
        let summary = if d.created {
            format!("Created {} ({}).", d.path, lines_of(text.lines().count()))
        } else if d.hunks.is_empty() {
            format!("{} already had this content; nothing changed.", d.path)
        } else {
            format!("Updated {}:\n{}", d.path, d.hunks.join("\n"))
        };
        let value = json!({"path": d.path, "created": d.created, "version": d.new_version.to_hex(), "hunks": d.hunks});
        done(value, vec![ContentBlock::text(summary)])
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<ExecOutcome, ExecOutcome> {
        let key = self.key(path);
        let expect = self.expected_version(path, &key).await?;
        let d = self.write.0.write(path, content, expect).await.map_err(fs_err)?;
        Ok(self.applied(d, content))
    }

    async fn write_tool(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        self.write_file(req_str(args, "path")?, req_str(args, "content")?).await
    }

    async fn replace(&self, path: &str, old: &str, new: &str, all: bool) -> Result<ExecOutcome, ExecOutcome> {
        let key = self.key(path);
        let Some(expect) = self.ledger.get(&key) else {
            // A missing file is more useful to report than "not read".
            return Err(match self.read.0.stat(path).await {
                Err(e @ FsError::NotFound(_)) => fs_err(e),
                _ => fs_err(FsError::NotObserved(key)),
            });
        };
        let d = self.write.0.edit(path, old, new, all, expect).await.map_err(fs_err)?;
        Ok(self.applied(d, new))
    }

    async fn edit_tool(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let all = match args.get("replace_all") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(bad_args("`replace_all` must be a boolean; nothing was done.")),
        };
        self.replace(req_str(args, "path")?, req_str(args, "old_string")?, req_str(args, "new_string")?, all).await
    }

    async fn editor(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let path = req_str(args, "path")?;
        match req_str(args, "command")? {
            "view" => {
                let stat = self.read.0.stat(path).await.map_err(fs_err)?;
                if stat.is_dir {
                    let entries = self.read.0.list(path).await.map_err(fs_err)?;
                    let files: Vec<String> =
                        entries.iter().map(|s| if s.is_dir { format!("{}/", s.path) } else { s.path.clone() }).collect();
                    let text = if files.is_empty() { format!("{} is empty.", stat.path) } else { files.join("\n") };
                    let value = json!({"path": stat.path, "pattern": "*", "files": files});
                    return Ok(done(value, vec![ContentBlock::text(text)]));
                }
                let (first, last) = match args.get("view_range").and_then(Value::as_array) {
                    None => (1, None),
                    Some(r) => match (r.first().and_then(Value::as_i64), r.get(1).and_then(Value::as_i64)) {
                        (Some(a), Some(-1)) if a >= 1 => (a as u32, None),
                        (Some(a), Some(b)) if a >= 1 && b >= a => (a as u32, Some(b as u32)),
                        _ => {
                            return Err(bad_args(
                                "`view_range` must be [first, last] with 1 ≤ first ≤ last, or last = -1; nothing was read.",
                            ));
                        }
                    },
                };
                self.read_range(path, first, last).await
            }
            "create" => self.write_file(path, req_str(args, "file_text")?).await,
            "str_replace" => self.replace(path, req_str(args, "old_str")?, opt_str(args, "new_str")?.unwrap_or(""), false).await,
            "insert" => {
                let after =
                    opt_u32(args, "insert_line")?.ok_or_else(|| bad_args("`insert_line` is required for insert; nothing was done."))?;
                let new = req_str(args, "new_str")?;
                let key = self.key(path);
                let Some(expect) = self.ledger.get(&key) else {
                    return Err(fs_err(FsError::NotObserved(key)));
                };
                let cur = self.read.0.read(path, None).await.map_err(fs_err)?;
                if cur.version != expect {
                    return Err(fs_err(FsError::StaleVersion { path: key, expected: expect, actual: cur.version }));
                }
                let lines: Vec<&str> = cur.text.split_inclusive('\n').collect();
                if after as usize > lines.len() {
                    return Err(bad_args(format!(
                        "`insert_line` {after} is past the end of `{key}` ({}); nothing was done.",
                        lines_of(lines.len())
                    )));
                }
                let mut text: String = lines[..after as usize].concat();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(new);
                if !new.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&lines[after as usize..].concat());
                let d = self.write.0.write(path, &text, Some(expect)).await.map_err(fs_err)?;
                Ok(self.applied(d, new))
            }
            other => Err(bad_args(format!("`command` must be view, create, str_replace or insert, not `{other}`; nothing was done."))),
        }
    }
}

#[cfg(test)]
mod tests;
