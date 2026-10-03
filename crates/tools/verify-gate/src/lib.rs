//! `verify-gate`: a turn that changed the workspace does not stop on broken code unchecked.
//!
//! One `HookEvent::TurnStopping` hook. When a turn is about to end `Completed` and it changed
//! the workspace, the project's check runs in the bound world (the world's `Subprocess`, as
//! `/bin/sh -c <command>` in the workspace root) with a timeout. A failure denies the stop with
//! `verify: <command> <outcome>\n<output tail>` (`prompts/failed.md`; the last `TAIL_BYTES` of
//! output), which the session appends as a notice before the model's next step; a pass allows it.
//!
//! What counts as "changed the workspace this turn", read from the session log (`aho.log`):
//! * necessary: a `ToolCall` in this turn whose class is `WorkspaceWrite`, `HostWrite` or
//!   `SpawnsProcess` — the set after which the session itself checkpoints. The class is logged on
//!   every call whether or not checkpoints are on, so no write path is missed (a shell `sed -i`
//!   is a `SpawnsProcess` call, not a `write`).
//! * sufficient when it can be shown: with the world's `Snapshot` and a checkpoint before the
//!   turn (the previous turn's end, no rewind since), the current tree must differ from it. That
//!   drops turns whose process calls changed nothing (`ls`, a language-server query). Without a
//!   baseline (first turn, no snapshots, a rewind) the class signal alone decides.
//!   Per-call `WorkspaceCheckpoint`s are not the signal: they exist only when snapshots and
//!   `checkpoint_per_call` are both on.
//!
//! The command is `verify.command` when set, else detected from the workspace root (`detect`);
//! no command → nothing runs. Config: `verify.enabled` (default true; false registers no hook),
//! `verify.command`, `verify.timeout_s` (default 300; a timeout is a failure).
//!
//! Recursion guard: the session keeps a turn going on a stop denial at most once
//! (`stop_hook_active` in the payload). The stop after that denial is not verified again — the
//! hook returns no decision — so a still-failing check never loops the turn. Every run is logged
//! by the session as `HookInvoked` + `HookResult` under `system:verify-gate`: decision `Allow`
//! (checked, passed), `Deny(<reason>)` (checked, failed) or `none` (not checked).

#![forbid(unsafe_code)]

mod detect;
mod run;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_fs::{FsRead, SnapshotHandle};
use aho_log::{Entry, EntryKind, LogStore};
use aho_proc::SubprocessHandle;
use aho_types::{Consequence, Hash};
use serde_json::{Value, json};

pub use detect::{Check, detect};
pub use run::{Outcome, Ran, TAIL_BYTES, run};

/// The session's own log, seeded by `aho-session`.
pub const LOG_SERVICE: &str = "aho.log";
pub const ENABLED_KEY: &str = "verify.enabled";
pub const COMMAND_KEY: &str = "verify.command";
pub const TIMEOUT_KEY: &str = "verify.timeout_s";
pub const DEFAULT_TIMEOUT_S: u64 = 300;

/// Call classes after which the workspace may differ (the session's checkpoint-per-call set).
const MAY_CHANGE: &[Consequence] = &[Consequence::WorkspaceWrite, Consequence::HostWrite, Consequence::SpawnsProcess];
const ON_STOP: &[HookEvent] = &[HookEvent::TurnStopping];

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyConfig {
    pub enabled: bool,
    pub command: Option<String>,
    pub timeout: Duration,
}

impl Config for VerifyConfig {
    const PREFIX: &'static str = "verify";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                ENABLED_KEY,
                Ty::Bool,
                Some(json!(true)),
                "Run the project's check before a turn that changed the workspace may stop; a failure keeps the turn going once.",
            ),
            ConfigKey::new(
                COMMAND_KEY,
                Ty::String,
                None,
                "Check command (run with /bin/sh -c in the workspace root). Default: detected from Cargo.toml, package.json, pyproject.toml/pytest.ini, go.mod.",
            ),
            ConfigKey::new(
                TIMEOUT_KEY,
                Ty::Int,
                Some(json!(DEFAULT_TIMEOUT_S)),
                "Seconds the check may run; past it the check counts as failed.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let enabled = match l.get(ENABLED_KEY) {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(ConfigError::Type { key: ENABLED_KEY.into(), expected: "true or false" }),
        };
        let command = match l.get(COMMAND_KEY) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Some(_) => return Err(ConfigError::Type { key: COMMAND_KEY.into(), expected: "a shell command string" }),
        };
        let timeout_s = match l.get(TIMEOUT_KEY) {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
            Some(v) => {
                v.as_u64().filter(|n| *n > 0).ok_or(ConfigError::Type { key: TIMEOUT_KEY.into(), expected: "a positive integer" })?
            }
        };
        Ok(VerifyConfig { enabled, command, timeout: Duration::from_secs(timeout_s) })
    }
}

