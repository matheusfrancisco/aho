use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{SystemTime, UNIX_EPOCH},
};

use aho_net::{GATEWAY_MAC, Nic, Transport};

use crate::packet;

const LEASE: u32 = 86_400;
const RENEW: u32 = 43_200;
const REBIND: u32 = 69_120;
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
const ALL_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 2);
const DHCP_SERVERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 2);

/// Also configure this address on the runtime interface: an IPv6 default router
/// must have a link-local source, not the ULA advertised as the DNS server.
pub(crate) fn router_link_local() -> Ipv6Addr {
    let mac = GATEWAY_MAC;
    Ipv6Addr::from([0xfe, 0x80, 0, 0, 0, 0, 0, 0, mac[0] ^ 2, mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]])
}

fn link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

fn multicast_mac(address: Ipv6Addr) -> [u8; 6] {
    let bytes = address.octets();
    [0x33, 0x33, bytes[12], bytes[13], bytes[14], bytes[15]]
}

fn subnet_mask(prefix: u8) -> Option<u32> {
    match prefix {
        0 => Some(0),
        1..=32 => Some(u32::MAX << (32 - prefix)),
        _ => None,
    }
}

fn v4_broadcast(nic: &Nic) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::from(u32::from(nic.ipv4) | !subnet_mask(nic.prefix_v4)?))
}

fn valid_destination_mac(frame: &[u8], destination: IpAddr, nic: &Nic) -> bool {
    let expected = match destination {
        IpAddr::V4(address) if address == Ipv4Addr::BROADCAST || Some(address) == v4_broadcast(nic) => [255; 6],
        IpAddr::V6(address) if address.is_multicast() => multicast_mac(address),
        _ => GATEWAY_MAC,
    };
    frame[..6] == expected
}

/// Classifies local link protocols even when their checksums/options are bad.
/// Callers must also drop unhandled gateway-addressed traffic: NTP at an external
/// destination is ordinary policy-controlled UDP, not a local service.
pub(crate) fn is_service(frame: &[u8]) -> bool {
    if frame.len() < 14 {
        return false;
    }
    let (protocol, offset) = match u16::from_be_bytes([frame[12], frame[13]]) {
        0x0800 if frame.len() >= 34 => (frame[23], 14 + usize::from(frame[14] & 15) * 4),
        0x86dd if frame.len() >= 54 => (frame[20], 54),
        _ => return false,
    };
    if protocol == 58 {
        return frame.get(offset) == Some(&133);
    }
    if protocol != 17 {
        return false;
    }
    let Some(ports) = frame.get(offset..offset + 4) else { return false };
    let source = u16::from_be_bytes([ports[0], ports[1]]);
    let destination = u16::from_be_bytes([ports[2], ports[3]]);
    matches!(source, 67 | 68 | 546 | 547) || matches!(destination, 67 | 68 | 546 | 547)
}

pub(crate) fn handle(frame: &[u8], nic: &Nic) -> Option<Vec<u8>> {
    let ip = packet::ip_packet(frame)?;
    if frame[6..12] != nic.mac || !valid_destination_mac(frame, ip.destination, nic) {
        return None;
    }
    if ip.protocol == 58 {
        return solicitation(&ip, nic);
    }
    let request = packet::parse(frame)?;
    if request.protocol != Transport::Udp {
        return None;
    }
    match (request.source, request.destination) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) if source.port() == 68 && destination.port() == 67 => {
            dhcp4(request.payload, *source.ip(), *destination.ip(), nic)
        }
        (SocketAddr::V6(source), SocketAddr::V6(destination)) if source.port() == 546 && destination.port() == 547 => {
            dhcp6(request.payload, *source.ip(), *destination.ip(), nic)
        }
        (_, destination) if destination.port() == 123 => ntp(&request, nic),
        _ => None,
    }
}

#[derive(Default)]
struct V4Options<'a> {
    kind: Option<u8>,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
    client: Option<&'a [u8]>,
}

fn v4_options(mut bytes: &[u8]) -> Option<V4Options<'_>> {
    let mut result = V4Options::default();
    let mut seen = [false; 256];
    while let Some((&code, rest)) = bytes.split_first() {
        bytes = rest;
        match code {
            0 => continue,
            255 => return bytes.iter().all(|&byte| byte == 0).then_some(result),
            _ => {}
        }
        let (&length, rest) = bytes.split_first()?;
        let length = usize::from(length);
        let value = rest.get(..length)?;
        bytes = &rest[length..];
        // Option concatenation/overload is not needed for the supported small
        // options. Refuse duplicates instead of disagreeing with another parser.
        if seen[usize::from(code)] || code == 52 {
            return None;
        }
        seen[usize::from(code)] = true;
        match code {
            53 if length == 1 => result.kind = Some(value[0]),
            50 | 54 if length == 4 => {
                let address = Ipv4Addr::new(value[0], value[1], value[2], value[3]);
                if code == 50 {
                    result.requested = Some(address);
                } else {
                    result.server = Some(address);
                }
            }
            61 if length >= 2 => result.client = Some(value),
            57 if length == 2 && u16::from_be_bytes([value[0], value[1]]) >= 576 => {}
            50 | 53 | 54 | 57 | 61 => return None,
            _ => {}
        }
    }
    None // The DHCP end marker is required, including after padding.
}

