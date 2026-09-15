//! One language server per (language, workspace root), started on first use through the world's
//! `Subprocess`, initialized once, kept alive across calls, and shut down with the capability.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_cap::CancelToken;
use aho_fs::{Fs, lexical_rel};
use aho_proc::{ProcError, SpawnSpec, Subprocess};
use serde_json::{Value, json};

use crate::client::{Client, LspError};
use crate::config::{LspConfig, SERVERS_KEY, ServerConfig};
use crate::text::{Encoding, path_to_uri, uri_to_path};

/// Grace for `shutdown` + `exit` before the server's process group is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The shell line every server runs under: stdout is the protocol stream and the world merges
/// stderr into it, so stderr is discarded; a missing binary exits 127.
const LAUNCH: &str = "command -v \"$0\" >/dev/null 2>&1 || exit 127; exec \"$0\" \"$@\" 2>/dev/null";

/// A document as the server last saw it.
struct Doc {
    version: i64,
    text: String,
}

pub struct Server {
    pub language: String,
    /// Absolute root the server was initialized with.
    pub root: String,
    pub client: Client,
    pub capabilities: Value,
    pub encoding: Encoding,
    docs: tokio::sync::Mutex<HashMap<String, Doc>>,
}

/// What `Server::sync` did for one file.
pub struct Synced {
    pub uri: String,
    pub text: String,
    pub version: i64,
    /// When the server was told about new content; `None` = it already had this text.
    pub changed_at: Option<Instant>,
}

impl Server {
    /// Opens `rel` with its current content, or sends the full new text if it changed on disk.
    pub async fn sync(&self, fs: &dyn Fs, rel: &str, language_id: &str) -> Result<Synced, String> {
        let text = fs.read(rel, None).await.map_err(|e| e.to_string())?.text;
        let uri = path_to_uri(&join(&fs.root(), rel));
        let mut docs = self.docs.lock().await;
        let (version, changed_at) = match docs.get_mut(&uri) {
            Some(doc) if doc.text == text => (doc.version, None),
            Some(doc) => {
                doc.version += 1;
                doc.text = text.clone();
                let params = json!({"textDocument": {"uri": uri, "version": doc.version}, "contentChanges": [{"text": text}]});
                self.client.notify("textDocument/didChange", params).await.map_err(|e| e.to_string())?;
                (doc.version, Some(Instant::now()))
            }
            None => {
                let params = json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}});
                self.client.notify("textDocument/didOpen", params).await.map_err(|e| e.to_string())?;
                docs.insert(uri.clone(), Doc { version: 1, text: text.clone() });
                (1, Some(Instant::now()))
            }
        };
        Ok(Synced { uri, text, version, changed_at })
    }

    pub fn supports(&self, provider: &str) -> bool {
        self.capabilities.get(provider).is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    }

    /// `shutdown`, `exit`, then the process group if it is still there.
    async fn stop(&self, timeout: Duration) {
        let cancel = CancelToken::new();
        if self.client.dead().is_none() && self.client.request("shutdown", Value::Null, timeout, &cancel).await.is_ok() {
            // `exit` after a successful shutdown; a write failure means it is already gone.
            drop(self.client.notify("exit", Value::Null).await);
        }
        let child = self.client.child();
        if tokio::time::timeout(timeout, child.wait()).await.is_err() {
            drop(child.kill().await);
        }
    }
}

pub fn join(root: &str, rel: &str) -> String {
    if rel == "." || rel.is_empty() { root.to_string() } else { format!("{}/{rel}", root.trim_end_matches('/')) }
}

/// `languageId` for `didOpen`, from the file extension (LSP's well-known identifiers).
pub fn language_id(language: &str, rel: &str) -> String {
    let ext = rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        _ => language,
    }
    .to_string()
}

pub struct Pool {
    proc: Arc<dyn Subprocess>,
    pub fs: Arc<dyn Fs>,
    pub cfg: Arc<LspConfig>,
    servers: tokio::sync::Mutex<BTreeMap<(String, String), Arc<Server>>>,
}

