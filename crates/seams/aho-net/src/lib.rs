//! The sandbox's only NIC: bounded QEMU socket-protocol Ethernet frames.
#![forbid(unsafe_code)]

pub use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};
pub const SEAM: aho_cap::SeamId = aho_cap::SeamId("net");

pub const MAX_FRAME: usize = 1518;
pub const GATEWAY_MAC: [u8; 6] = [2, 0x61, 0x68, 0x6f, 0, 1];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Nic {
    pub name: String,
    pub mac: [u8; 6],
    pub ipv4: Ipv4Addr,
    pub ipv6: Ipv6Addr,
    pub gateway_v4: Ipv4Addr,
    pub gateway_v6: Ipv6Addr,
    pub prefix_v4: u8,
    pub prefix_v6: u8,
}

#[cfg(unix)]
pub struct Attachment {
    /// u32 big-endian length followed by exactly that many Ethernet bytes, both directions.
    pub stream: std::os::unix::net::UnixStream,
    pub nic: Nic,
}
#[cfg(not(unix))]
pub struct Attachment {
    pub nic: Nic,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    Allow,
    #[default]
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Isolation {
    pub internet: Access,
    pub host_lan: Access,
    pub other_sessions: Access,
}
impl Default for Isolation {
    fn default() -> Self {
        Self { internet: Access::Allow, host_lan: Access::Deny, other_sessions: Access::Deny }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum Destination {
    Cidr(IpNet),
    Domain(String),
}
impl Destination {
    pub fn parse(value: &str) -> Result<Self, NetError> {
        if let Ok(net) = value.parse() {
            return Ok(Self::Cidr(net));
        }
        if let Ok(ip) = value.parse::<IpAddr>() {
            return Ok(Self::Cidr(IpNet::from(ip)));
        }
        let name = value.trim_end_matches('.').to_ascii_lowercase();
        let base = name.strip_prefix("*.").unwrap_or(&name);
        if base.len() > 253
            || base.is_empty()
            || base.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err(NetError::Invalid(format!("invalid destination {value:?}")));
        }
        Ok(Self::Domain(name))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Empty means unrestricted within isolation; otherwise at least one allow must match.
    pub allow: Vec<Destination>,
    /// Deny always wins, including a CIDR deny after domain resolution.
    pub deny: Vec<Destination>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortMap {
    pub protocol: Transport,
    /// Explicit bind address, never implicitly wildcard.
    pub host: SocketAddr,
    pub guest: String,
    pub guest_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    pub id: String,
    pub ipv4: Ipv4Net,
    pub ipv6: Ipv6Net,
    pub isolation: Isolation,
    pub egress: EgressPolicy,
    /// Trusted operator-selected recursive resolvers; guests cannot override them.
    pub dns_upstream: Vec<SocketAddr>,
    pub port_maps: Vec<PortMap>,
    pub max_flows: usize,
}
impl NetworkConfig {
    /// Defaults with a prefix derived from `id` (`with_prefix` picks one explicitly).
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        let hash = id.bytes().fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16777619));
        Self::with_prefix(id, (hash % 254 + 1) as u8)
    }

    /// Defaults on `10.85.<k>.0/24` and `fd85:0:<k>::/64`; `k` in 1..=254 names one network's
    /// subnet, so an allocator can hand distinct sessions distinct `k`.
    pub fn with_prefix(id: impl Into<String>, k: u8) -> Self {
        let k = k.clamp(1, 254);
        Self {
            id: id.into(),
            // Both prefixes are constant and valid; the defaults are unreachable fallbacks.
            ipv4: Ipv4Net::new(Ipv4Addr::new(10, 85, k, 0), 24).unwrap_or_default(),
            ipv6: Ipv6Net::new(Ipv6Addr::new(0xfd85, 0, u16::from(k), 0, 0, 0, 0, 0), 64).unwrap_or_default(),
            isolation: Isolation::default(),
            egress: EgressPolicy::default(),
            dns_upstream: vec![SocketAddr::from(([1, 1, 1, 1], 53)), SocketAddr::from(([8, 8, 8, 8], 53))],
            port_maps: vec![],
            max_flows: 1024,
        }
    }

    /// The `k` of a `with_prefix` network (the third IPv4 octet).
    pub fn prefix_index(&self) -> u8 {
        self.ipv4.addr().octets()[2]
    }
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self::new("default")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowEvent {
    pub network: String,
    pub guest: String,
    pub protocol: Transport,
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub allowed: bool,
    pub reason: String,
    /// Operator-facing Notice hint for a blocked flow.
    pub unlock: Option<String>,
    pub unix_ms: u64,
}
/// The composition appends these as non-chain Net/Flow events; callbacks must not block.
pub type FlowSink = Arc<dyn Fn(FlowEvent) + Send + Sync>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub config: NetworkConfig,
    pub nics: Vec<Nic>,
    pub attached: Vec<String>,
    pub active_flows: usize,
    pub enforcement: String,
}

#[derive(Debug)]
pub enum NetError {
    Invalid(String),
    Io(std::io::Error),
    Unavailable(String),
}
impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) | Self::Unavailable(s) => f.write_str(s),
            Self::Io(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for NetError {}
impl From<std::io::Error> for NetError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub trait Network: Send + Sync {
    fn attach(&self, name: &str) -> Result<Attachment, NetError>;
    fn status(&self) -> Result<NetworkStatus, NetError>;
    /// Atomically replaces persisted operator policy/maps and closes pre-existing flows.
    fn configure(&self, config: NetworkConfig) -> Result<(), NetError>;
    fn set_flow_sink(&self, sink: Option<FlowSink>) -> Result<(), NetError>;
}
#[derive(Clone)]
pub struct NetworkHandle(pub Arc<dyn Network>);
impl aho_cap::Handle for NetworkHandle {
    const KIND: aho_cap::HandleKind = aho_cap::HandleKind::NetAttach;
}