fn option4(output: &mut Vec<u8>, code: u8, value: &[u8]) {
    output.extend_from_slice(&[code, value.len() as u8]);
    output.extend_from_slice(value);
}

fn dhcp4(bytes: &[u8], source: Ipv4Addr, destination: Ipv4Addr, nic: &Nic) -> Option<Vec<u8>> {
    let mask = subnet_mask(nic.prefix_v4)?;
    if bytes.len() < 240
        || bytes[..4] != [1, 1, 6, 0]
        || bytes[236..240] != [99, 130, 83, 99]
        || bytes[28..34] != nic.mac
        || bytes[16..28].iter().any(|&byte| byte != 0)
        || (!source.is_unspecified() && source != nic.ipv4)
        || (destination != Ipv4Addr::BROADCAST && destination != v4_broadcast(nic)? && destination != nic.gateway_v4)
    {
        return None;
    }
    let flags = u16::from_be_bytes([bytes[10], bytes[11]]);
    if flags & 0x7fff != 0 {
        return None;
    }
    let ciaddr = Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]);
    if (!ciaddr.is_unspecified() && ciaddr != nic.ipv4) || (source.is_unspecified() != ciaddr.is_unspecified()) {
        return None;
    }
    let options = v4_options(&bytes[240..])?;
    let kind = options.kind?;
    let response_kind = match kind {
        1 if ciaddr.is_unspecified() && options.server.is_none() => 2,
        3 => {
            if let Some(server) = options.server
                && (server != nic.gateway_v4 || !ciaddr.is_unspecified() || options.requested.is_none())
            {
                return None;
            }
            if let Some(requested) = options.requested {
                if !ciaddr.is_unspecified() {
                    return None;
                }
                if requested == nic.ipv4 { 5 } else { 6 }
            } else if ciaddr == nic.ipv4 {
                5
            } else {
                return None;
            }
        }
        8 if ciaddr == nic.ipv4 && options.requested.is_none() && options.server.is_none() => 5,
        _ => return None,
    };
    let nak = response_kind == 6;
    let client_length = options.client.map_or(0, |client| client.len() + 2);
    let configuration_length = if nak {
        0
    } else if kind == 8 {
        28
    } else {
        46
    };
    let mut reply = Vec::with_capacity((250 + client_length + configuration_length).max(300));
    reply.resize(240, 0);
    reply[..4].copy_from_slice(&[2, 1, 6, 0]);
    reply[4..8].copy_from_slice(&bytes[4..8]);
    reply[10..12].copy_from_slice(&if nak { 0x8000u16 } else { flags }.to_be_bytes());
    if !nak {
        reply[12..16].copy_from_slice(&ciaddr.octets());
        if kind != 8 {
            reply[16..20].copy_from_slice(&nic.ipv4.octets());
        }
    }
    reply[28..44].copy_from_slice(&bytes[28..44]);
    reply[236..240].copy_from_slice(&[99, 130, 83, 99]);
    option4(&mut reply, 53, &[response_kind]);
    option4(&mut reply, 54, &nic.gateway_v4.octets());
    if let Some(client) = options.client {
        option4(&mut reply, 61, client);
    }
    if !nak {
        option4(&mut reply, 1, &mask.to_be_bytes());
        option4(&mut reply, 3, &nic.gateway_v4.octets());
        option4(&mut reply, 6, &nic.gateway_v4.octets());
        option4(&mut reply, 42, &nic.gateway_v4.octets());
        option4(&mut reply, 26, &1500u16.to_be_bytes());
        if kind != 8 {
            option4(&mut reply, 51, &LEASE.to_be_bytes());
            option4(&mut reply, 58, &RENEW.to_be_bytes());
            option4(&mut reply, 59, &REBIND.to_be_bytes());
        }
    }
    reply.push(255);
    reply.resize(reply.len().max(300), 0);
    let broadcast = nak || (ciaddr.is_unspecified() && flags & 0x8000 != 0);
    let destination = if broadcast {
        Ipv4Addr::BROADCAST
    } else if !ciaddr.is_unspecified() {
        ciaddr
    } else {
        nic.ipv4
    };
    Some(packet::udp_frame(
        GATEWAY_MAC,
        if broadcast { [255; 6] } else { nic.mac },
        SocketAddr::from((nic.gateway_v4, 67)),
        SocketAddr::from((destination, 68)),
        &reply,
    ))
}

struct Options6<'a>(&'a [u8]);

impl<'a> Iterator for Options6<'a> {
    type Item = Result<(u16, &'a [u8]), ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        if self.0.len() < 4 {
            self.0 = &[];
            return Some(Err(()));
        }
        let code = u16::from_be_bytes([self.0[0], self.0[1]]);
        let length = usize::from(u16::from_be_bytes([self.0[2], self.0[3]]));
        if length > self.0.len() - 4 {
            self.0 = &[];
            return Some(Err(()));
        }
        let value = &self.0[4..4 + length];
        self.0 = &self.0[4 + length..];
        Some(Ok((code, value)))
    }
}

