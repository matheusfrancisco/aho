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

pub const DEFAULT_READY_MS: u64 = 60_000;
pub const DEFAULT_REQUEST_MS: u64 = 30_000;
pub const DEFAULT_DIAGNOSTICS_MS: u64 = 15_000;
pub const DEFAULT_MAX_RESULTS: u64 = 50;

/// Servers whose binaries the plan probes: the capability is Locked when none is on PATH.
pub const KNOWN_SERVERS: &[&str] = &["rust-analyzer", "gopls", "pyright-langserver", "typescript-language-server", "clangd"];

/// Built-in rows a configured language falls back to field by field (so setting only
/// `caps.tool-lsp.servers.rust.command` keeps the rust globs and root markers).
fn builtin(language: &str) -> Option<Value> {
    Some(match language {
        // No `cargo check` on save/startup: the tools are read-only and answer from the server's
        // own analysis (native diagnostics), so the server is not asked to build the workspace.
        "rust" => json!({
            "command": ["rust-analyzer"], "globs": ["*.rs"], "roots": ["Cargo.toml"],
            "init_options": {"checkOnSave": false},
            "settings": {"rust-analyzer": {"checkOnSave": false}}
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

fn default_servers() -> Value {
    json!({"rust": builtin("rust")})
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
}

impl Default for LspConfig {
    fn default() -> Self {
        let (servers, broken) = parse_servers(default_servers().as_object().cloned().unwrap_or_default());
        LspConfig {
            servers,
            broken,
            ready_timeout_ms: DEFAULT_READY_MS,
            request_timeout_ms: DEFAULT_REQUEST_MS,
            diagnostics_wait_ms: DEFAULT_DIAGNOSTICS_MS,
            max_results: DEFAULT_MAX_RESULTS as usize,
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

fn server(language: &str, row: &Value) -> Result<ServerConfig, BadServer> {
    let key = |f: &str| format!("{SERVERS_KEY}.{language}.{f}");
    let bad = |error: String, fix: String| BadServer { language: language.to_string(), error, fix };
    let base = builtin(language).unwrap_or(Value::Null);
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

fn parse_servers(table: serde_json::Map<String, Value>) -> (Vec<ServerConfig>, Vec<BadServer>) {
    let (mut ok, mut broken) = (Vec::new(), Vec::new());
    for (language, row) in table {
        if row.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        match server(&language, &row) {
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

impl Config for LspConfig {
    const PREFIX: &'static str = "caps.tool-lsp";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                SERVERS_KEY,
                Ty::Table,
                Some(default_servers()),
                "Language servers: `caps.tool-lsp.servers.<language>.{command, globs, roots, init_options, settings, enabled}`. Built-in rows for rust, go, python, typescript and c fill any field left unset.",
            ),
            ConfigKey::new(
                READY_KEY,
                Ty::Int,
                Some(json!(DEFAULT_READY_MS)),
                "How long an lsp call waits for its server to finish indexing before answering anyway.",
            ),
            ConfigKey::new(REQUEST_KEY, Ty::Int, Some(json!(DEFAULT_REQUEST_MS)), "How long one request to a language server may take."),
            ConfigKey::new(
                DIAGNOSTICS_KEY,
                Ty::Int,
                Some(json!(DEFAULT_DIAGNOSTICS_MS)),
                "How long lsp_diagnostics waits for the server to publish diagnostics for the current file content.",
            ),
            ConfigKey::new(
                MAX_RESULTS_KEY,
                Ty::Int,
                Some(json!(DEFAULT_MAX_RESULTS)),
                "Most locations / symbols / diagnostics one lsp call returns.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let mut table = l.table(SERVERS_KEY);
        if table.is_empty() && l.get(SERVERS_KEY).is_none() {
            table = default_servers().as_object().cloned().unwrap_or_default();
        }
        let (servers, broken) = parse_servers(table);
        Ok(LspConfig {
            servers,
            broken,
            ready_timeout_ms: positive(l, READY_KEY, DEFAULT_READY_MS)?,
            request_timeout_ms: positive(l, REQUEST_KEY, DEFAULT_REQUEST_MS)?,
            diagnostics_wait_ms: positive(l, DIAGNOSTICS_KEY, DEFAULT_DIAGNOSTICS_MS)?,
            max_results: positive(l, MAX_RESULTS_KEY, DEFAULT_MAX_RESULTS)? as usize,
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
}
