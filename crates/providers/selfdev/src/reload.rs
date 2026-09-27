//! `aho selfdev reload`: publish → smoke → Activated{pending} → flip
//! `current` + `canary` → handoff → Handoff{ok} or rollback + Handoff{rolled_back}.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aho_log::{EntryKind, LogStore};
use aho_policy::Operator;
use aho_types::{BoxFuture, Principal, Value};
use serde_json::json;

use crate::builds::{Builds, Channel, SelfdevError, Smoke};
use crate::entries::{ACTIVATED, HANDOFF, NS, PUBLISHED, SMOKE};

/// The channels a reload flips; `stable` moves only by `promote`.
pub const RELOAD_CHANNELS: [Channel; 2] = [Channel::Current, Channel::Canary];

fn append(log: &dyn LogStore, by: &Principal, name: &str, body: Value) -> Result<(), SelfdevError> {
    log.append(None, by.clone(), EntryKind::Custom { ns: NS.into(), name: name.into(), body })
        .map(|_| ())
        .map_err(|e| SelfdevError::Log(e.to_string()))
}

/// Runs the whole reload; every phase is appended to `log` by the operator. `handoff` receives
/// `<root>/current/aho` and performs the process handoff (e.g. `daemon::client::reload_and_wait`).
/// Returns the activated hash.
pub async fn reload<F>(
    op: &Operator,
    builds: &Builds,
    log: &dyn LogStore,
    binary: &Path,
    smoke_timeout: Duration,
    handoff: F,
) -> Result<String, SelfdevError>
where
    F: FnOnce(PathBuf) -> BoxFuture<'static, Result<(), String>>,
{
    let by = op.principal();
    let published = builds.publish(binary)?;
    let hash = published.hash.clone();
    append(log, &by, PUBLISHED, json!({"hash": hash, "path": published.path}))?;

    // The smoke boot blocks for up to its timeout; keep it off the executor that serves sessions.
    let smoke = {
        let (b, p) = (builds.clone(), published.clone());
        tokio::task::spawn_blocking(move || b.smoke(&p, smoke_timeout))
            .await
            .unwrap_or_else(|e| Smoke { ok: false, detail: format!("the smoke boot did not finish ({e}); nothing was activated") })
    };
    append(log, &by, SMOKE, json!({"hash": hash, "ok": smoke.ok, "detail": smoke.detail}))?;
    if !smoke.ok {
        return Err(SelfdevError::SmokeFailed { hash, detail: smoke.detail });
    }

    let previous: BTreeMap<Channel, Option<String>> = RELOAD_CHANNELS.iter().map(|c| (*c, builds.channel(*c))).collect();
    append(log, &by, ACTIVATED, json!({"hash": hash, "channels": RELOAD_CHANNELS, "previous": previous, "pending": true}))?;
    let activation = match builds.activate(&hash, &RELOAD_CHANNELS) {
        Ok(a) => a,
        Err(e) => {
            let reason = format!("the channel flip failed: {e}");
            append(log, &by, HANDOFF, json!({"hash": hash, "outcome": "rolled_back", "reason": reason}))?;
            return Err(e);
        }
    };

    match handoff(builds.root().join(Channel::Current.name()).join("aho")).await {
        Ok(()) => {
            append(log, &by, HANDOFF, json!({"hash": hash, "outcome": "ok"}))?;
            Ok(hash)
        }
        Err(reason) => {
            let rollback = builds.rollback(&activation).err().map(|e| e.to_string());
            let mut body = json!({"hash": hash, "outcome": "rolled_back", "reason": reason});
            if let Some(r) = &rollback {
                body["rollback_error"] = Value::String(r.clone());
            }
            append(log, &by, HANDOFF, body)?;
            Err(SelfdevError::HandoffFailed { hash, reason, rollback })
        }
    }
}
