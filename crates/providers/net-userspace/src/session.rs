//! The network as a session capability (07): one userspace network per session, in
//! `<sessions>/<id>.net/`, started only when a sandbox world first attaches (a host-world session
//! never creates it). Every flow decision becomes a `Net/Flow` entry in the session log; a
//! blocked flow also becomes one Notice naming its unlock, so the agent can ask the operator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::{EntryKind, LogStore};
use aho_net::{
    Access, Attachment, Destination, EgressPolicy, FlowEvent, FlowSink, Isolation, NetError, Network, NetworkConfig, NetworkHandle,
    NetworkStatus,
};
use aho_types::{Form, Principal, SessionId, Value};
use serde_json::json;

use crate::{UserspaceNetwork, store};

/// Log namespace of this capability's entries (`Net/Flow`, `Net/Configured`).
pub const NS: &str = "Net";
const BY: &str = "net-userspace";
/// A blocked destination is announced to the agent at most this often.
const NOTICE_EVERY: Duration = Duration::from_secs(60);

/// `net.*` plus the shared `egress.allow` / `egress.deny` lists: the seed of a new session's
/// network. Once created, the network's own persisted settings win (`aho net` changes them).
#[derive(Clone, Debug)]
pub struct NetSettings {
    pub dns_upstream: Vec<std::net::SocketAddr>,
    pub isolation: Isolation,
    pub egress: EgressPolicy,
    pub max_flows: usize,
}

fn access(l: &Loadout, key: &str, default: Access) -> Result<Access, ConfigError> {
    match l.str(key) {
        None => Ok(default),
        Some("allow") => Ok(Access::Allow),
        Some("deny") => Ok(Access::Deny),
        Some(_) => Err(ConfigError::Type { key: key.into(), expected: "allow or deny" }),
    }
}

fn strings(l: &Loadout, key: &str) -> Result<Vec<String>, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or(ConfigError::Type { key: key.into(), expected: "an array of strings" }),
        Some(_) => Err(ConfigError::Type { key: key.into(), expected: "an array of strings" }),
    }
}

impl Config for NetSettings {
    const PREFIX: &'static str = "net";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "net.dns_upstream",
                Ty::Array,
                Some(json!(["1.1.1.1:53", "8.8.8.8:53"])),
                "Recursive resolvers the sandbox network forwards to (`ip:port`); guests cannot choose their own.",
            ),
            ConfigKey::new("net.isolation.internet", Ty::String, Some(json!("allow")), "Sandbox ↔ internet: allow or deny."),
            ConfigKey::new(
                "net.isolation.host_lan",
                Ty::String,
                Some(json!("deny")),
                "Sandbox ↔ the host's LAN and addresses: allow or deny.",
            ),
            ConfigKey::new(
                "net.isolation.other_sessions",
                Ty::String,
                Some(json!("deny")),
                "Sandbox ↔ other sessions' sandboxes: allow or deny.",
            ),
            ConfigKey::new("net.max_flows", Ty::Int, Some(json!(1024)), "Most concurrent flows one sandbox network tracks (1..=16384)."),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let dns_upstream = strings(l, "net.dns_upstream")?
            .iter()
            .map(|s| s.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ConfigError::Type { key: "net.dns_upstream".into(), expected: "an array of `ip:port` resolvers" })?;
        // An entry the network cannot enforce is refused, never skipped: a dropped deny widens
        // access, and a dropped allow can empty the list, which means unrestricted.
        let destinations = |key: &str| {
            strings(l, key)?.iter().map(|d| Destination::parse(d)).collect::<Result<Vec<_>, _>>().map_err(|_| ConfigError::Type {
                key: key.into(),
                expected: "domains, *.domains, IPs or CIDRs (the sandbox network enforces nothing else)",
            })
        };
        let (allow, deny) = (destinations("egress.allow")?, destinations("egress.deny")?);
        let max_flows = l.get("net.max_flows").and_then(Value::as_u64).unwrap_or(1024);
        Ok(NetSettings {
            dns_upstream,
            isolation: Isolation {
                internet: access(l, "net.isolation.internet", Access::Allow)?,
                host_lan: access(l, "net.isolation.host_lan", Access::Deny)?,
                other_sessions: access(l, "net.isolation.other_sessions", Access::Deny)?,
            },
            egress: EgressPolicy { allow, deny },
            max_flows: usize::try_from(max_flows).unwrap_or(usize::MAX),
        })
    }
}

impl NetSettings {
    fn seed(&self, id: &str, k: u8) -> NetworkConfig {
        let mut c = NetworkConfig::with_prefix(id, k);
        if !self.dns_upstream.is_empty() {
            c.dns_upstream = self.dns_upstream.clone();
        }
        c.isolation = self.isolation.clone();
        c.egress = self.egress.clone();
        c.max_flows = self.max_flows;
        c
    }
}

/// Where session `id`'s network lives.
pub fn state_dir(sessions_dir: &Path, id: SessionId) -> PathBuf {
    sessions_dir.join(format!("{:x}.net", id.0))
}

/// Opens (or creates) session `id`'s network now. A new network gets the first subnet no other
/// persisted or running network uses; an existing one keeps its own settings.
pub fn open_session_network(sessions_dir: &Path, id: SessionId, settings: &NetSettings) -> Result<UserspaceNetwork, NetError> {
    let dir = state_dir(sessions_dir, id);
    let name = format!("{:x}", id.0);
    let start = NetworkConfig::new(name.clone()).prefix_index();
    if store::exists(&dir) {
        // The persisted id and prefix win; the seed only has to carry the same id.
        return UserspaceNetwork::open(&dir, settings.seed(&name, start));
    }
    let taken = store::persisted_prefixes(sessions_dir, &dir);
    let mut last = NetError::Unavailable("every sandbox subnet 10.85.1-254.0/24 is in use".into());
    for step in 0..254u16 {
        let k = ((u16::from(start) - 1 + step) % 254 + 1) as u8;
        if taken.contains(&k) {
            continue;
        }
        match UserspaceNetwork::open(&dir, settings.seed(&name, k)) {
            // Another process's live network holds this subnet: try the next one.
            Err(NetError::Invalid(why)) if why.contains("collide") => last = NetError::Invalid(why),
            other => return other,
        }
    }
    Err(last)
}

