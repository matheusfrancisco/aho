//! Anonymous usage telemetry, derived from the session log by `aho_inspect::events`: a closed
//! vocabulary where every value is a unit enum token, so a path, prompt or third-party tool
//! name is unrepresentable on the wire, not filtered.
//!
//! * Opt-in twice: the capability ships disabled (`aho enable telemetry`) and stays Locked
//!   until `telemetry.endpoint` is set. A non-empty `DO_NOT_TRACK` wins over both.
//! * Flushes at turn end and session end: one bounded POST, one attempt, never a retry, never
//!   holding the turn open. Success clears the queue; any failure drops the batch.
//! * A 4xx answer (except 408/429) writes `<aho.home>/telemetry.latch` with the endpoint, and
//!   a matching latch means nothing is sent until the endpoint changes.
//! * The wire payload is exactly [`payload`]: what `aho telemetry preview` prints is what
//!   would be sent, byte for byte.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_egress::{EgressHandle, HostList, HttpRequest, Limits, Method};
use aho_inspect::{Envelope, Event};
use aho_log::LogStore;
use aho_types::{EntryId, Timestamp, Value};
use serde_json::json;

/// `<aho.home>/telemetry.latch`: holds the endpoint that refused a batch.
pub const LATCH_FILE: &str = "telemetry.latch";

/// Bounds for one flush; telemetry never follows redirects and never waits long.
const FLUSH_LIMITS: Limits = Limits { max_body_bytes: 16 * 1024, timeout_ms: 3_000, max_redirects: 0 };

/// The wire payload and the `aho telemetry preview` output: the same JSON.
pub fn payload(envelope: &Envelope, events: &[Event]) -> serde_json::Value {
    json!({ "envelope": envelope, "events": events })
}

// ───────────────────────────── config & deps ─────────────────────────────

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    /// `None` until the operator sets `telemetry.endpoint`; the plan shows the capability Locked.
    pub endpoint: Option<String>,
    /// Most events held between flushes; the oldest are dropped beyond it.
    pub queue_max: usize,
    /// The operator's egress allow/deny list; `egress.deny` wins here like everywhere else.
    pub hosts: HostList,
}

impl Config for TelemetryConfig {
    const PREFIX: &'static str = "telemetry";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = aho_egress::config_keys();
        keys.extend([
            ConfigKey::new("telemetry.endpoint", Ty::String, None, "Where telemetry events go. Nothing is sent until it is set."),
            ConfigKey::new(
                "telemetry.queue_max",
                Ty::Int,
                Some(json!(256)),
                "Most events held between flushes; the oldest are dropped beyond it.",
            ),
        ]);
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let endpoint = match l.get("telemetry.endpoint") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(s.trim().to_string()),
            Some(_) => return Err(ConfigError::Type { key: "telemetry.endpoint".into(), expected: "a URL string" }),
        };
        let queue_max = match l.get("telemetry.queue_max") {
            None | Some(Value::Null) => 256,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::Type { key: "telemetry.queue_max".into(), expected: "a positive integer" })?
                as usize,
        };
        Ok(TelemetryConfig { endpoint, queue_max, hosts: HostList::from_loadout(l)? })
    }
}

pub struct TelemetryDeps {
    pub egress: EgressHandle,
    /// The session's own log (`aho.log`), seeded by `aho-session`.
    pub log: Option<Arc<dyn LogStore>>,
    /// The aho home directory (`aho.home`), where the latch file lives.
    pub home: Option<String>,
}

impl Deps for TelemetryDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Egress(Route::Sandbox)];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(TelemetryDeps { egress: r.handle()?, log: r.service::<Arc<dyn LogStore>>("aho.log"), home: r.service::<String>("aho.home") })
    }
}

// ───────────────────────────── authority ─────────────────────────────

/// The environment facts telemetry may consult; tests inject their own.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvProbe {
    pub do_not_track: bool,
    pub is_ci: bool,
    pub ran_from_cargo: bool,
}

impl EnvProbe {
    /// The only environment read: `DO_NOT_TRACK`, `CI` and `CARGO`, each as "set and non-empty".
    #[allow(clippy::disallowed_methods, reason = "authority: telemetry-env")]
    fn from_process() -> EnvProbe {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        EnvProbe { do_not_track: set("DO_NOT_TRACK"), is_ci: set("CI"), ran_from_cargo: set("CARGO") }
    }
}

/// The only filesystem access: `<aho.home>/telemetry.latch`. With `write = None` it returns
/// the latched endpoint, if any; with `Some(endpoint)` it records one and echoes it back.
/// A missing or unwritable latch is not an error: sending just stops for this session only.
#[allow(clippy::disallowed_methods, reason = "authority: telemetry-latch")]
fn latch_file(path: &Path, write: Option<&str>) -> Option<String> {
    match write {
        Some(endpoint) => std::fs::write(path, endpoint).ok().map(|()| endpoint.to_string()),
        None => std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()),
    }
}

// ───────────────────────────── capability ─────────────────────────────

#[derive(Default)]
struct FlushState {
    /// Newest entry id already turned into events; the next flush slices after it.
    last: Option<EntryId>,
    queue: VecDeque<Event>,
    latched: bool,
}

struct Inner {
    egress: EgressHandle,
    log: Arc<dyn LogStore>,
    endpoint: String,
    queue_max: usize,
    hosts: HostList,
    latch: PathBuf,
    is_ci: bool,
    ran_from_cargo: bool,
    state: Mutex<FlushState>,
}

