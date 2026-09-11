use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use aho_net::{MAX_FRAME, Transport};

const ETHERNET: usize = 14;
const MTU: usize = 1500;

pub(crate) struct Packet<'a> {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub protocol: Transport,
    pub payload: &'a [u8],
    pub tcp_syn: bool,
}

pub(crate) struct IpPacket<'a> {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub protocol: u8,
    pub hop_limit: u8,
    pub payload: &'a [u8],
}

/// This boundary deliberately accepts neither IP options nor extension headers.
/// In particular, fragments never reach transport parsing or the policy engine.
pub(crate) fn ip_packet(frame: &[u8]) -> Option<IpPacket<'_>> {
    if !(ETHERNET..=MAX_FRAME).contains(&frame.len()) {
        return None;
    }
    let bytes = &frame[ETHERNET..];
    match u16::from_be_bytes([frame[12], frame[13]]) {
        0x0800 => {
            if bytes.len() < 20 || bytes[0] != 0x45 {
                return None;
            }
            let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
            let flags_offset = u16::from_be_bytes([bytes[6], bytes[7]]);
            if !(20..=MTU).contains(&length)
                || length > bytes.len()
                || flags_offset & !0x4000 != 0
                || bytes[8] == 0
                || checksum(&bytes[..20]) != 0
            {
                return None;
            }
            Some(IpPacket {
                source: Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]).into(),
                destination: Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]).into(),
                protocol: bytes[9],
                hop_limit: bytes[8],
                payload: &bytes[20..length],
            })
        }
        0x86dd => {
            if bytes.len() < 40 || bytes[0] >> 4 != 6 {
                return None;
            }
            let length = 40 + usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
            if length > MTU || length > bytes.len() || bytes[7] == 0 {
                return None;
            }
            // Only terminal headers used here are accepted; this also refuses
            // atomic fragments, routing headers, ESP/AH and jumbograms.
            if !matches!(bytes[6], 6 | 17 | 58) {
                return None;
            }
            Some(IpPacket {
                source: Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).ok()?).into(),
                destination: Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[24..40]).ok()?).into(),
                protocol: bytes[6],
                hop_limit: bytes[7],
                payload: &bytes[40..length],
            })
        }
        _ => None,
    }
}

pub(crate) fn parse(frame: &[u8]) -> Option<Packet<'_>> {
    let ip = ip_packet(frame)?;
    let bytes = ip.payload;
    let (protocol, header, tcp_syn) = match ip.protocol {
        17 => {
            if bytes.len() < 8 || usize::from(u16::from_be_bytes([bytes[4], bytes[5]])) != bytes.len() {
                return None;
            }
            let omitted = bytes[6] == 0 && bytes[7] == 0;
            if (omitted && ip.source.is_ipv6()) || (!omitted && transport_checksum(ip.source, ip.destination, 17, bytes) != 0) {
                return None;
            }
            (Transport::Udp, 8, false)
        }
        6 => {
            if bytes.len() < 20 || bytes[12] & 0x0e != 0 {
                return None;
            }
            let header = usize::from(bytes[12] >> 4) * 4;
            if !(20..=60).contains(&header)
                || header > bytes.len()
                || transport_checksum(ip.source, ip.destination, 6, bytes) != 0
                || !valid_tcp_options(&bytes[20..header])
                || bytes[0..2] == [0, 0]
            {
                return None;
            }
            (Transport::Tcp, header, bytes[13] & 0x02 != 0)
        }
        _ => return None,
    };
    let source_port = u16::from_be_bytes([bytes[0], bytes[1]]);
    let destination_port = u16::from_be_bytes([bytes[2], bytes[3]]);
    if destination_port == 0 {
        return None;
    }
    Some(Packet {
        source: SocketAddr::new(ip.source, source_port),
        destination: SocketAddr::new(ip.destination, destination_port),
        protocol,
        payload: &bytes[header..],
        tcp_syn,
    })
}

fn valid_tcp_options(mut options: &[u8]) -> bool {
    while let Some(&kind) = options.first() {
        if kind == 0 {
            return options[1..].iter().all(|&byte| byte == 0);
        }
        if kind == 1 {
            options = &options[1..];
            continue;
        }
        let Some(&length) = options.get(1) else { return false };
        let length = usize::from(length);
        if length < 2 || length > options.len() {
            return false;
        }
        let valid = match kind {
            2 => length == 4,
            3 => length == 3,
            4 => length == 2,
            5 => length >= 10 && (length - 2) % 8 == 0,
            8 => length == 10,
            _ => true,
        };
        if !valid {
            return false;
        }
        options = &options[length..];
    }
    true
}

