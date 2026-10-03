//! `caps.tool-lsp.*`: which language server serves which files, and the call budgets.

use aho_cap::{Config, ConfigError};
use aho_config::{ConfigKey, Loadout, Ty};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

pub const SERVERS_KEY: &str = "caps.tool-lsp.servers";
pub const READY_KEY: &str = "caps.tool-lsp.ready_timeout_ms";
pub const REQUEST_KEY: &str = "caps.tool-lsp.request_timeout_ms";
pub const DIAGNOSTICS_KEY: &str = "caps.tool-lsp.diagnostics_wait_ms";
pub const MAX_RESULTS_KEY: &str = "caps.tool-lsp.max_results";
pub const ON_WRITE_KEY: &str = "caps.tool-lsp.diagnostics_on_write";
pub const WRITE_WAIT_KEY: &str = "caps.tool-lsp.write_diagnostics_wait_ms";
pub const CHECK_ON_SAVE_KEY: &str = "caps.tool-lsp.check_on_save";
pub const LATE_WAIT_KEY: &str = "caps.tool-lsp.late_diagnostics_wait_ms";

pub const DEFAULT_READY_MS: u64 = 60_000;
pub const DEFAULT_REQUEST_MS: u64 = 30_000;
pub const DEFAULT_DIAGNOSTICS_MS: u64 = 15_000;
pub const DEFAULT_MAX_RESULTS: u64 = 50;
/// Under `tools.grace_ms` (1000), so the wait does not turn the write's own result into a late one.
pub const DEFAULT_WRITE_WAIT_MS: u64 = 800;
pub const DEFAULT_LATE_WAIT_MS: u64 = 180_000;

/// Servers whose binaries the plan probes: the capability is Locked when none is on PATH.
pub const KNOWN_SERVERS: &[&str] = &["rust-analyzer", "gopls", "pyright-langserver", "typescript-language-server", "clangd"];

/// Built-in rows a configured language falls back to field by field (so setting only
/// `caps.tool-lsp.servers.rust.command` keeps the rust globs and root markers). `check` is
/// `check_on_save`: rust-analyzer then runs `cargo check` at startup and on every `didSave`, so
/// borrow-checker and trait errors its own analysis misses reach the diagnostics.
fn builtin(language: &str, check: bool) -> Option<Value> {
    Some(match language {
        "rust" => json!({
            "command": ["rust-analyzer"], "globs": ["*.rs"], "roots": ["Cargo.toml"],
            "init_options": {"checkOnSave": check},
            "settings": {"rust-analyzer": {"checkOnSave": check}}
        }),
        "go" => json!({"command": ["gopls"], "globs": ["*.go"], "roots": ["go.mod"]}),
        "python" => {
            json!({"command": ["pyright-langserver", "--stdio"], "globs": ["*.py", "*.pyi"], "roots": ["pyproject.toml", "setup.py"]})
        }
        "typescript" => {
            json!({"command": ["typescript-language-server", "--stdio"], "globs": ["*.ts", "*.tsx", "*.js", "*.jsx"], "roots": ["tsconfig.json", "package.json"]})
        }
        "c" => json!({"command": ["clangd"], "globs": ["*.c", "*.h", "*.cc", "*.cpp", "*.hpp"], "roots": ["compile_commands.json"]}),
        _ => return None,
    })
}

/// The default table: the rust row's command, globs and roots. Its `init_options`/`settings` stay
/// unset so they fall back to `builtin`, which follows `check_on_save`.
fn default_servers() -> Value {
    let mut rust = builtin("rust", true).unwrap_or(Value::Null);
    if let Some(row) = rust.as_object_mut() {
        row.remove("init_options");
        row.remove("settings");
    }
    json!({"rust": rust})
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub language: String,
    /// argv; `command[0]` is looked up on the world's PATH.
    pub command: Vec<String>,
    pub globs: GlobSet,
    pub glob_src: Vec<String>,
    /// Files whose nearest ancestor directory becomes the server's workspace root.
    pub roots: Vec<String>,
    pub init_options: Value,
    /// Answers to `workspace/configuration` (e.g. `{"rust-analyzer": {…}}`).
    pub settings: Value,
}

