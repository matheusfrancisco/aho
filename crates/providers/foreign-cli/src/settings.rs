//! Config shared by every foreign-CLI provider, under the provider's own prefix
//! (`subagent.claude.*`, `subagent.codex.*`).

use aho_cap::{ConfigError, cfg_str};
use aho_config::{ConfigKey, Loadout, Ty};
use serde_json::json;

pub const DEFAULT_SOCKET: &str = "aho-subagents";
pub const DEFAULT_POLL_MS: u64 = 250;
pub const DEFAULT_BLOCKED_AFTER_MS: u64 = 20_000;
pub const DEFAULT_INTERRUPT_GRACE_MS: u64 = 5_000;
pub const DEFAULT_MAX_DEPTH: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// tmux binary (name on PATH or a path).
    pub tmux: String,
    /// aho's own tmux server: a socket name (`-L`) or, when it contains `/`, a socket path (`-S`).
    pub socket: String,
    /// Interval between transcript reads.
    pub poll_ms: u64,
    /// A run whose transcript is silent this long while the screen does not change is reported
    /// as blocked on the CLI's own dialog.
    pub blocked_after_ms: u64,
    /// After Escape, how long the CLI has to record the interrupt before aho closes the run.
    pub interrupt_grace_ms: u64,
    pub max_depth: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tmux: "tmux".into(),
            socket: DEFAULT_SOCKET.into(),
            poll_ms: DEFAULT_POLL_MS,
            blocked_after_ms: DEFAULT_BLOCKED_AFTER_MS,
            interrupt_grace_ms: DEFAULT_INTERRUPT_GRACE_MS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

fn uint(l: &Loadout, key: &str, default: u64, min: u64) -> Result<u64, ConfigError> {
    match l.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(v) => v.as_u64().filter(|n| *n >= min).ok_or(ConfigError::Type { key: key.into(), expected: "a positive integer" }),
    }
}

impl Settings {
    /// The keys, spelled under `prefix`; `product` names the CLI in the docs.
    pub fn keys(prefix: &str, product: &str) -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                &format!("{prefix}.tmux"),
                Ty::String,
                Some(json!("tmux")),
                "tmux binary that hosts the CLI (a name on PATH or a path).",
            ),
            ConfigKey::new(
                &format!("{prefix}.tmux_socket"),
                Ty::String,
                Some(json!(DEFAULT_SOCKET)),
                "aho's own tmux server, kept apart from yours: a socket name (`tmux -L <name>`) or a path containing `/` (`tmux -S <path>`); attach with `tmux -L <name> attach -t <session>`.",
            ),
            ConfigKey::new(
                &format!("{prefix}.poll_ms"),
                Ty::Int,
                Some(json!(DEFAULT_POLL_MS)),
                &format!("Interval between reads of {product}'s transcript (ms)."),
            ),
            ConfigKey::new(
                &format!("{prefix}.blocked_after_ms"),
                Ty::Int,
                Some(json!(DEFAULT_BLOCKED_AFTER_MS)),
                &format!(
                    "A run whose transcript stays silent this long while {product}'s screen does not change is returned as blocked on its own dialog, with the screen (ms)."
                ),
            ),
            ConfigKey::new(
                &format!("{prefix}.interrupt_grace_ms"),
                Ty::Int,
                Some(json!(DEFAULT_INTERRUPT_GRACE_MS)),
                &format!("After an interrupt (Escape), how long {product} has to record it before the run is closed as aborted (ms)."),
            ),
            ConfigKey::new(
                &format!("{prefix}.max_depth"),
                Ty::Int,
                Some(json!(DEFAULT_MAX_DEPTH)),
                "Deepest delegation allowed for these children (the root session is 0); a spawn past it is refused.",
            ),
        ]
    }

    pub fn from_loadout(l: &Loadout, prefix: &str) -> Result<Settings, ConfigError> {
        let max_depth = uint(l, &format!("{prefix}.max_depth"), u64::from(DEFAULT_MAX_DEPTH), 0)?;
        Ok(Settings {
            tmux: cfg_str(l, &format!("{prefix}.tmux"), "tmux"),
            socket: cfg_str(l, &format!("{prefix}.tmux_socket"), DEFAULT_SOCKET),
            poll_ms: uint(l, &format!("{prefix}.poll_ms"), DEFAULT_POLL_MS, 1)?,
            blocked_after_ms: uint(l, &format!("{prefix}.blocked_after_ms"), DEFAULT_BLOCKED_AFTER_MS, 1)?,
            interrupt_grace_ms: uint(l, &format!("{prefix}.interrupt_grace_ms"), DEFAULT_INTERRUPT_GRACE_MS, 1)?,
            max_depth: u32::try_from(max_depth)
                .map_err(|_| ConfigError::Type { key: format!("{prefix}.max_depth"), expected: "a small non-negative integer" })?,
        })
    }
}

/// Resolves a binary the way a shell would: a value with a `/` is a path, anything else is
/// looked up on `PATH`.
#[allow(clippy::disallowed_methods, reason = "authority: foreign-cli-binary-discovery")]
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        let p = std::path::PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}