#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Telemetry {
    /// [`Capability::build`] with an injected probe, so tests never touch the process environment.
    pub fn with_probe(d: TelemetryDeps, cfg: TelemetryConfig, probe: EnvProbe) -> Result<Telemetry, BuildError> {
        if probe.do_not_track {
            return Err(BuildError::Unsupported("DO_NOT_TRACK is set, so telemetry stays off. Unset it to enable."));
        }
        let endpoint = cfg.endpoint.ok_or(BuildError::Config(ConfigError::MissingKey("telemetry.endpoint".into())))?;
        let log = d.log.ok_or(BuildError::Unsupported("the session did not seed `aho.log`, so there is nothing to derive events from"))?;
        let home =
            d.home.ok_or(BuildError::Unsupported("the session did not seed `aho.home`, so the telemetry latch has nowhere to live"))?;
        let latch = Path::new(&home).join(LATCH_FILE);
        let latched = latch_file(&latch, None).is_some_and(|prev| prev == endpoint);
        Ok(Telemetry {
            inner: Arc::new(Inner {
                egress: d.egress,
                log,
                endpoint,
                queue_max: cfg.queue_max.max(1),
                hosts: cfg.hosts,
                latch,
                is_ci: probe.is_ci,
                ran_from_cargo: probe.ran_from_cargo,
                state: Mutex::new(FlushState { latched, ..FlushState::default() }),
            }),
        })
    }

    /// True when the endpoint refused a batch (or a matching latch predates this session).
    pub fn latched(&self) -> bool {
        lk(&self.inner.state).latched
    }

    /// Derives fresh events, queues them (bounded), and makes exactly one send attempt.
    /// Every attempt drops its batch: telemetry never retries and never blocks a turn.
    pub async fn flush(&self, session_end: bool) {
        let Some(batch) = self.collect(session_end) else { return };
        let envelope = aho_inspect::envelope(Timestamp::now(), self.inner.is_ci, self.inner.ran_from_cargo);
        let Ok(body) = serde_json::to_vec(&payload(&envelope, &batch)) else { return };
        let req = HttpRequest {
            method: Method::Post,
            url: self.inner.endpoint.clone(),
            headers: Vec::new(),
            body: Some(body),
            limits: FLUSH_LIMITS,
            hosts: self.inner.hosts.clone(),
        }
        .header("content-type", "application/json");
        match self.inner.egress.0.fetch(req).await {
            Ok(resp) if resp.is_success() => {}
            Ok(resp) if (400..500).contains(&resp.status) && resp.status != 408 && resp.status != 429 => self.latch_now(),
            _ => {}
        }
    }

    /// The endpoint said "never" (a 4xx that is not a timeout or rate limit): record the latch
    /// and stop sending until the operator points telemetry somewhere else.
    fn latch_now(&self) {
        let _written = latch_file(&self.inner.latch, Some(&self.inner.endpoint));
        lk(&self.inner.state).latched = true;
    }

    /// Slices the branch after the last exported entry, derives events, and drains the bounded
    /// queue into one batch. `None` means there is nothing to send.
    fn collect(&self, session_end: bool) -> Option<Vec<Event>> {
        let inner = &*self.inner;
        let mut st = lk(&inner.state);
        if st.latched {
            return None;
        }
        let branch = inner.log.head().and_then(|h| inner.log.read_branch(h).ok()).unwrap_or_default();
        let start = st.last.map_or(0, |last| branch.partition_point(|e| e.id <= last));
        let mut fresh = aho_inspect::events(&branch[start..]);
        if session_end {
            fresh.push(aho_inspect::session_snapshot(&branch));
        }
        st.last = branch.last().map(|e| e.id).or(st.last);
        for ev in fresh {
            if st.queue.len() == inner.queue_max {
                st.queue.pop_front();
            }
            st.queue.push_back(ev);
        }
        if st.queue.is_empty() {
            return None;
        }
        Some(st.queue.drain(..).collect())
    }
}

impl Capability for Telemetry {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = TelemetryDeps;
    type Config = TelemetryConfig;

    fn build(d: TelemetryDeps, cfg: TelemetryConfig) -> Result<Self, BuildError> {
        Telemetry::with_probe(d, cfg, EnvProbe::from_process())
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let me = self.clone();
        r.hook(HookSpec {
            on: &[HookEvent::TurnEnd, HookEvent::SessionEnd],
            if_class: None,
            if_tool: None,
            timeout_ms: 5_000,
            once: false,
            on_error: OnError::Allow,
            run: Arc::new(move |ev, _| {
                let me = me.clone();
                Box::pin(async move {
                    me.flush(ev == HookEvent::SessionEnd).await;
                    HookOutput::default()
                })
            }),
        });
    }
}

declare_capability!(
    Telemetry,
    "telemetry",
    kind = Kind::Provider,
    provides = [],
    requires = [Requirement::Seam(aho_egress::SEAM), Requirement::Setting { key: "telemetry.endpoint", pred: Pred::Set }],
    tools = &[],
    unlock = Fix::cli("aho set telemetry.endpoint <url>"),
    doc = "Anonymous usage events derived from the session log: a closed vocabulary (no paths, prompts or tool names), opt-in, DO_NOT_TRACK honored; flushed at turn end to telemetry.endpoint within the operator's egress list."
);