fn option6(output: &mut Vec<u8>, code: u16, value: &[u8]) {
    output.extend_from_slice(&code.to_be_bytes());
    output.extend_from_slice(&(value.len() as u16).to_be_bytes());
    output.extend_from_slice(value);
}

fn server_duid() -> [u8; 10] {
    let mut duid = [0, 3, 0, 1, 0, 0, 0, 0, 0, 0];
    duid[4..].copy_from_slice(&GATEWAY_MAC);
    duid
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn valid_duid(bytes: &[u8]) -> bool {
    // Identity remains opaque; only the known wire formats are validated.
    // A DUID never replaces the Ethernet source authorization.
    if !(3..=128).contains(&bytes.len()) {
        return false;
    }
    let ethernet = bytes.get(2..4) == Some([0, 1].as_slice());
    match u16::from_be_bytes([bytes[0], bytes[1]]) {
        0 => false,
        1 => {
            if ethernet {
                bytes.len() == 14
            } else {
                bytes.len() >= 9
            }
        }
        2 => bytes.len() >= 7,
        3 => {
            if ethernet {
                bytes.len() == 10
            } else {
                bytes.len() >= 5
            }
        }
        4 => bytes.len() == 18,
        _ => true,
    }
}

fn validate_ia(bytes: &[u8]) -> Option<()> {
    if bytes.len() < 12 {
        return None;
    }
    let t1 = read_u32(&bytes[4..8]);
    let t2 = read_u32(&bytes[8..12]);
    if t1 != 0 && t2 != 0 && t1 > t2 {
        return None;
    }
    for option in Options6(&bytes[12..]) {
        let (code, value) = option.ok()?;
        match code {
            5 => {
                if value.len() < 24 || read_u32(&value[16..20]) > read_u32(&value[20..24]) {
                    return None;
                }
                let address = Ipv6Addr::from(<[u8; 16]>::try_from(&value[..16]).ok()?);
                if address.is_unspecified() || address.is_multicast() || link_local(address) {
                    return None;
                }
                for suboption in Options6(&value[24..]) {
                    let (code, value) = suboption.ok()?;
                    if matches!(code, 3 | 5) || (code == 13 && value.len() < 2) {
                        return None;
                    }
                }
            }
            13 if value.len() >= 2 => {}
            3 | 13 => return None,
            _ => {}
        }
    }
    Some(())
}

struct V6Options<'a> {
    client: Option<&'a [u8]>,
    server: Option<&'a [u8]>,
    associations: usize,
}

fn v6_options(bytes: &[u8], kind: u8) -> Option<V6Options<'_>> {
    let mut result = V6Options { client: None, server: None, associations: 0 };
    let mut seen_iaids = [0u32; 16];
    let mut count = 0;
    let mut oro = false;
    let mut elapsed = false;
    let mut rapid = false;
    for option in Options6(bytes) {
        let (code, value) = option.ok()?;
        match code {
            1 | 2 => {
                if !valid_duid(value) {
                    return None;
                }
                let slot = if code == 1 { &mut result.client } else { &mut result.server };
                if slot.replace(value).is_some() {
                    return None;
                }
            }
            3 => {
                if kind == 11 || count == seen_iaids.len() {
                    return None;
                }
                validate_ia(value)?;
                let iaid = read_u32(value);
                if seen_iaids[..count].contains(&iaid) {
                    return None;
                }
                seen_iaids[count] = iaid;
                count += 1;
            }
            6 => {
                if oro || value.len() % 2 != 0 {
                    return None;
                }
                oro = true;
            }
            8 => {
                if elapsed || value.len() != 2 {
                    return None;
                }
                elapsed = true;
            }
            14 => {
                if rapid || !value.is_empty() || kind != 1 {
                    return None;
                }
                rapid = true;
            }
            20 if value.is_empty() => {}
            23 if !value.is_empty() && value.len() % 16 == 0 => {}
            // Relayed/authenticated traffic, bare IAADDR, and unsupported IA
            // families must not be mistaken for a supported direct exchange.
            4 | 5 | 9 | 11 | 20 | 23 | 25 => return None,
            _ => {}
        }
    }
    result.associations = count;
    Some(result)
}

fn on_link(address: Ipv6Addr, nic: &Nic) -> bool {
    let Some(shift) = 128u32.checked_sub(u32::from(nic.prefix_v6)) else { return false };
    let mask = if shift == 128 { 0 } else { u128::MAX << shift };
    u128::from(address) & mask == u128::from(nic.ipv6) & mask
}

fn ia_addresses(bytes: &[u8]) -> impl Iterator<Item = Ipv6Addr> + '_ {
    // Called only after validate_ia has checked every nested option.
    Options6(&bytes[12..]).filter_map(|option| {
        let (code, value) = option.ok()?;
        if code != 5 {
            return None;
        }
        value.get(..16).and_then(|b| <[u8; 16]>::try_from(b).ok()).map(Ipv6Addr::from)
    })
}