fn sum(mut accumulator: u32, bytes: &[u8]) -> u32 {
    let (pairs, rest) = bytes.as_chunks::<2>();
    for pair in pairs {
        accumulator += u32::from(u16::from_be_bytes(*pair));
    }
    if let Some(&last) = rest.first() {
        accumulator += u32::from(last) << 8;
    }
    accumulator
}

fn finish(mut accumulator: u32) -> u16 {
    while accumulator >> 16 != 0 {
        accumulator = (accumulator & 0xffff) + (accumulator >> 16);
    }
    !(accumulator as u16)
}

pub(crate) fn checksum(bytes: &[u8]) -> u16 {
    finish(sum(0, bytes))
}

pub(crate) fn transport_checksum(source: IpAddr, destination: IpAddr, protocol: u8, bytes: &[u8]) -> u16 {
    let accumulator = match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => sum(sum(0, &source.octets()), &destination.octets()),
        (IpAddr::V6(source), IpAddr::V6(destination)) => sum(sum(0, &source.octets()), &destination.octets()),
        _ => return u16::MAX,
    };
    finish(sum(accumulator + u32::from(protocol) + bytes.len() as u32, bytes))
}

fn frame_header(
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    hop_limit: u8,
    payload_length: usize,
) -> Option<(Vec<u8>, usize)> {
    let header = match (source, destination) {
        (IpAddr::V4(_), IpAddr::V4(_)) => 20,
        (IpAddr::V6(_), IpAddr::V6(_)) => 40,
        _ => return None,
    };
    if payload_length > MTU - header {
        return None;
    }
    let mut frame = vec![0; ETHERNET + header + payload_length];
    frame[..6].copy_from_slice(&destination_mac);
    frame[6..12].copy_from_slice(&source_mac);
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            let ip = &mut frame[ETHERNET..ETHERNET + header];
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&((header + payload_length) as u16).to_be_bytes());
            ip[6] = 0x40;
            ip[8] = hop_limit;
            ip[9] = protocol;
            ip[12..16].copy_from_slice(&source.octets());
            ip[16..20].copy_from_slice(&destination.octets());
            let value = checksum(ip);
            ip[10..12].copy_from_slice(&value.to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            let ip = &mut frame[ETHERNET..ETHERNET + header];
            ip[0] = 0x60;
            ip[4..6].copy_from_slice(&(payload_length as u16).to_be_bytes());
            ip[6] = protocol;
            ip[7] = hop_limit;
            ip[8..24].copy_from_slice(&source.octets());
            ip[24..40].copy_from_slice(&destination.octets());
        }
        _ => return None,
    }
    Some((frame, ETHERNET + header))
}

/// Returns an empty frame for a family mismatch or an over-MTU datagram.
pub(crate) fn udp_frame(
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
    source: SocketAddr,
    destination: SocketAddr,
    payload: &[u8],
) -> Vec<u8> {
    let Some(length) = payload.len().checked_add(8) else { return Vec::new() };
    let Some((mut frame, offset)) = frame_header(source_mac, destination_mac, source.ip(), destination.ip(), 17, 64, length) else {
        return Vec::new();
    };
    let udp = &mut frame[offset..];
    udp[..2].copy_from_slice(&source.port().to_be_bytes());
    udp[2..4].copy_from_slice(&destination.port().to_be_bytes());
    udp[4..6].copy_from_slice(&(length as u16).to_be_bytes());
    udp[8..].copy_from_slice(payload);
    let value = transport_checksum(source.ip(), destination.ip(), 17, udp);
    udp[6..8].copy_from_slice(&if value == 0 { u16::MAX } else { value }.to_be_bytes());
    frame
}