pub struct VerifyDeps {
    pub proc: SubprocessHandle,
    pub fs: FsRead,
    /// Optional: sharpens "changed" to "the tree differs from the turn's start".
    pub snapshot: Option<SnapshotHandle>,
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for VerifyDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::FsRead, HandleKind::Snapshot];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(VerifyDeps { proc: r.handle()?, fs: r.handle()?, snapshot: r.handle::<SnapshotHandle>().ok(), log: r.service(LOG_SERVICE) })
    }
}

struct Gate {
    proc: SubprocessHandle,
    fs: FsRead,
    snapshot: Option<SnapshotHandle>,
    log: Arc<dyn LogStore>,
    command: Option<String>,
    timeout: Duration,
}

pub struct VerifyGate {
    /// `None` when `verify.enabled = false`: nothing is registered.
    gate: Option<Arc<Gate>>,
}

impl Gate {
    async fn on_stop(&self, payload: &Value) -> HookOutput {
        // A denial is honored once per turn; the stop after it is not re-verified (crate doc).
        if payload["transition"] != "Completed" || payload["stop_hook_active"] == true {
            return HookOutput::default();
        }
        let Some(turn) = payload["turn"].as_u64() else {
            return HookOutput::default();
        };
        if !self.changed(turn).await {
            return HookOutput::default();
        }
        let check = match &self.command {
            Some(c) => Check::new(c.clone()),
            None => match detect(self.fs.0.as_ref()).await {
                Some(c) => c,
                None => return HookOutput::default(),
            },
        };
        let ran = run(self.proc.0.as_ref(), &check, self.timeout).await;
        let decision = if ran.passed(&check) { Decision::Allow } else { Decision::Deny(ran.reason(&check)) };
        HookOutput { decision: Some(decision), ..HookOutput::default() }
    }

    async fn changed(&self, turn: u64) -> bool {
        let Some(head) = self.log.head() else {
            return false;
        };
        let Ok(entries) = self.log.read_branch(head) else {
            // Unreadable log: check rather than let a possibly broken change through.
            return true;
        };
        let start = entries.iter().rposition(|e| matches!(e.kind, EntryKind::TurnStart { turn: t } if t.0 == turn)).unwrap_or(0);
        let wrote = entries[start..].iter().any(|e| matches!(&e.kind, EntryKind::ToolCall { class, .. } if MAY_CHANGE.contains(class)));
        if !wrote {
            return false;
        }
        let (Some(snap), Some(base)) = (&self.snapshot, baseline(&entries[..start])) else {
            return true;
        };
        match snap.0.tree().await {
            Ok(now) => now != base,
            Err(_) => true,
        }
    }
}

/// The workspace tree when the turn started: the newest checkpoint before it, unless a rewind
/// came after that checkpoint.
fn baseline(before: &[Entry]) -> Option<Hash> {
    before.iter().rev().find_map(|e| match &e.kind {
        EntryKind::WorkspaceCheckpoint { tree, .. } => Some(Some(*tree)),
        EntryKind::WorkspaceRestored { .. } => Some(None),
        _ => None,
    })?
}

impl Capability for VerifyGate {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VerifyDeps;
    type Config = VerifyConfig;

    fn build(deps: VerifyDeps, cfg: VerifyConfig) -> Result<Self, BuildError> {
        if !cfg.enabled {
            return Ok(VerifyGate { gate: None });
        }
        let log = deps.log.ok_or(BuildError::Unsupported("the session did not seed `aho.log`, so a turn's changes cannot be seen"))?;
        Ok(VerifyGate {
            gate: Some(Arc::new(Gate {
                proc: deps.proc,
                fs: deps.fs,
                snapshot: deps.snapshot,
                log,
                command: cfg.command,
                timeout: cfg.timeout,
            })),
        })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let Some(gate) = self.gate.clone() else {
            return;
        };
        let timeout_ms = u64::try_from((gate.timeout + run::OVERHEAD).as_millis()).unwrap_or(u64::MAX);
        r.hook(HookSpec {
            on: ON_STOP,
            if_class: None,
            if_tool: None,
            timeout_ms,
            once: false,
            on_error: OnError::Deny,
            run: Arc::new(move |_, payload: &Value| {
                let gate = gate.clone();
                let payload = payload.clone();
                Box::pin(async move { gate.on_stop(&payload).await })
            }),
        });
    }
}

declare_capability!(
    VerifyGate,
    "verify-gate",
    kind = Kind::Hook,
    provides = [SeamId("hooks")],
    requires = [Requirement::Seam(aho_proc::SEAM), Requirement::Seam(aho_fs::SEAM)],
    tools = &[],
    doc = "Before a turn that changed the workspace stops, runs the project's check (verify.command or detected) in the bound world; a failure keeps the turn going once with the output tail."
);