fn ia_reply(output: &mut Vec<u8>, iaid: &[u8], status: Option<u16>, nic: &Nic) {
    let mut value = [0u8; 40];
    value[..4].copy_from_slice(iaid);
    if let Some(status) = status {
        value[12..14].copy_from_slice(&13u16.to_be_bytes());
        value[14..16].copy_from_slice(&2u16.to_be_bytes());
        value[16..18].copy_from_slice(&status.to_be_bytes());
        option6(output, 3, &value[..18]);
    } else {
        value[4..8].copy_from_slice(&RENEW.to_be_bytes());
        value[8..12].copy_from_slice(&REBIND.to_be_bytes());
        value[12..14].copy_from_slice(&5u16.to_be_bytes());
        value[14..16].copy_from_slice(&24u16.to_be_bytes());
        value[16..32].copy_from_slice(&nic.ipv6.octets());
        value[32..36].copy_from_slice(&LEASE.to_be_bytes());
        value[36..40].copy_from_slice(&LEASE.to_be_bytes());
        option6(output, 3, &value);
    }
}

fn dhcp6(bytes: &[u8], source: Ipv6Addr, destination: Ipv6Addr, nic: &Nic) -> Option<Vec<u8>> {
    if bytes.len() < 4
        || nic.prefix_v6 > 128
        || (!link_local(source) && source != nic.ipv6)
        || (destination != DHCP_SERVERS && destination != router_link_local() && destination != nic.gateway_v6)
    {
        return None;
    }
    let kind = bytes[0];
    if !matches!(kind, 1 | 3 | 4 | 5 | 6 | 11) {
        return None;
    }
    let options = v6_options(&bytes[4..], kind)?;
    let duid = server_duid();
    if kind != 11 && options.client.is_none() {
        return None;
    }
    match kind {
        1 | 4 | 6 if options.server.is_some() => return None,
        3 | 5 if options.server != Some(duid.as_slice()) => return None,
        11 if options.server.is_some_and(|server| server != duid) => return None,
        _ => {}
    }
    let unicast = destination != DHCP_SERVERS;
    if unicast && !matches!(kind, 3 | 5 | 11) {
        return None;
    }
    let client_length = options.client.map_or(0, |client| client.len() + 4);
    let mut reply = Vec::with_capacity(46 + client_length + options.associations * 44);
    reply.push(if kind == 1 { 2 } else { 7 });
    reply.extend_from_slice(&bytes[1..4]);
    if let Some(client) = options.client {
        option6(&mut reply, 1, client);
    }
    option6(&mut reply, 2, &duid);
    if unicast && kind != 11 {
        // We never advertise Server Unicast. A client must retry on-link.
        option6(&mut reply, 13, &5u16.to_be_bytes());
    } else if kind == 4 {
        let mut any = false;
        let mut status = 0u16;
        for option in Options6(&bytes[4..]) {
            let (code, value) = option.ok()?;
            if code == 3 {
                for address in ia_addresses(value) {
                    any = true;
                    if !on_link(address, nic) {
                        status = 4; // NotOnLink, not a claim of address ownership.
                    }
                }
            }
        }
        if !any {
            return None;
        }
        option6(&mut reply, 13, &status.to_be_bytes());
    } else {
        let mut allocated = false;
        for option in Options6(&bytes[4..]) {
            let (code, value) = option.ok()?;
            if code != 3 {
                continue;
            }
            let unknown = matches!(kind, 5 | 6) && ia_addresses(value).any(|address| address != nic.ipv6);
            let status = if unknown {
                Some(3)
            }
            // NoBinding
            else if allocated {
                Some(2)
            }
            // NoAddrsAvail: only one address per NIC.
            else {
                allocated = true;
                None
            };
            ia_reply(&mut reply, &value[..4], status, nic);
        }
        // Guest-supplied DNS options are never copied or used as upstreams.
        option6(&mut reply, 23, &nic.gateway_v6.octets());
        if kind == 11 {
            option6(&mut reply, 32, &LEASE.to_be_bytes());
        }
    }
    Some(packet::udp_frame(GATEWAY_MAC, nic.mac, SocketAddr::from((router_link_local(), 547)), SocketAddr::from((source, 546)), &reply))
}

pub(crate) fn advertisement(nic: &Nic) -> Vec<u8> {
    router_advertisement(nic, ALL_NODES, multicast_mac(ALL_NODES))
}