pub(crate) fn icmpv6_frame(
    source_mac: [u8; 6],
    destination_mac: [u8; 6],
    source: Ipv6Addr,
    destination: Ipv6Addr,
    payload: &[u8],
) -> Vec<u8> {
    if payload.len() < 4 {
        return Vec::new();
    }
    let Some((mut frame, offset)) = frame_header(source_mac, destination_mac, source.into(), destination.into(), 58, 255, payload.len())
    else {
        return Vec::new();
    };
    let icmp = &mut frame[offset..];
    icmp.copy_from_slice(payload);
    icmp[2..4].fill(0);
    let value = transport_checksum(source.into(), destination.into(), 58, icmp);
    icmp[2..4].copy_from_slice(&value.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(v6: bool) -> Vec<u8> {
        let (source, destination) = if v6 { ("[fd00::2]:1234", "[fd00::1]:4321") } else { ("10.0.0.2:1234", "10.0.0.1:4321") };
        udp_frame([2; 6], [4; 6], source.parse().unwrap(), destination.parse().unwrap(), b"odd")
    }

    fn repair_v4(frame: &mut [u8]) {
        frame[24..26].fill(0);
        let value = checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&value.to_be_bytes());
    }

    #[test]
    fn internet_checksum_matches_rfc_1071_vector() {
        assert_eq!(checksum(&[0, 1, 0xf2, 3, 0xf4, 0xf5, 0xf6, 0xf7]), 0x220d);
    }

    #[test]
    fn odd_datagrams_verify_both_pseudoheaders_and_detect_corruption() {
        for v6 in [false, true] {
            let mut frame = datagram(v6);
            let packet = parse(&frame).unwrap();
            assert_eq!(packet.payload, b"odd");
            assert_eq!(packet.destination.port(), 4321);
            *frame.last_mut().unwrap() ^= 1;
            assert!(parse(&frame).is_none());
        }
    }

    #[test]
    fn every_truncated_datagram_is_rejected() {
        for v6 in [false, true] {
            let frame = datagram(v6);
            for end in 0..frame.len() {
                assert!(parse(&frame[..end]).is_none(), "v6={v6}, length={end}");
            }
        }
    }

    #[test]
    fn fragments_options_and_ipv6_extensions_never_reach_transport() {
        for bits in [0x2000u16, 1, 0x8000, 0x6000] {
            let mut frame = datagram(false);
            frame[20..22].copy_from_slice(&bits.to_be_bytes());
            repair_v4(&mut frame);
            assert!(parse(&frame).is_none());
        }
        let mut options = datagram(false);
        options[14] = 0x46;
        repair_v4(&mut options);
        assert!(parse(&options).is_none());
        for next in [0, 43, 44, 50, 51, 60, 59] {
            let mut frame = datagram(true);
            frame[20] = next;
            assert!(parse(&frame).is_none());
        }
    }

    #[test]
    fn ipv4_only_may_omit_udp_checksum_and_lengths_must_match() {
        let mut v4 = datagram(false);
        v4[40..42].fill(0);
        assert!(parse(&v4).is_some());
        v4[38..40].copy_from_slice(&8u16.to_be_bytes());
        assert!(parse(&v4).is_none());
        let mut v6 = datagram(true);
        v6[60..62].fill(0);
        assert!(parse(&v6).is_none());
    }

    #[test]
    fn tcp_requires_valid_checksum_header_and_options() {
        for (source, destination) in [("10.0.0.2", "10.0.0.1"), ("fd00::2", "fd00::1")] {
            let source: IpAddr = source.parse().unwrap();
            let destination: IpAddr = destination.parse().unwrap();
            let (mut frame, offset) = frame_header([2; 6], [4; 6], source, destination, 6, 64, 27).unwrap();
            let tcp = &mut frame[offset..];
            tcp[..2].copy_from_slice(&1234u16.to_be_bytes());
            tcp[2..4].copy_from_slice(&443u16.to_be_bytes());
            tcp[12] = 0x60;
            tcp[13] = 2;
            tcp[20..24].copy_from_slice(&[2, 4, 5, 180]);
            tcp[24..].copy_from_slice(b"syn");
            let value = transport_checksum(source, destination, 6, tcp);
            tcp[16..18].copy_from_slice(&value.to_be_bytes());
            let parsed = parse(&frame).unwrap();
            assert!(parsed.tcp_syn);
            assert_eq!(parsed.payload, b"syn");
            frame[offset + 14] ^= 1;
            assert!(parse(&frame).is_none());
            frame[offset + 14] ^= 1;
            frame[offset + 21] = 1;
            frame[offset + 16..offset + 18].fill(0);
            let value = transport_checksum(source, destination, 6, &frame[offset..]);
            frame[offset + 16..offset + 18].copy_from_slice(&value.to_be_bytes());
            assert!(parse(&frame).is_none());
        }
    }

    #[test]
    fn emitter_enforces_mtu_and_address_family_before_allocation() {
        let source: SocketAddr = "[fd00::2]:1000".parse().unwrap();
        let destination: SocketAddr = "[fd00::1]:2000".parse().unwrap();
        assert!(parse(&udp_frame([2; 6], [4; 6], source, destination, &[0; 1452])).is_some());
        assert!(udp_frame([2; 6], [4; 6], source, destination, &[0; 1453]).is_empty());
        assert!(udp_frame([2; 6], [4; 6], source, "10.0.0.1:2000".parse().unwrap(), &[]).is_empty());
    }
}
