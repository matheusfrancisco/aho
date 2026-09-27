use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};

use aho_net::{Access, Destination, EgressPolicy, IpNet, Isolation, NetworkConfig};

const MAX_GUEST_GRANTS: usize = 256;
const MAX_GRANTS: usize = 4096;

struct Grant {
    domain: String,
    expires: Instant,
}

/// Only DNS answers validated by our resolver can create domain-to-IP authority.
/// An untrusted caller's claimed domain is never sufficient to authorize a flow.
pub(crate) struct Policy {
    isolation: Isolation,
    egress: EgressPolicy,
    host_ips: Vec<IpAddr>,
    host_networks: Vec<IpNet>,
    other_networks: Vec<IpNet>,
    grants: HashMap<String, HashMap<IpAddr, Vec<Grant>>>,
    valid: bool,
}

impl Policy {
    pub(crate) fn new(config: &NetworkConfig) -> Self {
        let mut egress = config.egress.clone();
        let mut valid = true;
        for rule in egress.allow.iter_mut().chain(&mut egress.deny) {
            if let Destination::Domain(pattern) = rule {
                let wildcard = pattern.starts_with("*.");
                let base = pattern.strip_prefix("*.").unwrap_or(pattern);
                if let Some(name) = normalize_domain(base) {
                    *pattern = if wildcard { format!("*.{name}") } else { name };
                } else {
                    valid = false;
                }
            }
        }
        Self {
            isolation: config.isolation.clone(),
            egress,
            host_ips: config.port_maps.iter().map(|map| canonical_ip(map.host.ip())).collect(),
            host_networks: Vec::new(),
            other_networks: Vec::new(),
            grants: HashMap::new(),
            valid,
        }
    }

    /// Include every host interface address, including globally routed addresses.
    /// Port-map bind addresses collected at construction remain classified as host.
    pub(crate) fn set_host_ips(&mut self, addresses: Vec<IpAddr>) {
        for address in addresses {
            let address = canonical_ip(address);
            if !self.host_ips.contains(&address) {
                self.host_ips.push(address);
            }
        }
    }

    pub(crate) fn set_other_networks(&mut self, networks: Vec<IpNet>) {
        self.other_networks = networks;
    }

    pub(crate) fn set_host_networks(&mut self, networks: Vec<IpNet>) {
        self.host_networks = networks;
    }

    pub(crate) fn check(&self, guest: &str, destination: IpAddr, domain: Option<&str>) -> Result<(), String> {
        self.check_address(destination, false)?;
        let domain = domain.map(|name| self.checked_name(name)).transpose()?;
        self.check_authority(guest, destination, domain.as_deref())
    }

    /// The runtime's registry knows whether a destination belongs to another
    /// session even before its subnet snapshot has reached this policy instance.
    pub(crate) fn check_other_session(&self, guest: &str, destination: IpAddr) -> Result<(), String> {
        self.check_address(destination, true)?;
        self.check_authority(guest, destination, None)
    }

    fn check_authority(&self, guest: &str, destination: IpAddr, domain: Option<&str>) -> Result<(), String> {
        if self.egress.allow.is_empty() || self.cidr_allowed(destination) {
            return Ok(());
        }
        let now = Instant::now();
        let granted = self.grants.get(guest).and_then(|addresses| addresses.get(&canonical_ip(destination))).is_some_and(|grants| {
            grants.iter().any(|grant| {
                grant.expires > now
                    && domain.is_none_or(|name| name == grant.domain)
                    && self.domain_allowed(&grant.domain)
                    && !self.domain_denied(&grant.domain)
            })
        });
        if granted { Ok(()) } else { Err("destination has no live guest-specific allow grant".into()) }
    }

    /// CIDR-only allow policies must be able to resolve names; the returned IP
    /// still has to match a CIDR before the answer is released to the guest.
    pub(crate) fn check_domain(&self, domain: &str) -> Result<(), String> {
        let name = self.checked_name(domain)?;
        if self.egress.allow.is_empty()
            || self.domain_allowed(&name)
            || self.egress.allow.iter().any(|rule| matches!(rule, Destination::Cidr(_)))
        {
            Ok(())
        } else {
            Err("domain is not allowed".into())
        }
    }