fn router_advertisement(nic: &Nic, destination: Ipv6Addr, destination_mac: [u8; 6]) -> Vec<u8> {
    if nic.prefix_v6 > 128 {
        return Vec::new();
    }
    let mut body = [0u8; 88];
    body[0] = 134;
    body[4] = 64;
    body[5] = 0xc0; // Managed address configuration and other DHCPv6 information.
    body[6..8].copy_from_slice(&1800u16.to_be_bytes());
    body[16..18].copy_from_slice(&[1, 1]);
    body[18..24].copy_from_slice(&GATEWAY_MAC);
    body[24..26].copy_from_slice(&[5, 1]);
    body[28..32].copy_from_slice(&1500u32.to_be_bytes());
    body[32..36].copy_from_slice(&[3, 4, nic.prefix_v6, 0x80]); // On-link, not SLAAC.
    body[36..40].copy_from_slice(&LEASE.to_be_bytes());
    body[40..44].copy_from_slice(&RENEW.to_be_bytes());
    let mask = if nic.prefix_v6 == 0 { 0 } else { u128::MAX << (128 - nic.prefix_v6) };
    body[48..64].copy_from_slice(&(u128::from(nic.ipv6) & mask).to_be_bytes());
    body[64..66].copy_from_slice(&[25, 3]);
    body[68..72].copy_from_slice(&1800u32.to_be_bytes());
    body[72..88].copy_from_slice(&nic.gateway_v6.octets());
    packet::icmpv6_frame(GATEWAY_MAC, destination_mac, router_link_local(), destination, &body)
}

fn solicitation(ip: &packet::IpPacket<'_>, nic: &Nic) -> Option<Vec<u8>> {
    let (IpAddr::V6(source), IpAddr::V6(destination)) = (ip.source, ip.destination) else { return None };
    let bytes = ip.payload;
    let icmp = smoltcp::wire::Icmpv6Packet::new_checked(bytes).ok()?;
    if ip.hop_limit != 255
        || bytes.len() < 8
        || bytes[0] != 133
        || bytes[1] != 0
        || bytes[4..8] != [0; 4]
        || !icmp.verify_checksum(&source, &destination)
        || (!source.is_unspecified() && !link_local(source) && source != nic.ipv6)
        || (destination != ALL_ROUTERS && destination != router_link_local() && destination != nic.gateway_v6)
    {
        return None;
    }
    let mut options = &bytes[8..];
    let mut source_option = false;
    while !options.is_empty() {
        let length = usize::from(*options.get(1)?) * 8;
        if length == 0 || length > options.len() {
            return None;
        }
        if options[0] == 1 {
            if source_option || source.is_unspecified() || length != 8 || options[2..8] != nic.mac {
                return None;
            }
            source_option = true;
        }
        options = &options[length..];
    }
    Some(if source.is_unspecified() { advertisement(nic) } else { router_advertisement(nic, source, nic.mac) })
}

