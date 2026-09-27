//! Codex as a subagent provider: observe, don't wrap.
//!
//! Each child is the real `codex` binary, untouched and signed in with the user's own
//! credentials, running its interactive TUI in a session of aho's own tmux server (see
//! [`foreign_cli`]) with the child's workspace as its working directory:
//!
//! ```text
//! codex -C <cwd> --no-alt-screen -s workspace-write|read-only -a on-request [-m M] -- <task>
//! ```
//!
//! * Codex picks its own thread id, so the rollout is found by looking: the oldest
//!   `<codex home>/sessions/YYYY/MM/DD/rollout-*.jsonl` written since launch whose first record
//!   (`session_meta`) names this working directory, is not a Codex sub-agent thread, and is not
//!   already another child's (`<codex home>` = `subagent.codex.home`, else `$CODEX_HOME`, else
//!   `~/.codex`). Codex writes the rollout when the first prompt is submitted.
//! * The rollout is tailed by byte offset and normalized by [`normalize`]; every turn ends with
//!   `event_msg/task_complete` or `event_msg/turn_aborted`.
//! * `-s read-only` when the child's policy lacks `WorkspaceWrite`, otherwise
//!   `-s workspace-write`; `-a on-request` means Codex asks before leaving its sandbox, and a
//!   question it asks freezes the screen and comes back to the parent as a blocked run.
//!
//! Features: model; no persona, fork, or tool filter.

#![forbid(unsafe_code)]

pub mod normalize;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::LogStore;
use aho_subagent::SubagentProvider;
use aho_types::Value;
use foreign_cli::{Cli, ForeignProvider, LOG_SERVICE, Launch, LaunchCtx, LocateCtx, Record, Settings, which};
use serde_json::json;

/// Capability id; also `Subagent/Started.provider`.
pub const PROVIDER: &str = "subagent-codex";
pub const PREFIX: &str = "subagent.codex";
pub const INSTALL: &str = "brew install tmux codex";
/// A rollout modified this long before launch is not a candidate (clock/filesystem slack).
pub const LOCATE_SLACK_MS: u64 = 2_000;

/// Launch and transcript settings for Codex.
#[derive(Clone, Debug)]
pub struct Codex {
    /// `codex` on PATH, or a path.
    pub binary: String,
    /// Codex's home (holds `sessions/`).
    pub home: PathBuf,
    /// Passed to the child as `CODEX_HOME` (set when configured or inherited).
    pub export_home: bool,
}

#[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-transcript-directories")]
fn numeric_dirs(dir: &Path, keep: usize) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.into_iter().rev().take(keep).collect()
}

/// The first record of a rollout, if it is a `session_meta`.
#[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-session-meta")]
pub fn session_meta(path: &Path) -> Option<Value> {
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path).ok()?).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(line.trim_end()).ok()?;
    (v.get("type").and_then(Value::as_str) == Some("session_meta")).then(|| v.get("payload").cloned()).flatten()
}

#[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-transcript-modified")]
fn modified_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

#[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-workspace")]
fn same_dir(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b || std::fs::canonicalize(a).is_ok_and(|c| c == b)
}

impl Codex {
    /// Recent day directories under `sessions/`, newest first.
    fn day_dirs(&self) -> Vec<PathBuf> {
        let root = self.home.join("sessions");
        let mut out = Vec::new();
        for y in numeric_dirs(&root, 2) {
            for m in numeric_dirs(&y, 2) {
                out.extend(numeric_dirs(&m, 3));
            }
        }
        out
    }
}

impl Cli for Codex {
    const PROVIDER: &'static str = PROVIDER;
    const PRODUCT: &'static str = "Codex";
    const SHORT: &'static str = "codex";
    const PERSONA: bool = false;

    fn launch(&self, ctx: &LaunchCtx<'_>) -> Result<Launch, String> {
        let bin = which(&self.binary).ok_or_else(|| {
            format!("Codex (`{}`) is not on PATH, so nothing was started. Install it (`{INSTALL}`) or `aho set {PREFIX}.binary /path/to/codex`.", self.binary)
        })?;
        let mut argv = vec![bin.display().to_string(), "-C".into(), ctx.cwd.display().to_string(), "--no-alt-screen".into()];
        argv.extend(["-s".to_string(), if ctx.writable { "workspace-write" } else { "read-only" }.to_string()]);
        argv.extend(["-a".to_string(), "on-request".to_string()]);
        if let Some(m) = ctx.model {
            argv.extend(["-m".to_string(), m.to_string()]);
        }
        argv.extend(["--".to_string(), ctx.prompt.to_string()]);
        let env = if self.export_home { vec![("CODEX_HOME".to_string(), self.home.display().to_string())] } else { vec![] };
        Ok(Launch { argv, env, cli_session: None })
    }