impl ServerConfig {
    pub fn binary(&self) -> &str {
        self.command.first().map(String::as_str).unwrap_or("")
    }
}

/// One configured language that could not be used, and the command that fixes it.
#[derive(Clone, Debug, PartialEq)]
pub struct BadServer {
    pub language: String,
    pub error: String,
    pub fix: String,
}

#[derive(Clone, Debug)]
pub struct LspConfig {
    pub servers: Vec<ServerConfig>,
    pub broken: Vec<BadServer>,
    pub ready_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub diagnostics_wait_ms: u64,
    pub max_results: usize,
    /// Append the written file's errors/warnings to `write`/`edit`/`str_replace_editor` results.
    pub diagnostics_on_write: bool,
    /// How long a write's result waits for the server's own diagnostics.
    pub write_wait_ms: u64,
    /// Send `didSave` after a write, and turn rust-analyzer's `checkOnSave` on.
    pub check_on_save: bool,
    /// How long (from the write) the background task waits for indexing and an on-save check
    /// before giving up on a late notice.
    pub late_wait_ms: u64,
}

impl Default for LspConfig {
    fn default() -> Self {
        let (servers, broken) = parse_servers(default_servers().as_object().cloned().unwrap_or_default(), true);
        LspConfig {
            servers,
            broken,
            ready_timeout_ms: DEFAULT_READY_MS,
            request_timeout_ms: DEFAULT_REQUEST_MS,
            diagnostics_wait_ms: DEFAULT_DIAGNOSTICS_MS,
            max_results: DEFAULT_MAX_RESULTS as usize,
            diagnostics_on_write: true,
            write_wait_ms: DEFAULT_WRITE_WAIT_MS,
            check_on_save: true,
            late_wait_ms: DEFAULT_LATE_WAIT_MS,
        }
    }
}

impl LspConfig {
    /// The first configured language whose globs match the workspace-relative `path`.
    pub fn for_path(&self, path: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.globs.is_match(path))
    }
    pub fn language(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.language == name)
    }
}

fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    match v? {
        Value::String(s) => Some(s.split_whitespace().map(str::to_string).collect()),
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        _ => None,
    }
}

fn server(language: &str, row: &Value, check: bool) -> Result<ServerConfig, BadServer> {
    let key = |f: &str| format!("{SERVERS_KEY}.{language}.{f}");
    let bad = |error: String, fix: String| BadServer { language: language.to_string(), error, fix };
    let base = builtin(language, check).unwrap_or(Value::Null);
    let field = |f: &str| row.get(f).filter(|v| !v.is_null()).or_else(|| base.get(f));
    let command = strings(field("command")).filter(|c| !c.is_empty()).ok_or_else(|| {
        bad(
            format!("`{}` must be a command (a string or an array of strings); this language has no server.", key("command")),
            format!("aho set {} '[\"<server-binary>\", \"<arg>\"]'", key("command")),
        )
    })?;
    let glob_src = strings(field("globs")).filter(|g| !g.is_empty()).ok_or_else(|| {
        bad(
            format!("`{}` must list file globs (e.g. [\"*.rs\"]); this language has no server.", key("globs")),
            format!("aho set {} '[\"*.ext\"]'", key("globs")),
        )
    })?;
    let mut set = GlobSetBuilder::new();
    for g in &glob_src {
        let glob = Glob::new(g).map_err(|e| {
            bad(format!("`{}` has an invalid glob `{g}` ({e}).", key("globs")), format!("aho set {} '[\"*.ext\"]'", key("globs")))
        })?;
        set.add(glob);
    }
    let globs = set
        .build()
        .map_err(|e| bad(format!("`{}` could not be compiled ({e}).", key("globs")), format!("aho explain {}", key("globs"))))?;
    let roots = strings(field("roots")).unwrap_or_default();
    Ok(ServerConfig {
        language: language.to_string(),
        command,
        globs,
        glob_src,
        roots,
        init_options: field("init_options").cloned().unwrap_or(Value::Null),
        settings: field("settings").cloned().unwrap_or(Value::Null),
    })
}

