//! The operator's edits (`aho net allow`, `aho net map`) as pure changes to a `NetworkConfig`;
//! the caller applies the result to the running network or to its state directory.

use std::net::{IpAddr, SocketAddr};

use aho_net::{Access, Destination, NetError, NetworkConfig, PortMap, Transport};

use crate::policy::domain_matches;

/// The isolation classes `aho net allow` opens by name (the unlock a blocked flow names).
pub const CLASSES: [&str; 3] = ["internet", "host-lan", "other-sessions"];

/// Lets `what` through: an isolation class, or a destination (domain, `*.domain`, IP, CIDR).
/// A deny entry equal to it is removed; a non-empty allow list gains it. `Ok(None)` when it
/// already passes. A broader deny that still covers it is refused, never widened silently.
pub fn allow(config: &mut NetworkConfig, what: &str) -> Result<Option<String>, NetError> {
    let isolation = &mut config.isolation;
    let class = match what {
        "internet" => Some(&mut isolation.internet),
        "host-lan" => Some(&mut isolation.host_lan),
        "other-sessions" => Some(&mut isolation.other_sessions),
        _ => None,
    };
    if let Some(access) = class {
        if *access == Access::Allow {
            return Ok(None);
        }
        *access = Access::Allow;
        return Ok(Some(format!("isolation: {what} is now allowed")));
    }
    let destination = Destination::parse(what).map_err(|_| {
        NetError::Invalid(format!("`{what}` is neither a destination (domain, *.domain, IP, CIDR) nor one of {}", CLASSES.join(", ")))
    })?;
    let before = config.egress.deny.len();
    config.egress.deny.retain(|d| *d != destination);
    let removed = before != config.egress.deny.len();
    if let Some(cover) = config.egress.deny.iter().find(|d| covers(d, &destination)) {
        return Err(NetError::Invalid(format!("`{what}` is still denied by the broader egress.deny entry `{}`", show(cover))));
    }
    let added = !config.egress.allow.is_empty() && !config.egress.allow.contains(&destination);
    if added {
        config.egress.allow.push(destination);
    }
    Ok(match (removed, added) {
        (false, false) => None,
        (true, false) => Some(format!("egress: `{what}` is no longer denied")),
        (false, true) => Some(format!("egress: `{what}` joined the allow list")),
        (true, true) => Some(format!("egress: `{what}` is no longer denied and joined the allow list")),
    })
}

fn covers(deny: &Destination, target: &Destination) -> bool {
    match (deny, target) {
        (Destination::Domain(pattern), Destination::Domain(name)) => match name.strip_prefix("*.") {
            // `*.a.b` is covered by `*.b`: every name under a.b is under b.
            Some(base) => pattern.strip_prefix("*.").is_some_and(|p| domain_matches(&format!("*.{p}"), base)),
            None => domain_matches(pattern, name),
        },
        (Destination::Cidr(deny), Destination::Cidr(net)) => deny.contains(net),
        _ => false,
    }
}

fn show(d: &Destination) -> String {
    match d {
        Destination::Cidr(net) => net.to_string(),
        Destination::Domain(name) => name.clone(),
    }
}

/// `aho net map HOST[:GUEST]`: forwards `bind:HOST` to `guest:GUEST` (GUEST defaults to HOST).
/// Replaces an existing map on the same host address; `Ok(None)` when it is already there.
pub fn map(config: &mut NetworkConfig, spec: &str, bind: IpAddr, guest: &str, protocol: Transport) -> Result<Option<String>, NetError> {
    let bad = || NetError::Invalid(format!("`{spec}` is not a port map; write HOST_PORT:GUEST_PORT, e.g. 8080:3000"));
    let (host, guest_port) = match spec.split_once(':') {
        Some((h, g)) => (h.parse::<u16>().map_err(|_| bad())?, g.parse::<u16>().map_err(|_| bad())?),
        None => {
            let p = spec.parse::<u16>().map_err(|_| bad())?;
            (p, p)
        }
    };
    if host == 0 || guest_port == 0 {
        return Err(bad());
    }
    let entry = PortMap { protocol, host: SocketAddr::new(bind, host), guest: guest.into(), guest_port };
    let kind = match protocol {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    let line = format!("{kind} {} → {guest}:{guest_port}", entry.host);
    match config.port_maps.iter_mut().find(|m| m.protocol == protocol && m.host == entry.host) {
        Some(existing) if *existing == entry => Ok(None),
        Some(existing) => {
            *existing = entry;
            Ok(Some(format!("port map replaced: {line}")))
        }
        None => {
            config.port_maps.push(entry);
            Ok(Some(format!("port map added: {line}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NetworkConfig {
        NetworkConfig::with_prefix("t", 9)
    }

    #[test]
    fn allowing_a_destination_removes_its_deny_and_joins_only_a_restrictive_allow_list() {
        let mut c = config();
        c.egress.deny = vec![Destination::parse("crates.io").unwrap()];
        assert!(allow(&mut c, "crates.io").unwrap().is_some());
        assert!(c.egress.deny.is_empty());
        assert!(c.egress.allow.is_empty(), "an empty allow list means unrestricted: adding to it would narrow everything else");
        assert_eq!(allow(&mut c, "crates.io").unwrap(), None);

        c.egress.allow = vec![Destination::parse("github.com").unwrap()];
        assert!(allow(&mut c, "crates.io").unwrap().is_some());
        assert_eq!(c.egress.allow.len(), 2);
    }

    #[test]
    fn a_broader_deny_is_never_widened_and_nothing_changes() {
        let mut c = config();
        c.egress.deny = vec![Destination::parse("*.internal").unwrap(), Destination::parse("10.0.0.0/8").unwrap()];
        c.egress.allow = vec![Destination::parse("github.com").unwrap()];
        let before = c.clone();
        for what in ["db.internal", "*.corp.internal", "10.1.2.3", "10.1.0.0/16"] {
            let e = allow(&mut c, what).unwrap_err().to_string();
            assert!(e.contains("broader egress.deny"), "{what}: {e}");
            assert_eq!(c, before, "{what}");
        }
        // Not covered: `internal` itself is not under `*.internal`.
        assert!(allow(&mut c, "internal").unwrap().is_some());
    }

    #[test]
    fn isolation_classes_open_by_name_and_garbage_is_refused() {
        let mut c = config();
        assert_eq!(c.isolation.host_lan, Access::Deny);
        assert!(allow(&mut c, "host-lan").unwrap().is_some());
        assert_eq!(c.isolation.host_lan, Access::Allow);
        assert_eq!(allow(&mut c, "host-lan").unwrap(), None);
        assert!(allow(&mut c, "not a host!").is_err());
    }

    #[test]
    fn a_map_names_both_ports_or_one_and_replaces_its_host_address() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let mut c = config();
        assert!(map(&mut c, "8080:3000", lo, "sandbox", Transport::Tcp).unwrap().is_some());
        assert_eq!(map(&mut c, "8080:3000", lo, "sandbox", Transport::Tcp).unwrap(), None);
        assert!(map(&mut c, "8080:4000", lo, "sandbox", Transport::Tcp).unwrap().unwrap().contains("replaced"));
        assert!(map(&mut c, "8080", lo, "sandbox", Transport::Udp).unwrap().is_some());
        assert_eq!(c.port_maps.len(), 2);
        assert_eq!(c.port_maps[0].guest_port, 4000);
        assert_eq!(c.port_maps[1].guest_port, 8080);
        for bad in ["0:80", "80:0", "x:1", "1:2:3", ""] {
            assert!(map(&mut c, bad, lo, "sandbox", Transport::Tcp).is_err(), "{bad}");
        }
    }
}