/// Writes flow decisions into the session log: every one as `Net/Flow`, and a blocked one as a
/// Notice naming its unlock (once per destination per minute).
pub fn log_sink(log: Arc<dyn LogStore>) -> FlowSink {
    let noticed: Mutex<HashMap<String, Instant>> = Mutex::default();
    Arc::new(move |event: FlowEvent| {
        let body = serde_json::to_value(&event).unwrap_or(Value::Null);
        let _ = log.append(None, Principal::system(BY), EntryKind::Custom { ns: NS.into(), name: "Flow".into(), body });
        if event.allowed {
            return;
        }
        let Some(unlock) = event.unlock.clone() else { return };
        let fresh = noticed.lock().is_ok_and(|mut seen| {
            let now = Instant::now();
            seen.retain(|_, at| now.duration_since(*at) < NOTICE_EVERY);
            seen.insert(format!("{}|{unlock}", event.guest), now).is_none()
        });
        if fresh {
            let text = format!(
                "network: {} → {} was blocked ({}). The operator can allow it with `{unlock}`.",
                event.guest, event.destination, event.reason
            );
            let _ = log.append(None, Principal::system(BY), EntryKind::UserMessage { form: Form::Notice, text, client_key: None });
        }
    })
}

/// `Net/Configured`: what an operator's `aho net` applied, by whom, as the whole new config.
pub fn configured_entry(config: &NetworkConfig, change: &str) -> EntryKind {
    EntryKind::Custom { ns: NS.into(), name: "Configured".into(), body: json!({ "change": change, "config": config }) }
}

/// Records configurations applied through the control socket into the owner's session log.
fn configured_sink(log: Arc<dyn LogStore>) -> crate::ConfigSink {
    Arc::new(move |config: &NetworkConfig| {
        let _ = log.append(None, Principal::Operator, configured_entry(config, "aho net"));
    })
}

/// The session's network, opened on first use.
struct Lazy {
    sessions_dir: PathBuf,
    id: SessionId,
    settings: NetSettings,
    log: Option<Arc<dyn LogStore>>,
    net: OnceLock<Result<UserspaceNetwork, String>>,
}

impl Lazy {
    fn net(&self) -> Result<&UserspaceNetwork, NetError> {
        self.net
            .get_or_init(|| {
                let net = open_session_network(&self.sessions_dir, self.id, &self.settings).map_err(|e| e.to_string())?;
                if let Some(log) = &self.log {
                    net.set_flow_sink(Some(log_sink(log.clone()))).map_err(|e| e.to_string())?;
                    net.set_config_sink(Some(configured_sink(log.clone())));
                }
                Ok(net)
            })
            .as_ref()
            .map_err(|e| NetError::Unavailable(format!("the session network could not start: {e}")))
    }
}

impl Network for Lazy {
    fn attach(&self, name: &str) -> Result<Attachment, NetError> {
        self.net()?.attach(name)
    }
    fn status(&self) -> Result<NetworkStatus, NetError> {
        self.net()?.status()
    }
    fn configure(&self, config: NetworkConfig) -> Result<(), NetError> {
        self.net()?.configure(config)
    }
    fn set_flow_sink(&self, sink: Option<FlowSink>) -> Result<(), NetError> {
        self.net()?.set_flow_sink(sink)
    }
}

/// What the network needs from the session: where its logs live, its id, and its log.
pub struct NetDeps {
    pub sessions_dir: Option<PathBuf>,
    pub session: Option<SessionId>,
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for NetDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(NetDeps { sessions_dir: r.service("aho.session_dir"), session: r.service("aho.session_id"), log: r.service("aho.log") })
    }
}

/// `net-userspace`: the `net` seam for sandbox worlds.
pub struct UserspaceNet {
    handle: NetworkHandle,
}

impl Capability for UserspaceNet {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = NetDeps;
    type Config = NetSettings;

    fn build(deps: NetDeps, settings: NetSettings) -> Result<Self, BuildError> {
        let (Some(sessions_dir), Some(id)) = (deps.sessions_dir, deps.session) else {
            return Err(BuildError::Unsupported("the sandbox network needs a session (its id and directory); none was seeded"));
        };
        let lazy = Lazy { sessions_dir, id, settings, log: deps.log, net: OnceLock::new() };
        Ok(UserspaceNet { handle: NetworkHandle(Arc::new(lazy)) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(self.handle.clone());
    }
}

declare_capability!(
    UserspaceNet,
    "net-userspace",
    kind = Kind::Provider,
    provides = [aho_net::SEAM],
    requires = [Requirement::AnyOf(&[Requirement::Platform(Os::Linux), Requirement::Platform(Os::MacOs)])],
    tools = &[],
    provides_handles = [HandleKind::NetAttach],
    enforced_by = crate::ENFORCED_BY,
    entry_kinds = ["Net/Flow", "Net/Configured"],
    doc = "The sandbox's only network (07): userspace switch with DHCP/DHCPv6/RA, `<name>.aho` DNS, NTP, NAT44/NAT66, port maps, isolation and egress policy; starts when a sandbox world attaches."
);
