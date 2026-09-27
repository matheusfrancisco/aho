//! Claude Code as a subagent provider: observe, don't wrap.
//!
//! Each child is the real `claude` binary, untouched and signed in with the user's own
//! credentials, running interactively in a session of aho's own tmux server (see
//! [`foreign_cli`]) with the child's workspace as its working directory:
//!
//! ```text
//! claude --session-id <uuid> --permission-mode manual|plan [--model M] [--append-system-prompt P] -- <task>
//! ```
//!
//! * aho picks the session id, so the transcript is
//!   `<config dir>/projects/<cwd with every non-alphanumeric byte as '-'>/<uuid>.jsonl`
//!   (`<config dir>` = `subagent.claude.config_dir`, else `$CLAUDE_CONFIG_DIR`, else
//!   `~/.claude`), found by name across `projects/*` when the directory name differs.
//! * The transcript is tailed by byte offset and normalized by [`normalize`]; the interactive
//!   CLI ends every turn with a `system/turn_duration` record.
//! * `--permission-mode plan` (read-only) when the child's policy lacks `WorkspaceWrite`,
//!   otherwise `manual` (Claude Code asks before acting; a question it asks freezes the
//!   screen and comes back to the parent as a blocked run it can answer).
//!
//! Features: persona (`--append-system-prompt`) and model; no fork, no tool filter.

#![forbid(unsafe_code)]

pub mod normalize;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::LogStore;
use aho_subagent::SubagentProvider;
use aho_types::Hash;
use foreign_cli::{Cli, ForeignProvider, LOG_SERVICE, Launch, LaunchCtx, LocateCtx, Record, Settings, which};
use serde_json::json;

/// Capability id; also `Subagent/Started.provider`.
pub const PROVIDER: &str = "subagent-claude";
pub const PREFIX: &str = "subagent.claude";
pub const INSTALL: &str = "brew install tmux && curl -fsSL https://claude.ai/install.sh | bash";

/// Claude Code's project directory name for a working directory.
pub fn escape_cwd(cwd: &Path) -> String {
    cwd.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// A version-4 UUID derived from `seed` (Claude Code requires `--session-id` to be a UUID).
pub fn uuid_from(seed: &Hash) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&seed.0[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Launch and transcript settings for Claude Code.
#[derive(Clone, Debug)]
pub struct ClaudeCode {
    /// `claude` on PATH, or a path.
    pub binary: String,
    /// Directory holding Claude Code's `projects/`.
    pub config_dir: PathBuf,
    /// Passed to the child as `CLAUDE_CONFIG_DIR` (set when configured or inherited).
    pub export_config_dir: bool,
}

impl ClaudeCode {
    /// The transcript for a session id in a working directory.
    pub fn transcript_path(&self, cwd: &Path, session: &str) -> PathBuf {
        self.config_dir.join("projects").join(escape_cwd(cwd)).join(format!("{session}.jsonl"))
    }
}

impl Cli for ClaudeCode {
    const PROVIDER: &'static str = PROVIDER;
    const PRODUCT: &'static str = "Claude Code";
    const SHORT: &'static str = "claude";
    const PERSONA: bool = true;

    fn launch(&self, ctx: &LaunchCtx<'_>) -> Result<Launch, String> {
        let bin = which(&self.binary).ok_or_else(|| {
            format!(
                "Claude Code (`{}`) is not on PATH, so nothing was started. Install it (`{INSTALL}`) or `aho set {PREFIX}.binary /path/to/claude`.",
                self.binary
            )
        })?;
        let session =
            uuid_from(&Hash::of_json(&(PROVIDER, ctx.child.0.to_string(), ctx.cwd.display().to_string(), aho_types::Timestamp::now().0)));
        let mut argv = vec![bin.display().to_string(), "--session-id".into(), session.clone()];
        argv.extend(["--permission-mode".to_string(), if ctx.writable { "manual" } else { "plan" }.to_string()]);
        if let Some(m) = ctx.model {
            argv.extend(["--model".to_string(), m.to_string()]);
        }
        if let Some(p) = ctx.persona {
            argv.extend(["--append-system-prompt".to_string(), p.to_string()]);
        }
        argv.extend(["--".to_string(), ctx.prompt.to_string()]);
        let env =
            if self.export_config_dir { vec![("CLAUDE_CONFIG_DIR".to_string(), self.config_dir.display().to_string())] } else { vec![] };
        Ok(Launch { argv, env, cli_session: Some(session) })
    }

    #[allow(clippy::disallowed_methods, reason = "authority: subagent-claude-locate")]
    fn locate(&self, ctx: &LocateCtx<'_>) -> Option<PathBuf> {
        let session = ctx.cli_session?;
        let direct = self.transcript_path(ctx.cwd, session);
        if direct.is_file() {
            return Some(direct);
        }
        // Claude Code shortens very long directory names; the file name is the session id either way.
        let name = format!("{session}.jsonl");
        std::fs::read_dir(self.config_dir.join("projects")).ok()?.flatten().map(|d| d.path().join(&name)).find(|p| p.is_file())
    }

    fn normalize(&self, line: &[u8]) -> Record {
        normalize::normalize(line)
    }
}

// ───────────────────────────── capability ─────────────────────────────

pub struct ClaudeDeps {
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for ClaudeDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ClaudeDeps { log: r.service(LOG_SERVICE) })
    }
}

