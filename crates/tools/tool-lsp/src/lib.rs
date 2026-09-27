//! `lsp_*` tools: code navigation through language servers running in the bound world.
//!
//! * `lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_rename_preview` take a position as
//!   `path` + `line` + `column` (1-based; column counts characters), `path` + `symbol`, or a
//!   bare `symbol` name resolved through `workspace/symbol`.
//! * `lsp_diagnostics{path}` opens the file with its current disk content and asks for its
//!   diagnostics: pulled (`textDocument/diagnostic`) when the server offers it, else the pushed
//!   `publishDiagnostics` for exactly that content once it settles (bounded wait). The built-in
//!   rust row turns rust-analyzer's `checkOnSave` off, so no `cargo check` runs on the tools' behalf.
//! * `lsp_symbols` is the document outline (`path`) or a workspace symbol search (`query`).
//! * `lsp_rename_preview` renders the server's `WorkspaceEdit` as unified diffs and writes
//!   nothing; `workspace/applyEdit` requests from a server are always refused.
//!
//! Servers are spawned through the world's `Subprocess` handle (never `std::process`), speak
//! JSON-RPC with Content-Length framing over stdio, and live one per (language, workspace
//! root): initialized once, kept across calls, stopped (`shutdown` + `exit`, then the process
//! group) at session end or when the capability is dropped. Files are read through the world's
//! `FsRead`. Configuration lives under `caps.tool-lsp.*` (see `config`).

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::*;
use aho_fs::FsRead;
use aho_log::OpKind;
use aho_proc::SubprocessHandle;
use aho_types::*;
use serde_json::json;

pub mod client;
pub mod config;
pub mod framing;
mod ops;
pub mod pool;
pub mod text;

pub use client::{Client, LspError};
pub use config::{KNOWN_SERVERS, LspConfig, ServerConfig};
pub use pool::Pool;

pub struct LspDeps {
    pub proc: SubprocessHandle,
    pub fs: FsRead,
}

impl Deps for LspDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::FsRead];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(LspDeps { proc: r.handle()?, fs: r.handle()? })
    }
}

#[derive(Clone)]
pub struct ToolLsp {
    pool: Arc<Pool>,
}

impl ToolLsp {
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// Stops every running language server; returns how many were running.
    pub async fn shutdown(&self) -> usize {
        self.pool.shutdown().await
    }

    pub async fn run(&self, tool: &str, req: &ExecRequest) -> ExecOutcome {
        match tool {
            "lsp_definition" => self.pool.definition(req).await,
            "lsp_references" => self.pool.references(req).await,
            "lsp_hover" => self.pool.hover(req).await,
            "lsp_diagnostics" => self.pool.diagnostics(req).await,
            "lsp_symbols" => self.pool.symbols(req).await,
            "lsp_rename_preview" => self.pool.rename_preview(req).await,
            other => ops::bad_args(format!("tool-lsp has no tool `{other}`.")),
        }
    }
}

impl Capability for ToolLsp {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = LspDeps;
    type Config = LspConfig;

    fn build(deps: LspDeps, cfg: LspConfig) -> Result<Self, BuildError> {
        if cfg.servers.is_empty() {
            return Err(match cfg.broken.first() {
                Some(b) => BuildError::Probe(format!("{} Fix: {}", b.error, b.fix)),
                None => BuildError::Probe(format!(
                    "no language server is configured ({} is empty or every row is disabled). Set one, e.g. aho set {}.rust.command rust-analyzer",
                    config::SERVERS_KEY,
                    config::SERVERS_KEY
                )),
            });
        }
        Ok(ToolLsp { pool: Arc::new(Pool::new(deps.proc.0, deps.fs.0, Arc::new(cfg))) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let me = self.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move { me.run(name, &req).await })
            });
            r.tool(decl, exec);
        }
        let pool = self.pool.clone();
        r.hook(HookSpec {
            on: &[HookEvent::SessionEnd],
            if_class: None,
            if_tool: None,
            timeout_ms: 15_000,
            once: true,
            on_error: OnError::Allow,
            run: Arc::new(move |_, _| {
                let pool = pool.clone();
                Box::pin(async move {
                    pool.shutdown().await;
                    HookOutput::default()
                })
            }),
        });
    }
}