fn parse_servers(table: serde_json::Map<String, Value>, check: bool) -> (Vec<ServerConfig>, Vec<BadServer>) {
    let (mut ok, mut broken) = (Vec::new(), Vec::new());
    for (language, row) in table {
        if row.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        match server(&language, &row, check) {
            Ok(s) => ok.push(s),
            Err(b) => broken.push(b),
        }
    }
    (ok, broken)
}

fn positive(l: &Loadout, key: &str, default: u64) -> Result<u64, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().filter(|n| *n > 0).ok_or(ConfigError::Type { key: key.into(), expected: "a positive integer" }),
    }
}

fn flag(l: &Loadout, key: &str, default: bool) -> Result<bool, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or(ConfigError::Type { key: key.into(), expected: "true or false" }),
    }
}

impl Config for LspConfig {
    const PREFIX: &'static str = "caps.tool-lsp";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                SERVERS_KEY,
                Ty::Table,
                Some(default_servers()),
                "Language servers: `caps.tool-lsp.servers.<language>.{command, globs, roots, init_options, settings, enabled}`. Built-in rows for rust, go, python, typescript and c fill any field left unset; the rust row's `checkOnSave` follows caps.tool-lsp.check_on_save.",
            ),
            ConfigKey::new(
                READY_KEY,
                Ty::Int,
                Some(json!(DEFAULT_READY_MS)),
                "Milliseconds an lsp call waits for its server to finish indexing before answering anyway.",
            ),
            ConfigKey::new(
                REQUEST_KEY,
                Ty::Int,
                Some(json!(DEFAULT_REQUEST_MS)),
                "Milliseconds one request to a language server may take.",
            ),
            ConfigKey::new(
                DIAGNOSTICS_KEY,
                Ty::Int,
                Some(json!(DEFAULT_DIAGNOSTICS_MS)),
                "Milliseconds lsp_diagnostics waits for the server to publish diagnostics for the current file content.",
            ),
            ConfigKey::new(
                MAX_RESULTS_KEY,
                Ty::Int,
                Some(json!(DEFAULT_MAX_RESULTS)),
                "Most locations / symbols / diagnostics one lsp call returns.",
            ),
            ConfigKey::new(
                ON_WRITE_KEY,
                Ty::Bool,
                Some(json!(true)),
                "Append the language server's errors and warnings for the written file to write/edit/str_replace_editor results.",
            ),
            ConfigKey::new(
                WRITE_WAIT_KEY,
                Ty::Int,
                Some(json!(DEFAULT_WRITE_WAIT_MS)),
                "Milliseconds a write's result waits for the server's own diagnostics of the new content; keep it under tools.grace_ms. What arrives later comes as a late notice.",
            ),
            ConfigKey::new(
                CHECK_ON_SAVE_KEY,
                Ty::Bool,
                Some(json!(true)),
                "Send didSave after a write so servers run their on-save check (rust-analyzer: cargo check, enabled through its checkOnSave).",
            ),
            ConfigKey::new(
                LATE_WAIT_KEY,
                Ty::Int,
                Some(json!(DEFAULT_LATE_WAIT_MS)),
                "Milliseconds after a write that diagnostics may still arrive (indexing, an on-save cargo check) and be delivered as a late notice at the next request.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let check_on_save = flag(l, CHECK_ON_SAVE_KEY, true)?;
        let mut table = l.table(SERVERS_KEY);
        if table.is_empty() && l.get(SERVERS_KEY).is_none() {
            table = default_servers().as_object().cloned().unwrap_or_default();
        }
        let (servers, broken) = parse_servers(table, check_on_save);
        Ok(LspConfig {
            servers,
            broken,
            ready_timeout_ms: positive(l, READY_KEY, DEFAULT_READY_MS)?,
            request_timeout_ms: positive(l, REQUEST_KEY, DEFAULT_REQUEST_MS)?,
            diagnostics_wait_ms: positive(l, DIAGNOSTICS_KEY, DEFAULT_DIAGNOSTICS_MS)?,
            max_results: positive(l, MAX_RESULTS_KEY, DEFAULT_MAX_RESULTS)? as usize,
            diagnostics_on_write: flag(l, ON_WRITE_KEY, true)?,
            write_wait_ms: positive(l, WRITE_WAIT_KEY, DEFAULT_WRITE_WAIT_MS)?,
            check_on_save,
            late_wait_ms: positive(l, LATE_WAIT_KEY, DEFAULT_LATE_WAIT_MS)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_config::{ResolveInputs, Schema, resolve};

    fn loadout(sets: &[(&str, &str)]) -> Loadout {
        let mut schema = Schema::default();
        for k in LspConfig::keys() {
            schema.add(k);
        }
        let mut inputs = ResolveInputs::default();
        inputs.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        resolve(&inputs, &schema, false).unwrap()
    }

    #[test]
    fn the_default_is_rust_analyzer_for_rust_files() {
        let c = LspConfig::from_loadout(&loadout(&[])).unwrap();
        assert_eq!(c.servers.len(), 1);
        let rust = c.for_path("src/main.rs").unwrap();
        assert_eq!(rust.command, vec!["rust-analyzer"]);
        assert_eq!(rust.roots, vec!["Cargo.toml"]);
        assert!(c.for_path("main.go").is_none());
        assert_eq!(c.max_results, DEFAULT_MAX_RESULTS as usize);
    }

    #[test]
    fn a_partial_row_keeps_the_builtin_fields_and_a_new_language_needs_globs() {
        let c = LspConfig::from_loadout(&loadout(&[
            ("caps.tool-lsp.servers.go.command", "/opt/gopls serve"),
            ("caps.tool-lsp.servers.zig.command", "zls"),
        ]))
        .unwrap();
        let go = c.language("go").unwrap();
        assert_eq!(go.command, vec!["/opt/gopls", "serve"]);
        assert!(go.globs.is_match("cmd/main.go"));
        assert_eq!(c.broken.len(), 1);
        assert_eq!(c.broken[0].language, "zig");
        assert!(c.broken[0].error.contains("caps.tool-lsp.servers.zig.globs"), "{}", c.broken[0].error);
    }

    #[test]
    fn non_positive_budgets_are_config_errors() {
        assert!(LspConfig::from_loadout(&loadout(&[(REQUEST_KEY, "0")])).is_err());
        assert_eq!(LspConfig::from_loadout(&loadout(&[(READY_KEY, "5")])).unwrap().ready_timeout_ms, 5);
    }

    #[test]
    fn check_on_save_drives_rust_analyzers_check_unless_the_row_sets_its_own_settings() {
        let on = LspConfig::from_loadout(&loadout(&[])).unwrap();
        assert!(on.check_on_save && on.diagnostics_on_write);
        assert_eq!(on.language("rust").unwrap().settings, json!({"rust-analyzer": {"checkOnSave": true}}));
        let off = LspConfig::from_loadout(&loadout(&[(CHECK_ON_SAVE_KEY, "false")])).unwrap();
        assert!(!off.check_on_save);
        let rust = off.language("rust").unwrap();
        assert_eq!(rust.settings, json!({"rust-analyzer": {"checkOnSave": false}}));
        assert_eq!(rust.init_options, json!({"checkOnSave": false}));
        let own = LspConfig::from_loadout(&loadout(&[
            (CHECK_ON_SAVE_KEY, "false"),
            ("caps.tool-lsp.servers.rust.settings.rust-analyzer.check.command", "clippy"),
        ]))
        .unwrap();
        assert_eq!(own.language("rust").unwrap().settings, json!({"rust-analyzer": {"check": {"command": "clippy"}}}));
        assert!(LspConfig::from_loadout(&loadout(&[(ON_WRITE_KEY, "3")])).is_err());
    }
}