    /// A permitted name may use a CDN alias, but an explicitly denied alias
    /// cannot be laundered through that name.
    pub(crate) fn check_alias(&self, domain: &str) -> Result<(), String> {
        self.checked_name(domain).map(|_| ())
    }

    /// Same-network switching is not host egress, but explicit denies still win.
    pub(crate) fn check_local(&self, domain: &str, address: IpAddr) -> Result<(), String> {
        self.checked_name(domain)?;
        if self.egress.deny.iter().any(|rule| matches!(rule, Destination::Cidr(net) if cidr_matches(net, address))) {
            Err("destination CIDR is explicitly denied".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_resolved(&self, domain: &str, address: IpAddr) -> Result<(), String> {
        let name = self.checked_name(domain)?;
        self.check_address(address, false)?;
        if self.egress.allow.is_empty() || self.cidr_allowed(address) || self.domain_allowed(&name) {
            Ok(())
        } else {
            Err("DNS answer does not match an allowed destination".into())
        }
    }

    /// Called only after validating the complete response, never while walking
    /// an unvalidated answer section. Zero TTL conveys no reusable authority.
    pub(crate) fn observe(&mut self, guest: &str, domain: &str, address: IpAddr, ttl: u32) -> Result<(), String> {
        self.check_resolved(domain, address)?;
        let domain = self.checked_name(domain)?;
        if ttl == 0 || !self.domain_allowed(&domain) {
            return Ok(());
        }
        let now = Instant::now();
        let expires = now.checked_add(Duration::from_secs(u64::from(ttl))).ok_or_else(|| "DNS TTL is out of range".to_string())?;
        self.grants.retain(|_, addresses| {
            addresses.retain(|_, grants| {
                grants.retain(|grant| grant.expires > now);
                !grants.is_empty()
            });
            !addresses.is_empty()
        });
        let address = canonical_ip(address);
        if let Some(existing) = self
            .grants
            .get_mut(guest)
            .and_then(|addresses| addresses.get_mut(&address))
            .and_then(|grants| grants.iter_mut().find(|grant| grant.domain == domain))
        {
            existing.expires = expires;
            return Ok(());
        }
        let count = |addresses: &HashMap<IpAddr, Vec<Grant>>| addresses.values().map(Vec::len).sum::<usize>();
        if self.grants.values().map(count).sum::<usize>() >= MAX_GRANTS
            || self.grants.get(guest).is_some_and(|addresses| count(addresses) >= MAX_GUEST_GRANTS)
        {
            return Err("DNS authorization capacity exhausted".into());
        }
        self.grants.entry(guest.to_owned()).or_default().entry(address).or_default().push(Grant { domain, expires });
        Ok(())
    }

    fn checked_name(&self, name: &str) -> Result<String, String> {
        if !self.valid {
            return Err("invalid destination policy".into());
        }
        let name = normalize_domain(name).ok_or_else(|| "invalid domain name".to_string())?;
        if self.domain_denied(&name) { Err("domain is explicitly denied".into()) } else { Ok(name) }
    }

    fn domain_allowed(&self, name: &str) -> bool {
        self.egress.allow.iter().any(|rule| matches!(rule, Destination::Domain(pattern) if domain_matches(pattern, name)))
    }

    fn domain_denied(&self, name: &str) -> bool {
        self.egress.deny.iter().any(|rule| matches!(rule, Destination::Domain(pattern) if domain_matches(pattern, name)))
    }

    fn cidr_allowed(&self, address: IpAddr) -> bool {
        self.egress.allow.iter().any(|rule| matches!(rule, Destination::Cidr(net) if cidr_matches(net, address)))
    }

    fn check_address(&self, address: IpAddr, known_other_session: bool) -> Result<(), String> {
        if !self.valid {
            return Err("invalid destination policy".into());
        }
        if self.egress.deny.iter().any(|rule| matches!(rule, Destination::Cidr(net) if cidr_matches(net, address))) {
            return Err("destination CIDR is explicitly denied".into());
        }
        match self.isolation_of(address, known_other_session) {
            None => Err("special, reserved, or non-unicast destination".into()),
            Some((Access::Allow, _)) => Ok(()),
            Some((Access::Deny, _)) => Err("destination is blocked by isolation".into()),
        }
    }

    /// The isolation rule that governs `address` and its `aho net allow` keyword; `None` for a
    /// destination nothing may reach.
    fn isolation_of(&self, address: IpAddr, known_other_session: bool) -> Option<(Access, &'static str)> {
        let address = canonical_ip(address);
        let class = classify(address);
        if class == AddressClass::Unroutable {
            return None;
        }
        Some(if self.host_ips.contains(&address) || self.host_networks.iter().any(|net| cidr_matches(net, address)) {
            (self.isolation.host_lan, "host-lan")
        } else if known_other_session || self.other_networks.iter().any(|net| cidr_matches(net, address)) {
            (self.isolation.other_sessions, "other-sessions")
        } else if class == AddressClass::HostLan {
            (self.isolation.host_lan, "host-lan")
        } else {
            (self.isolation.internet, "internet")
        })
    }

    /// The command the operator runs to let a blocked flow to `address` through: the isolation
    /// class when isolation blocked it, else the destination itself. `None` when nothing can.
    pub(crate) fn unlock(&self, address: IpAddr) -> Option<String> {
        match self.isolation_of(address, false)? {
            (Access::Deny, class) => Some(format!("aho net allow {class}")),
            (Access::Allow, _) => Some(format!("aho net allow {}", canonical_ip(address))),
        }
    }
}

pub(crate) fn normalize_domain(name: &str) -> Option<String> {
    if name == "." {
        return Some(".".into());
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty()
        || name.len() > 253
        || name.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
    {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

pub(crate) fn domain_matches(pattern: &str, name: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        name.strip_suffix(suffix).is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.'))
    } else {
        pattern == name
    }
}

pub(crate) fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(address)),
        address => address,
    }
}

fn cidr_matches(net: &IpNet, address: IpAddr) -> bool {
    if net.contains(&address) {
        return true;
    }
    match canonical_ip(address) {
        IpAddr::V4(address) => net.contains(&IpAddr::V4(address)) || net.contains(&IpAddr::V6(address.to_ipv6_mapped())),
        IpAddr::V6(_) => false,
    }
}

#[derive(PartialEq, Eq)]
enum AddressClass {
    Internet,
    HostLan,
    Unroutable,
}

fn classify(address: IpAddr) -> AddressClass {
    match address {
        IpAddr::V4(address) => classify_v4(address),
        IpAddr::V6(address) => classify_v6(address),
    }
}

fn classify_v4(address: Ipv4Addr) -> AddressClass {
    let [a, b, c, _] = address.octets();
    if a == 0
        || a >= 224
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113)
    {
        AddressClass::Unroutable
    } else if a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
    {
        AddressClass::HostLan
    } else {
        AddressClass::Internet
    }
}

fn classify_v6(address: Ipv6Addr) -> AddressClass {
    if let Some(address) = address.to_ipv4_mapped() {
        return classify_v4(address);
    }
    let [a, b, c, ..] = address.segments();
    if address.is_loopback() || a & 0xfe00 == 0xfc00 || a & 0xffc0 == 0xfe80 || a & 0xffc0 == 0xfec0 {
        return AddressClass::HostLan;
    }
    // Accept only native global unicast, excluding documentation, benchmarking,
    // IETF special-use, Teredo, 6to4 and other embedded-IPv4 transition schemes.
    // In particular NAT64 and IPv4-compatible addresses cannot disguise LAN IPs.
    if a & 0xe000 != 0x2000
        || (a == 0x2001 && (b < 0x0200 || b == 0x0db8))
        || a == 0x2002
        || (a == 0x3fff && b < 0x1000)
        || (a == 0x2620 && b == 0x004f && c == 0x8000)
    {
        AddressClass::Unroutable
    } else {
        AddressClass::Internet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }
    fn destination(value: &str) -> Destination {
        Destination::parse(value).unwrap()
    }

    #[test]
    fn cidr_denies_win_over_domain_grants_and_overlapping_allows() {
        let mut config = NetworkConfig::default();
        config.egress.allow = vec![destination("8.0.0.0/8"), destination("allowed.example")];
        config.egress.deny = vec![destination("8.8.8.0/24")];
        let mut policy = Policy::new(&config);
        assert!(policy.observe("one", "allowed.example", ip("8.8.8.8"), 60).is_err());
        assert!(policy.check("one", ip("8.8.8.8"), Some("allowed.example")).is_err());
        assert!(policy.check("one", ip("::ffff:8.8.8.8"), None).is_err());
        assert!(policy.check("one", ip("8.8.9.0"), None).is_ok());
    }

    #[test]
    fn lan_special_and_mapped_addresses_cannot_be_domain_rebound() {
        let mut config = NetworkConfig::default();
        config.egress.allow = vec![destination("allowed.example")];
        let mut policy = Policy::new(&config);
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.1",
            "64:ff9b::a00:1",
            "2002:a00:1::1",
        ] {
            assert!(policy.observe("one", "allowed.example", ip(address), 60).is_err(), "{address}");
        }
        assert!(policy.observe("one", "allowed.example", ip("2606:4700:4700::1111"), 60).is_ok());
    }