fn timestamp() -> Option<[u8; 8]> {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let seconds = time.as_secs().wrapping_add(2_208_988_800) as u32;
    let fraction = ((u64::from(time.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut value = [0; 8];
    value[..4].copy_from_slice(&seconds.to_be_bytes());
    value[4..].copy_from_slice(&fraction.to_be_bytes());
    Some(value)
}

fn ntp(request: &packet::Packet<'_>, nic: &Nic) -> Option<Vec<u8>> {
    let allowed = match (request.source.ip(), request.destination.ip()) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => source == nic.ipv4 && destination == nic.gateway_v4,
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            (source == nic.ipv6 || link_local(source)) && (destination == nic.gateway_v6 || destination == router_link_local())
        }
        _ => false,
    };
    let bytes = request.payload;
    if !allowed || request.source.port() == 0 || bytes.len() != 48 {
        return None;
    }
    let version = (bytes[0] >> 3) & 7;
    if !matches!(version, 3 | 4) || bytes[0] & 7 != 3 {
        return None;
    }
    let received = timestamp()?;
    let mut reply = [0u8; 48];
    reply[0] = version << 3 | 4;
    // A local host clock, deliberately below real synchronized time sources.
    // This service makes no claim to an external/GPS/atomic reference.
    reply[1] = 10;
    reply[2] = (bytes[2] as i8).clamp(4, 17) as u8;
    reply[3] = (-20i8) as u8;
    reply[8..12].copy_from_slice(&65_536u32.to_be_bytes());
    reply[12..16].copy_from_slice(b"LOCL");
    reply[16..24].copy_from_slice(&received);
    reply[24..32].copy_from_slice(&bytes[40..48]);
    reply[32..40].copy_from_slice(&received);
    reply[40..48].copy_from_slice(&timestamp()?);
    Some(packet::udp_frame(GATEWAY_MAC, nic.mac, request.destination, request.source, &reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic() -> Nic {
        Nic {
            name: "test".into(),
            mac: [2, 0, 0, 0, 0, 2],
            ipv4: "10.85.1.2".parse().unwrap(),
            gateway_v4: "10.85.1.1".parse().unwrap(),
            ipv6: "fd85:1::2".parse().unwrap(),
            gateway_v6: "fd85:1::1".parse().unwrap(),
            prefix_v4: 24,
            prefix_v6: 64,
        }
    }

    fn request4(kind: u8, extra: &[(u8, &[u8])], ciaddr: Ipv4Addr) -> Vec<u8> {
        let nic = nic();
        let mut body = vec![0; 240];
        body[..4].copy_from_slice(&[1, 1, 6, 0]);
        body[4..8].copy_from_slice(&[1, 2, 3, 4]);
        body[12..16].copy_from_slice(&ciaddr.octets());
        body[28..34].copy_from_slice(&nic.mac);
        body[236..240].copy_from_slice(&[99, 130, 83, 99]);
        option4(&mut body, 53, &[kind]);
        for (code, value) in extra {
            option4(&mut body, *code, value);
        }
        body.push(255);
        packet::udp_frame(nic.mac, [255; 6], SocketAddr::from((ciaddr, 68)), SocketAddr::from((Ipv4Addr::BROADCAST, 67)), &body)
    }

    fn find4(mut bytes: &[u8], wanted: u8) -> Option<&[u8]> {
        while let Some((&code, rest)) = bytes.split_first() {
            if code == 255 {
                return None;
            }
            if code == 0 {
                bytes = rest;
                continue;
            }
            let length = usize::from(rest[0]);
            if code == wanted {
                return Some(&rest[1..1 + length]);
            }
            bytes = &rest[1 + length..];
        }
        None
    }

    fn duid() -> [u8; 10] {
        let mut value = [0, 3, 0, 1, 0, 0, 0, 0, 0, 0];
        value[4..].copy_from_slice(&nic().mac);
        value
    }

    fn ia(address: Option<Ipv6Addr>) -> Vec<u8> {
        let mut bytes = vec![0; 12];
        bytes[..4].copy_from_slice(&42u32.to_be_bytes());
        if let Some(address) = address {
            let mut value = [0u8; 24];
            value[..16].copy_from_slice(&address.octets());
            option6(&mut bytes, 5, &value);
        }
        bytes
    }

    fn request6(kind: u8, extra: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![kind, 1, 2, 3];
        option6(&mut body, 1, &duid());
        for (code, value) in extra {
            option6(&mut body, *code, value);
        }
        packet::udp_frame(
            nic().mac,
            multicast_mac(DHCP_SERVERS),
            "[fe80::2]:546".parse().unwrap(),
            SocketAddr::from((DHCP_SERVERS, 547)),
            &body,
        )
    }

    fn find6(bytes: &[u8], wanted: u16) -> Option<&[u8]> {
        Options6(bytes).find_map(|option| {
            let (code, value) = option.unwrap();
            (code == wanted).then_some(value)
        })
    }

    #[test]
    fn dhcp4_offer_is_stable_and_does_not_echo_requested_dns() {
        let nic = nic();
        let frame = request4(1, &[(50, &[10, 85, 1, 99]), (6, &[8, 8, 8, 8]), (61, &[1, 9])], Ipv4Addr::UNSPECIFIED);
        let response = handle(&frame, &nic).unwrap();
        let reply = packet::parse(&response).unwrap();
        assert_eq!(reply.payload[16..20], nic.ipv4.octets());
        assert_eq!(reply.payload[4..8], [1, 2, 3, 4]);
        assert_eq!(find4(&reply.payload[240..], 53), Some([2].as_slice()));
        assert_eq!(find4(&reply.payload[240..], 6), Some(nic.gateway_v4.octets().as_slice()));
        assert_eq!(find4(&reply.payload[240..], 61), Some([1, 9].as_slice()));
    }

    #[test]
    fn dhcp4_selecting_renewing_and_wrong_address_obey_server_semantics() {
        let nic = nic();
        for kind in [0, 1, 2] {
            let requested = if kind == 2 { [10, 85, 1, 99] } else { nic.ipv4.octets() };
            let server = nic.gateway_v4.octets();
            let extra = [(50, requested.as_slice()), (54, server.as_slice())];
            let response = if kind == 1 {
                handle(&request4(3, &[], nic.ipv4), &nic).unwrap()
            } else {
                handle(&request4(3, &extra, Ipv4Addr::UNSPECIFIED), &nic).unwrap()
            };
            let packet = packet::parse(&response).unwrap();
            assert_eq!(find4(&packet.payload[240..], 53), Some([if kind == 2 { 6 } else { 5 }].as_slice()));
            if kind == 2 {
                assert_eq!(packet.destination.ip(), IpAddr::V4(Ipv4Addr::BROADCAST));
                assert_eq!(packet.payload[16..20], [0; 4]);
            }
        }
        assert!(handle(&request4(3, &[(50, &nic.ipv4.octets()), (54, &[10, 85, 1, 9])], Ipv4Addr::UNSPECIFIED), &nic).is_none());
    }

    #[test]
    fn dhcp4_rejects_duplicate_options_relays_and_forged_sources() {
        let nic = nic();
        let duplicate = request4(1, &[(53, &[3])], Ipv4Addr::UNSPECIFIED);
        assert!(is_service(&duplicate));
        assert!(handle(&duplicate, &nic).is_none());
        let frame = request4(1, &[], Ipv4Addr::UNSPECIFIED);
        let packet = packet::parse(&frame).unwrap();
        for (offset, value) in [(24, 1), (28, 4)] {
            let mut body = packet.payload.to_vec();
            body[offset] = value;
            let corrupt = packet::udp_frame(nic.mac, [255; 6], packet.source, packet.destination, &body);
            assert!(handle(&corrupt, &nic).is_none());
        }
        let body = &packet.payload[..packet.payload.len() - 1];
        let unterminated = packet::udp_frame(nic.mac, [255; 6], packet.source, packet.destination, body);
        assert!(handle(&unterminated, &nic).is_none());
    }

    #[test]
    fn dhcp6_solicit_request_renew_rebind_preserve_ids_and_fixed_lease() {
        let nic = nic();
        let ia = ia(Some(nic.ipv6));
        let server = server_duid();
        for kind in [1, 3, 5, 6] {
            let mut options: Vec<(u16, &[u8])> = vec![(3, &ia)];
            if matches!(kind, 3 | 5) {
                options.push((2, &server));
            }
            let response = handle(&request6(kind, &options), &nic).unwrap();
            let packet = packet::parse(&response).unwrap();
            assert_eq!(packet.payload[..4], [if kind == 1 { 2 } else { 7 }, 1, 2, 3]);
            assert_eq!(find6(&packet.payload[4..], 1), Some(duid().as_slice()));
            assert_eq!(find6(&packet.payload[4..], 2), Some(server.as_slice()));
            let association = find6(&packet.payload[4..], 3).unwrap();
            assert_eq!(association[..4], 42u32.to_be_bytes());
            assert_eq!(&find6(&association[12..], 5).unwrap()[..16], &nic.ipv6.octets());
            assert_eq!(find6(&packet.payload[4..], 23), Some(nic.gateway_v6.octets().as_slice()));
        }
    }

    #[test]
    fn dhcp6_confirm_reports_link_membership_and_renew_unknown_reports_no_binding() {
        let nic = nic();
        for (address, status) in [("fd85:1::99", 0u16), ("fd85:2::2", 4)] {
            let association = ia(Some(address.parse().unwrap()));
            let response = handle(&request6(4, &[(3, &association)]), &nic).unwrap();
            let packet = packet::parse(&response).unwrap();
            assert_eq!(find6(&packet.payload[4..], 13), Some(status.to_be_bytes().as_slice()));
            assert!(find6(&packet.payload[4..], 3).is_none());
        }
        let association = ia(Some("fd85:1::99".parse().unwrap()));
        let response = handle(&request6(5, &[(3, &association), (2, &server_duid())]), &nic).unwrap();
        let packet = packet::parse(&response).unwrap();
        let association = find6(&packet.payload[4..], 3).unwrap();
        assert_eq!(find6(&association[12..], 13), Some(3u16.to_be_bytes().as_slice()));
        assert!(find6(&association[12..], 5).is_none());
    }

    #[test]
    fn dhcp6_rejects_malformed_ids_ias_and_wrong_server() {
        let nic = nic();
        let association = ia(Some(nic.ipv6));
        assert!(handle(&request6(3, &[(3, &association)]), &nic).is_none());
        assert!(handle(&request6(3, &[(3, &association), (2, &duid())]), &nic).is_none());
        assert!(handle(&request6(1, &[(3, &association), (3, &association)]), &nic).is_none());
        assert!(handle(&request6(1, &[(1, &duid())]), &nic).is_none());
        assert!(handle(&request6(1, &[(3, &[0; 11])]), &nic).is_none());
        assert!(handle(&request6(1, &[(6, &[0])]), &nic).is_none());
        let mut truncated = ia(None);
        truncated.extend_from_slice(&[0, 5, 0, 24, 1]);
        assert!(handle(&request6(1, &[(3, &truncated)]), &nic).is_none());
    }

    #[test]
    fn dhcp6_information_request_has_local_dns_and_no_lease() {
        let nic = nic();
        let response = handle(&request6(11, &[(6, &[0, 23])]), &nic).unwrap();
        let packet = packet::parse(&response).unwrap();
        assert_eq!(packet.payload[0], 7);
        assert_eq!(find6(&packet.payload[4..], 23), Some(nic.gateway_v6.octets().as_slice()));
        assert!(find6(&packet.payload[4..], 3).is_none());
        assert!(handle(&request6(11, &[(3, &ia(None))]), &nic).is_none());
    }

    #[test]
    fn ra_has_link_local_default_router_managed_prefix_dns_mtu_and_checksum() {
        let nic = nic();
        let frame = advertisement(&nic);
        let ip = packet::ip_packet(&frame).unwrap();
        assert_eq!(ip.source, IpAddr::V6(router_link_local()));
        assert_eq!(ip.destination, IpAddr::V6(ALL_NODES));
        assert_eq!(ip.hop_limit, 255);
        assert_eq!(packet::transport_checksum(ip.source, ip.destination, 58, ip.payload), 0);
        let icmp = smoltcp::wire::Icmpv6Packet::new_checked(ip.payload).unwrap();
        assert!(icmp.verify_checksum(&router_link_local(), &ALL_NODES));
        assert_eq!(&ip.payload[..2], &[134, 0]);
        assert_eq!(ip.payload[5], 0xc0);
        assert_eq!(&ip.payload[6..8], &1800u16.to_be_bytes());
        assert_eq!(&ip.payload[28..32], &1500u32.to_be_bytes());
        assert_eq!(&ip.payload[32..36], &[3, 4, 64, 0x80]);
        assert_eq!(&ip.payload[48..64], &"fd85:1::".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(&ip.payload[72..88], &nic.gateway_v6.octets());
    }

    #[test]
    fn dhcp6_unicast_renew_requires_multicast_and_anonymous_information_is_supported() {
        let nic = nic();
        let multicast = request6(5, &[(2, &server_duid()), (3, &ia(Some(nic.ipv6)))]);
        let original = packet::parse(&multicast).unwrap();
        let unicast =
            packet::udp_frame(nic.mac, GATEWAY_MAC, original.source, SocketAddr::from((router_link_local(), 547)), original.payload);
        let response = handle(&unicast, &nic).unwrap();
        let reply = packet::parse(&response).unwrap();
        assert_eq!(find6(&reply.payload[4..], 13), Some(5u16.to_be_bytes().as_slice()));
        assert!(find6(&reply.payload[4..], 3).is_none());
        let anonymous = packet::udp_frame(nic.mac, multicast_mac(DHCP_SERVERS), original.source, original.destination, &[11, 5, 6, 7]);
        let response = handle(&anonymous, &nic).unwrap();
        let reply = packet::parse(&response).unwrap();
        assert_eq!(reply.payload[..4], [7, 5, 6, 7]);
        assert!(find6(&reply.payload[4..], 1).is_none());
        assert_eq!(find6(&reply.payload[4..], 23), Some(nic.gateway_v6.octets().as_slice()));
    }

    #[test]
    fn known_duid_formats_and_association_limits_are_checked() {
        assert!(!valid_duid(&[0, 3, 0, 1, 9]));
        assert!(!valid_duid(&[0, 4, 0, 1, 2, 3]));
        assert!(valid_duid(&duid()));
        let mut options = Vec::new();
        option6(&mut options, 1, &duid());
        for id in 0..17u32 {
            let mut association = ia(None);
            association[..4].copy_from_slice(&id.to_be_bytes());
            option6(&mut options, 3, &association);
        }
        assert!(v6_options(&options, 1).is_none());
    }

    #[test]
    fn router_solicitation_rejects_bad_hop_limit_and_unspecified_source_link_option() {
        let nic = nic();
        let mut body = [0u8; 16];
        body[0] = 133;
        body[8..10].copy_from_slice(&[1, 1]);
        body[10..16].copy_from_slice(&nic.mac);
        let invalid = packet::icmpv6_frame(nic.mac, multicast_mac(ALL_ROUTERS), Ipv6Addr::UNSPECIFIED, ALL_ROUTERS, &body);
        assert!(is_service(&invalid));
        assert!(handle(&invalid, &nic).is_none());
        let mut valid = packet::icmpv6_frame(nic.mac, multicast_mac(ALL_ROUTERS), "fe80::2".parse().unwrap(), ALL_ROUTERS, &body);
        let response = handle(&valid, &nic).unwrap();
        assert_eq!(packet::ip_packet(&response).unwrap().destination, "fe80::2".parse::<IpAddr>().unwrap());
        valid[21] = 64;
        assert!(handle(&valid, &nic).is_none());
    }

    #[test]
    fn ntp_copies_originate_and_uses_actual_host_time_only_at_local_destination() {
        let nic = nic();
        let mut body = [0u8; 48];
        body[0] = 4 << 3 | 3;
        body[40..48].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let request =
            packet::udp_frame(nic.mac, GATEWAY_MAC, SocketAddr::from((nic.ipv4, 1234)), SocketAddr::from((nic.gateway_v4, 123)), &body);
        let response = handle(&request, &nic).unwrap();
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let packet = packet::parse(&response).unwrap();
        assert_eq!(packet.payload[0], 4 << 3 | 4);
        assert_eq!(&packet.payload[24..32], &body[40..48]);
        assert_eq!(&packet.payload[12..16], b"LOCL");
        for field in [32, 40] {
            let seconds = read_u32(&packet.payload[field..field + 4]).wrapping_sub(2_208_988_800);
            assert!((before..=after).contains(&u64::from(seconds)));
        }
        let external = packet::udp_frame(nic.mac, GATEWAY_MAC, SocketAddr::from((nic.ipv4, 1234)), "1.1.1.1:123".parse().unwrap(), &body);
        assert!(!is_service(&external));
        assert!(handle(&external, &nic).is_none());
        body[0] = 4 << 3 | 6;
        let control =
            packet::udp_frame(nic.mac, GATEWAY_MAC, SocketAddr::from((nic.ipv4, 1234)), SocketAddr::from((nic.gateway_v4, 123)), &body);
        assert!(handle(&control, &nic).is_none());
    }
}