declare_capability!(
    ToolLsp,
    "tool-lsp",
    kind = Kind::Tool,
    provides = [],
    requires = [
        Requirement::Seam(aho_proc::SEAM),
        Requirement::Seam(aho_fs::SEAM),
        Requirement::AnyOf(&[
            Requirement::Binary("rust-analyzer"),
            Requirement::Binary("gopls"),
            Requirement::Binary("pyright-langserver"),
            Requirement::Binary("typescript-language-server"),
            Requirement::Binary("clangd"),
        ]),
    ],
    tools = TOOLS,
    unlock = Fix::cli("rustup component add rust-analyzer"),
    doc = "Language-server navigation (definition, references, hover, diagnostics, symbols, rename preview); servers run in the bound world, one per language and workspace root."
);

const POSITION_DOC: &str = "Give `path` + `line` (+ `column`), `path` + `symbol`, or `symbol` alone (searched workspace-wide). Lines and columns are 1-based; columns count characters.";

fn position_props() -> Value {
    json!({
        "path": {"type": "string", "description": "File, relative to the workspace root (or absolute inside it)."},
        "line": {"type": "integer", "minimum": 1, "description": "1-based line."},
        "column": {"type": "integer", "minimum": 1, "description": "1-based character column; default: the symbol's column on that line, else the first non-blank."},
        "symbol": {"type": "string", "description": "A symbol name (`add`, `Pool::new`); with `path` it is looked up in that file, alone in the whole workspace."},
        "language": {"type": "string", "description": "The configured language to search when only `symbol` is given and several are configured."}
    })
}

fn schema_position(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "description": POSITION_DOC, "properties": position_props(), "additionalProperties": false}))
}

fn schema_references(_: &BoundWorld) -> RootSchema {
    let mut p = position_props();
    p["include_declaration"] = json!({"type": "boolean", "default": true, "description": "Count the declaration itself as a reference."});
    RootSchema(json!({"type": "object", "description": POSITION_DOC, "properties": p, "additionalProperties": false}))
}

fn schema_rename(_: &BoundWorld) -> RootSchema {
    let mut p = position_props();
    p["new_name"] = json!({"type": "string", "description": "The name to preview renaming the symbol to."});
    RootSchema(
        json!({"type": "object", "description": POSITION_DOC, "properties": p, "required": ["new_name"], "additionalProperties": false}),
    )
}

fn schema_diagnostics(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {"path": {"type": "string", "description": "File whose errors and warnings to report, for its current content on disk."}},
        "required": ["path"],
        "additionalProperties": false
    }))
}

fn schema_symbols(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Outline of this file."},
            "query": {"type": "string", "description": "Workspace symbol search (without `path`), or a name filter for the outline (with `path`)."},
            "language": {"type": "string", "description": "The configured language to search when several are configured."}
        },
        "additionalProperties": false
    }))
}

/// Every tool may start a language server in the world on first use: `SpawnsProcess`. None writes.
pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "lsp_definition",
        "Go to definition: where the symbol at a position (or the named symbol) is defined, as path:line:col with the line's text.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_position)
    .safe()
    .op(OpKind::Io, true, false),
    ToolDecl::new(
        "lsp_references",
        "Every reference to the symbol at a position (or the named symbol) across the workspace, as path:line:col rows, capped.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_references)
    .safe()
    .op(OpKind::Io, true, false),
    ToolDecl::new("lsp_hover", "Type, signature and docs of the symbol at a position (or the named symbol).", Consequence::SpawnsProcess)
        .schema(schema_position)
        .safe()
        .op(OpKind::Io, true, false),
    ToolDecl::new(
        "lsp_diagnostics",
        "Compiler/language-server errors and warnings for a file's current content, as path:line:col severity: message.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_diagnostics)
    .safe()
    .op(OpKind::Io, true, false),
    ToolDecl::new(
        "lsp_symbols",
        "Symbols: a file's outline (`path`) or a workspace-wide search by name (`query`), with kinds and locations.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_symbols)
    .safe()
    .op(OpKind::Io, true, false),
    ToolDecl::new(
        "lsp_rename_preview",
        "Preview renaming a symbol: the language server's edits as unified diffs per file. Read-only; nothing is written.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_rename)
    .safe()
    .op(OpKind::Io, true, false),
];