    #[test]
    fn wildcard_matching_obeys_label_boundaries_and_deny_priority() {
        let mut config = NetworkConfig::default();
        config.egress.allow = vec![destination("*.example.com")];
        config.egress.deny = vec![destination("*.blocked.example.com")];
        let policy = Policy::new(&config);
        assert!(policy.check_domain("www.Example.COM.").is_ok());
        for name in ["example.com", "notexample.com", "www.example.com.evil", "evilblocked.example.com.evil", "x.blocked.example.com"] {
            assert!(policy.check_domain(name).is_err(), "{name}");
        }
        assert!(policy.check_domain("notblocked.example.com").is_ok());
    }

    #[test]
    fn grants_are_guest_specific_expiring_and_not_claimable() {
        let mut config = NetworkConfig::default();
        config.egress.allow = vec![destination("allowed.example")];
        let mut policy = Policy::new(&config);
        let address = ip("8.8.8.8");
        assert!(policy.check("one", address, Some("allowed.example")).is_err());
        policy.observe("one", "allowed.example", address, 60).unwrap();
        assert!(policy.check("one", address, None).is_ok());
        assert!(policy.check("two", address, None).is_err());
        policy.grants.get_mut("one").unwrap().get_mut(&address).unwrap()[0].expires = Instant::now();
        assert!(policy.check("one", address, None).is_err());
        policy.observe("one", "allowed.example", address, 0).unwrap();
        assert!(policy.check("one", address, None).is_err());
    }

    #[test]
    fn host_addresses_and_other_sessions_have_distinct_isolation() {
        let mut config = NetworkConfig::default();
        config.isolation.other_sessions = Access::Allow;
        let mut policy = Policy::new(&config);
        policy.set_host_ips(vec![ip("8.8.8.8")]);
        policy.set_other_networks(vec!["10.9.0.0/24".parse().unwrap()]);
        assert!(policy.check("one", ip("8.8.8.8"), None).is_err());
        assert!(policy.check("one", ip("10.9.0.1"), None).is_ok());
        assert!(policy.check("one", ip("10.9.1.1"), None).is_err());
        assert!(policy.check_other_session("one", ip("10.9.1.1")).is_ok());
        config.isolation.host_lan = Access::Allow;
        let policy = Policy::new(&config);
        assert!(policy.check("one", ip("::ffff:192.168.0.1"), None).is_ok());
        assert!(policy.check("one", ip("224.0.0.1"), None).is_err());
    }
}