    #[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-locate")]
    fn locate(&self, ctx: &LocateCtx<'_>) -> Option<PathBuf> {
        let since = ctx.launched_ms.saturating_sub(LOCATE_SLACK_MS);
        let mut found: Vec<PathBuf> = Vec::new();
        for day in self.day_dirs() {
            let Ok(rd) = std::fs::read_dir(&day) else { continue };
            for p in rd.flatten().map(|e| e.path()) {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("rollout-") || !name.ends_with(".jsonl") || ctx.claimed.contains(&p) {
                    continue;
                }
                if modified_ms(&p).is_none_or(|m| m < since) {
                    continue;
                }
                let Some(meta) = session_meta(&p) else { continue };
                let cwd_ok = meta.get("cwd").and_then(Value::as_str).is_some_and(|c| same_dir(c, ctx.cwd));
                let own_thread = meta.get("source").is_some_and(Value::is_string)
                    && meta.get("parent_thread_id").is_none_or(Value::is_null)
                    && meta.get("forked_from_id").is_none_or(Value::is_null);
                if cwd_ok && own_thread {
                    found.push(p);
                }
            }
        }
        // File names start with the local time the thread began: the oldest is this child's.
        found.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        found.into_iter().next()
    }

    fn normalize(&self, line: &[u8]) -> Record {
        normalize::normalize(line)
    }
}

// ───────────────────────────── capability ─────────────────────────────

pub struct CodexDeps {
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for CodexDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(CodexDeps { log: r.service(LOG_SERVICE) })
    }
}

#[derive(Clone, Debug)]
pub struct CodexConfig {
    pub cli: Codex,
    pub settings: Settings,
}

impl Config for CodexConfig {
    const PREFIX: &'static str = PREFIX;
    fn keys() -> Vec<ConfigKey> {
        let mut keys = vec![
            ConfigKey::new("subagent.codex.binary", Ty::String, Some(json!("codex")), "The Codex binary (a name on PATH or a path)."),
            ConfigKey::new(
                "subagent.codex.home",
                Ty::String,
                None,
                "Codex's home (holds sessions/YYYY/MM/DD/rollout-*.jsonl); default: $CODEX_HOME, else ~/.codex. If set, it is also passed to the child as CODEX_HOME.",
            ),
        ];
        keys.extend(Settings::keys(PREFIX, "Codex"));
        keys
    }
    #[allow(clippy::disallowed_methods, reason = "authority: subagent-codex-config")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let configured = l.str("subagent.codex.home").filter(|s| !s.is_empty()).map(PathBuf::from);
        let inherited = std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()).map(PathBuf::from);
        let export = configured.is_some() || inherited.is_some();
        let home = match configured.or(inherited) {
            Some(d) => d,
            None => std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".codex"))
                .ok_or_else(|| ConfigError::MissingKey("subagent.codex.home".into()))?,
        };
        Ok(CodexConfig {
            cli: Codex { binary: cfg_str(l, "subagent.codex.binary", "codex"), home, export_home: export },
            settings: Settings::from_loadout(l, PREFIX)?,
        })
    }
}

/// The capability: registers the provider as the `subagent` service (and concretely as
/// `subagent-codex`), plus `SessionStart`/`SessionEnd` hooks that resume and close children.
pub struct SubagentCodex {
    provider: ForeignProvider<Codex>,
}

impl SubagentCodex {
    pub fn provider(&self) -> &ForeignProvider<Codex> {
        &self.provider
    }
}

impl Capability for SubagentCodex {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = CodexDeps;
    type Config = CodexConfig;

    fn build(deps: CodexDeps, cfg: CodexConfig) -> Result<Self, BuildError> {
        let log = deps
            .log
            .ok_or(BuildError::Unsupported("the session did not seed `aho.log`, which subagent-codex mirrors its children into"))?;
        let provider = ForeignProvider::new(cfg.cli, cfg.settings, log).map_err(BuildError::Probe)?;
        Ok(SubagentCodex { provider })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let dynp: Arc<dyn SubagentProvider> = Arc::new(self.provider.clone());
        r.service(aho_subagent::SERVICE, dynp);
        r.service(PROVIDER, Arc::new(self.provider.clone()));
        for h in self.provider.hooks() {
            r.hook(h);
        }
    }
}

declare_capability!(
    SubagentCodex,
    "subagent-codex",
    kind = Kind::Provider,
    provides = [aho_subagent::SEAM],
    requires = [Requirement::Binary("tmux"), Requirement::Binary("codex")],
    tools = &[],
    unlock = Fix::cli(INSTALL),
    entry_kinds = [
        "Subagent/Receipt",
        "Subagent/Launched",
        "Subagent/Attached",
        "Subagent/Sent",
        "Subagent/Delivered",
        "Subagent/Delivery",
        "Subagent/Message",
        "Subagent/ToolUse",
        "Subagent/ToolResult",
        "Subagent/TurnEnd",
        "Subagent/Error",
        "Subagent/Blocked",
        "Subagent/Result",
        "Subagent/Recovered",
        "Subagent/Note",
        "Subagent/Closed"
    ],
    doc = "Codex as a subagent: the real `codex` TUI in aho's tmux server with your own credentials, observed through its rollout JSONL (byte-offset tail) and mirrored into the parent log; crash-honest receipts."
);
