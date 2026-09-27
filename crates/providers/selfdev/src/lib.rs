//! Self-dev reload and releases: aho rebuilding and reloading itself.
//!
//! publish an immutable build → smoke-boot it → `Selfdev/Activated{pending}` → flip the
//! `current`/`canary` symlinks → the caller's handoff reloads the daemon → `Selfdev/Handoff{ok}`,
//! or roll both symlinks back and log `Handoff{rolled_back}`. Every phase is a log entry and the
//! rollback target is a fold over them.

#![forbid(unsafe_code)]

mod builds;
mod entries;
mod reload;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_types::Value;
use serde_json::json;

pub use builds::{Activation, Builds, Channel, DETAIL_MAX, Published, SelfdevError, Smoke};
pub use entries::{ACTIVATED, HANDOFF, NS, PUBLISHED, SMOKE, fold};
pub use reload::{RELOAD_CHANNELS, reload};

pub const DEFAULT_BUILDS_DIR: &str = "~/.aho/builds";
pub const DEFAULT_SMOKE_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfdevConfig {
    /// As configured; `~` is expanded from `HOME` at build time.
    pub builds_dir: String,
    pub smoke_timeout: Duration,
}

impl Config for SelfdevConfig {
    const PREFIX: &'static str = "selfdev";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "selfdev.builds_dir",
                Ty::String,
                Some(json!(DEFAULT_BUILDS_DIR)),
                "Where published builds (`versions/<hash>/aho`) and the `current`/`stable`/`canary` links live; `~` is your HOME.",
            ),
            ConfigKey::new(
                "selfdev.smoke_timeout_ms",
                Ty::Int,
                Some(json!(DEFAULT_SMOKE_TIMEOUT_MS)),
                "How long a new build's smoke boot (`aho plan --json` in a scratch project) may take before it is killed and the reload stops.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let builds_dir = match l.get("selfdev.builds_dir") {
            None | Some(Value::Null) => DEFAULT_BUILDS_DIR.to_string(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(_) => return Err(ConfigError::Type { key: "selfdev.builds_dir".into(), expected: "a non-empty path" }),
        };
        let ms = match l.get("selfdev.smoke_timeout_ms") {
            None | Some(Value::Null) => DEFAULT_SMOKE_TIMEOUT_MS,
            Some(v) => v
                .as_u64()
                .filter(|ms| *ms > 0)
                .ok_or(ConfigError::Type { key: "selfdev.smoke_timeout_ms".into(), expected: "a positive number of milliseconds" })?,
        };
        Ok(SelfdevConfig { builds_dir, smoke_timeout: Duration::from_millis(ms) })
    }
}

/// `~` / `~/…` → `$HOME/…`; other paths unchanged.
pub fn expand_home(path: &str, home: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let rest = match path.strip_prefix('~') {
        None => return Ok(PathBuf::from(path)),
        Some("") => "",
        Some(r) if r.starts_with('/') => r.trim_start_matches('/'),
        Some(_) => return Ok(PathBuf::from(path)),
    };
    match home.filter(|h| !h.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(rest)),
        None => Err(format!(
            "`selfdev.builds_dir` is `{path}` but HOME is unset, so `~` has no meaning; we did not start self-dev. Set an absolute path with `aho set selfdev.builds_dir <dir>`."
        )),
    }
}

/// The service the `aho selfdev` command looks up (`Registry::service("selfdev")`).
#[derive(Clone, Debug)]
pub struct SelfdevService {
    pub builds: Arc<Builds>,
    pub smoke_timeout: Duration,
}

pub struct Selfdev {
    builds: Arc<Builds>,
    smoke_timeout: Duration,
}

impl Capability for Selfdev {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = SelfdevConfig;

    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-home")]
    fn build(_: (), cfg: SelfdevConfig) -> Result<Self, BuildError> {
        let root = expand_home(&cfg.builds_dir, std::env::var_os("HOME").as_deref()).map_err(BuildError::Probe)?;
        Ok(Selfdev { builds: Arc::new(Builds::new(root)), smoke_timeout: cfg.smoke_timeout })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold);
        r.service("selfdev", SelfdevService { builds: self.builds.clone(), smoke_timeout: self.smoke_timeout });
    }
}

declare_capability!(
    Selfdev,
    "selfdev",
    kind = Kind::Provider,
    provides = [SeamId("selfdev")],
    requires = [Requirement::Cap("world-host")],
    tools = &[],
    entry_kinds = ["Selfdev/Published", "Selfdev/Smoke", "Selfdev/Activated", "Selfdev/Handoff"],
    doc = "Self-dev reload: publishes immutable builds, smoke-boots them, flips the current/canary links and rolls back from the log. Host world only: publishing writes host paths outside the workspace. Not an agent tool: HostWrite needs a host handle, so it cannot be a Kind::Tool. The operator runs `aho selfdev reload`; an agent asks through `unlock_request` (tool-inspect), which raises an approval card, and only the operator's command performs the reload."
);