#[derive(Clone, Debug)]
pub struct ClaudeConfig {
    pub cli: ClaudeCode,
    pub settings: Settings,
}

impl Config for ClaudeConfig {
    const PREFIX: &'static str = PREFIX;
    fn keys() -> Vec<ConfigKey> {
        let mut keys = vec![
            ConfigKey::new(
                "subagent.claude.binary",
                Ty::String,
                Some(json!("claude")),
                "The Claude Code binary (a name on PATH or a path).",
            ),
            ConfigKey::new(
                "subagent.claude.config_dir",
                Ty::String,
                None,
                "Claude Code's config directory (holds projects/<cwd>/<session>.jsonl); default: $CLAUDE_CONFIG_DIR, else ~/.claude. If set, it is also passed to the child as CLAUDE_CONFIG_DIR.",
            ),
        ];
        keys.extend(Settings::keys(PREFIX, "Claude Code"));
        keys
    }
    #[allow(clippy::disallowed_methods, reason = "authority: subagent-claude-config")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let configured = l.str("subagent.claude.config_dir").filter(|s| !s.is_empty()).map(PathBuf::from);
        let inherited = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()).map(PathBuf::from);
        let export = configured.is_some() || inherited.is_some();
        let config_dir = match configured.or(inherited) {
            Some(d) => d,
            None => std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".claude"))
                .ok_or_else(|| ConfigError::MissingKey("subagent.claude.config_dir".into()))?,
        };
        Ok(ClaudeConfig {
            cli: ClaudeCode { binary: cfg_str(l, "subagent.claude.binary", "claude"), config_dir, export_config_dir: export },
            settings: Settings::from_loadout(l, PREFIX)?,
        })
    }
}

/// The capability: registers the provider as the `subagent` service (and concretely as
/// `subagent-claude`), plus `SessionStart`/`SessionEnd` hooks that resume and close children.
pub struct SubagentClaude {
    provider: ForeignProvider<ClaudeCode>,
}

impl SubagentClaude {
    pub fn provider(&self) -> &ForeignProvider<ClaudeCode> {
        &self.provider
    }
}

impl Capability for SubagentClaude {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ClaudeDeps;
    type Config = ClaudeConfig;

    fn build(deps: ClaudeDeps, cfg: ClaudeConfig) -> Result<Self, BuildError> {
        let log = deps
            .log
            .ok_or(BuildError::Unsupported("the session did not seed `aho.log`, which subagent-claude mirrors its children into"))?;
        let provider = ForeignProvider::new(cfg.cli, cfg.settings, log).map_err(BuildError::Probe)?;
        Ok(SubagentClaude { provider })
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
    SubagentClaude,
    "subagent-claude",
    kind = Kind::Provider,
    provides = [aho_subagent::SEAM],
    requires = [Requirement::Binary("tmux"), Requirement::Binary("claude")],
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
    doc = "Claude Code as a subagent: the real `claude` in aho's tmux server with your own credentials, observed through its JSONL transcript (byte-offset tail) and mirrored into the parent log; crash-honest receipts."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_matches_recorded_project_directories() {
        // Pairs recorded on this machine: `cwd` field → directory under ~/.claude/projects.
        assert_eq!(escape_cwd(Path::new("/Users/me/m/projects/ai/tabsearch")), "-Users-me-m-projects-ai-tabsearch");
        assert_eq!(escape_cwd(Path::new("/Users/me/p/recordings-export 2")), "-Users-me-p-recordings-export-2");
        assert_eq!(escape_cwd(Path::new("/Users/me/codes/placenta_synth")), "-Users-me-codes-placenta-synth");
        assert_eq!(escape_cwd(Path::new("/Users/me/playground.zig/ziglings")), "-Users-me-playground-zig-ziglings");
    }

    #[test]
    fn session_ids_are_version_4_uuids() {
        let u = uuid_from(&Hash::of(b"x"));
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }
}