impl Pool {
    pub fn new(proc: Arc<dyn Subprocess>, fs: Arc<dyn Fs>, cfg: Arc<LspConfig>) -> Pool {
        Pool { proc, fs, cfg, servers: tokio::sync::Mutex::new(BTreeMap::new()) }
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.request_timeout_ms)
    }

    /// Workspace-relative form of a path the model gave (absolute inside the root, or relative).
    pub fn rel(&self, path: &str) -> Result<String, String> {
        lexical_rel(&self.fs.root(), path).map_err(|e| e.to_string())
    }

    /// Workspace-relative path of a server URI, or the absolute path when it lies outside.
    pub fn display_uri(&self, uri: &str) -> (String, bool) {
        match uri_to_path(uri) {
            Some(abs) => match lexical_rel(&self.fs.root(), &abs) {
                Ok(rel) => (rel, true),
                Err(_) => (abs, false),
            },
            None => (uri.to_string(), false),
        }
    }

    /// The configured language for `rel`, with the fix when none matches.
    pub fn language_for(&self, rel: &str) -> Result<&ServerConfig, String> {
        if let Some(s) = self.cfg.for_path(rel) {
            return Ok(s);
        }
        if let Some(b) = self.cfg.broken.iter().find(|b| rel.ends_with(&format!(".{}", b.language))) {
            return Err(format!("{} Fix: {}", b.error, b.fix));
        }
        let ext = rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("<ext>");
        Err(format!(
            "no language server is configured for `{rel}`, so nothing was asked. Configure one: aho set {SERVERS_KEY}.<language>.command <server> and {SERVERS_KEY}.<language>.globs '[\"*.{ext}\"]'."
        ))
    }

    /// Nearest ancestor of `rel` (inside the workspace) holding one of the root markers.
    pub async fn root_for(&self, cfg: &ServerConfig, rel: &str) -> String {
        let root = self.fs.root();
        let mut dir = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
        loop {
            for marker in &cfg.roots {
                let candidate = if dir == "." { marker.clone() } else { format!("{dir}/{marker}") };
                if self.fs.stat(&candidate).await.is_ok() {
                    return join(&root, &dir);
                }
            }
            if dir == "." {
                return root;
            }
            dir = dir.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
        }
    }

    /// Running servers for `language` (any root).
    pub async fn running(&self, language: &str) -> Vec<Arc<Server>> {
        self.servers.lock().await.iter().filter(|((l, _), s)| l == language && s.client.dead().is_none()).map(|(_, s)| s.clone()).collect()
    }

    /// The live server for (language, root), started and initialized if needed. A server that
    /// died is replaced.
    pub async fn server(&self, cfg: &ServerConfig, root: &str, cancel: &CancelToken) -> Result<Arc<Server>, LspError> {
        let key = (cfg.language.clone(), root.to_string());
        let mut map = self.servers.lock().await;
        if let Some(s) = map.get(&key) {
            if s.client.dead().is_none() {
                return Ok(s.clone());
            }
            map.remove(&key);
        }
        let s = Arc::new(self.start(cfg, root, cancel).await?);
        map.insert(key, s.clone());
        Ok(s)
    }

    async fn start(&self, cfg: &ServerConfig, root: &str, cancel: &CancelToken) -> Result<Server, LspError> {
        let mut argv = vec!["/bin/sh".to_string(), "-c".to_string(), LAUNCH.to_string()];
        argv.extend(cfg.command.iter().cloned());
        let spec = SpawnSpec { argv, cwd: Some(root.to_string()), ..SpawnSpec::default() };
        let bin = cfg.binary().to_string();
        let child = self.proc.spawn_interactive(spec).await.map_err(|e| match e {
            ProcError::NotFound(_) => LspError::Io(format!(
                "`/bin/sh` was not found in this world, so `{bin}` could not be started. Language servers need a POSIX shell in the world."
            )),
            other => LspError::Io(format!("`{bin}` could not be started: {other}")),
        })?;
        let root_uri = path_to_uri(root);
        let name = root.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(root);
        let folders = json!([{"uri": root_uri, "name": name}]);
        let client = Client::new(&bin, child, cfg.settings.clone(), folders.clone());
        let params = json!({
            "processId": Value::Null,
            "clientInfo": {"name": "aho", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": root,
            "workspaceFolders": folders,
            "initializationOptions": cfg.init_options,
            "capabilities": client_capabilities(),
        });
        let init = match client.request("initialize", params, self.request_timeout(), cancel).await {
            Ok(v) => v,
            Err(e @ LspError::Exited(_)) => {
                // The launch line exits 127 when the binary is missing; the write or the read may
                // notice first, so ask the process itself.
                let exit = tokio::time::timeout(SHUTDOWN_GRACE, client.child().wait()).await;
                if matches!(exit, Ok(Ok(x)) if x.code == 127) {
                    return Err(LspError::Exited(format!(
                        "`{bin}` ({SERVERS_KEY}.{}.command) is not on this world's PATH; no language server was started. Install it (rust: `rustup component add rust-analyzer`) or set {SERVERS_KEY}.{}.command to its full path.",
                        cfg.language, cfg.language
                    )));
                }
                drop(client.child().kill().await);
                return Err(e);
            }
            Err(e) => {
                drop(client.child().kill().await);
                return Err(e);
            }
        };
        let capabilities = init.get("capabilities").cloned().unwrap_or(Value::Null);
        let encoding = Encoding::parse(capabilities.get("positionEncoding").and_then(Value::as_str));
        client.notify("initialized", json!({})).await?;
        if !cfg.settings.is_null() {
            client.notify("workspace/didChangeConfiguration", json!({"settings": cfg.settings})).await?;
        }
        Ok(Server {
            language: cfg.language.clone(),
            root: root.to_string(),
            client,
            capabilities,
            encoding,
            docs: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Stops every server (capability stop / session end). Idempotent.
    pub async fn shutdown(&self) -> usize {
        let servers: Vec<Arc<Server>> = std::mem::take(&mut *self.servers.lock().await).into_values().collect();
        for s in &servers {
            s.stop(SHUTDOWN_GRACE).await;
        }
        servers.len()
    }
}

fn client_capabilities() -> Value {
    json!({
        "general": {"positionEncodings": ["utf-32", "utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {"configuration": true, "workspaceFolders": true, "applyEdit": false, "symbol": {}},
        "textDocument": {
            "synchronization": {"didSave": false, "willSave": false},
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "definition": {"linkSupport": true},
            "references": {},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
            "publishDiagnostics": {"versionSupport": true},
            "diagnostic": {"dynamicRegistration": false, "relatedDocumentSupport": false},
            "rename": {"prepareSupport": false}
        },
        "experimental": {"serverStatusNotification": true}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_follow_the_extension_and_fall_back_to_the_language() {
        assert_eq!(language_id("typescript", "src/app.tsx"), "typescriptreact");
        assert_eq!(language_id("c", "x/y.hpp"), "cpp");
        assert_eq!(language_id("zig", "main.zig"), "zig");
        assert_eq!(join("/w", "."), "/w");
        assert_eq!(join("/w/", "a/b"), "/w/a/b");
    }
}
