//! The network's only resolver: `<name>.aho` answers from this network's NICs, everything
//! else is checked against the egress policy, forwarded to the operator's upstream resolvers, and
//! validated record by record before any address reaches the guest. This is the only place that
//! creates domain → IP authority (`Policy::observe`); a denial is `REFUSED`, which the runtime
//! turns into a blocked-flow notice naming `aho net allow <name>`.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_net::{NetworkConfig, Nic};
use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{Name, RData, Record, RecordType};

use crate::policy::{Policy, normalize_domain};

/// Local answers are stable for the life of the network, but guests re-ask often enough to see a
/// newly attached peer.
const LOCAL_TTL: u32 = 60;
/// Upper bound on any TTL handed to a guest, so its cache never outlives the flow grant.
const MAX_TTL: u32 = 600;
/// Largest UDP reply sent without truncation (the DNS-over-UDP safe size, well under the MTU).
const UDP_PAYLOAD: usize = 1232;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_SUFFIX: &str = "aho";

/// A wire response for one guest query; always well-formed so the guest never waits on silence.
pub(crate) fn resolve(query: &[u8], nic: &Nic, nics: &[Nic], config: &NetworkConfig, policy: &Arc<Mutex<Policy>>, tcp: bool) -> Vec<u8> {
    let Ok(request) = Message::from_vec(query) else {
        return form_error(query);
    };
    encode(&request, &answer(&request, nic, nics, config, policy), tcp)
}

/// The wire form of `reply`; over UDP a reply too large for a datagram becomes a header-only
/// TC response so the guest retries over TCP.
fn encode(request: &Message, reply: &Message, tcp: bool) -> Vec<u8> {
    let bytes = reply.to_vec().unwrap_or_else(|_| failure(request, ResponseCode::ServFail));
    if tcp || bytes.len() <= UDP_PAYLOAD {
        return bytes;
    }
    let mut short = header_reply(request, reply.metadata.response_code);
    short.metadata.truncation = true;
    short.to_vec().unwrap_or_else(|_| failure(request, ResponseCode::ServFail))
}

/// The normalized question of a query, for the notice that names its unlock.
pub(crate) fn question_name(query: &[u8]) -> Option<String> {
    let message = Message::from_vec(query).ok()?;
    normalize_domain(&message.queries.first()?.name.to_ascii())
}

/// Whether a response is a policy refusal (QR set, RCODE 5).
pub(crate) fn refused(response: &[u8]) -> bool {
    response.len() >= 4 && response[2] & 0x80 != 0 && response[3] & 0x0f == 5
}

fn answer(request: &Message, nic: &Nic, nics: &[Nic], config: &NetworkConfig, policy: &Arc<Mutex<Policy>>) -> Message {
    if request.metadata.message_type != MessageType::Query {
        return header_reply(request, ResponseCode::FormErr);
    }
    if request.metadata.op_code != OpCode::Query {
        return header_reply(request, ResponseCode::NotImp);
    }
    let [question] = request.queries.as_slice() else {
        return header_reply(request, ResponseCode::FormErr);
    };
    let Some(name) = normalize_domain(&question.name.to_ascii()) else {
        return header_reply(request, ResponseCode::FormErr);
    };
    if let Some(label) = name.strip_suffix(&format!(".{LOCAL_SUFFIX}")) {
        return local(request, question.query_type, label, &name, nic, nics, policy);
    }
    if name == LOCAL_SUFFIX {
        return authoritative(request, ResponseCode::NXDomain, Vec::new());
    }
    match policy.lock() {
        Ok(p) if p.check_domain(&name).is_ok() => {}
        _ => return header_reply(request, ResponseCode::Refused),
    }
    let Some(upstream) = forward(request, &config.dns_upstream) else {
        return header_reply(request, ResponseCode::ServFail);
    };
    validate(request, upstream, &name, &nic.name, policy)
}

/// `<label>.aho`: this network's NICs (`gateway` is the resolver/router itself). Explicit denies
/// still apply to same-network names.
fn local(request: &Message, qtype: RecordType, label: &str, name: &str, nic: &Nic, nics: &[Nic], policy: &Arc<Mutex<Policy>>) -> Message {
    let (v4, v6) = if label == "gateway" {
        (nic.gateway_v4, nic.gateway_v6)
    } else {
        match nics.iter().find(|n| n.name.eq_ignore_ascii_case(label)) {
            Some(peer) => (peer.ipv4, peer.ipv6),
            None => return authoritative(request, ResponseCode::NXDomain, Vec::new()),
        }
    };
    let Ok(owner) = Name::from_ascii(format!("{name}.")) else {
        return header_reply(request, ResponseCode::FormErr);
    };
    let mut records = Vec::new();
    let allowed = |ip: IpAddr| policy.lock().is_ok_and(|p| p.check_local(name, ip).is_ok());
    if matches!(qtype, RecordType::A | RecordType::ANY) {
        if !allowed(IpAddr::V4(v4)) {
            return header_reply(request, ResponseCode::Refused);
        }
        records.push(Record::from_rdata(owner.clone(), LOCAL_TTL, RData::A(A(v4))));
    }
    if matches!(qtype, RecordType::AAAA | RecordType::ANY) {
        if !allowed(IpAddr::V6(v6)) {
            return header_reply(request, ResponseCode::Refused);
        }
        records.push(Record::from_rdata(owner, LOCAL_TTL, RData::AAAA(AAAA(v6))));
    }
    authoritative(request, ResponseCode::NoError, records)
}

/// Ask each upstream in order: UDP first, TCP when the answer was truncated. Only a response to
/// exactly our question, under our fresh id, is accepted.
fn forward(request: &Message, upstreams: &[SocketAddr]) -> Option<Message> {
    let mut outgoing = request.clone();
    outgoing.metadata.id = fresh_id();
    outgoing.metadata.recursion_desired = true;
    let wire = outgoing.to_vec().ok()?;
    for upstream in upstreams {
        let Some(mut response) = udp_exchange(*upstream, &wire) else { continue };
        if response.metadata.truncation {
            match tcp_exchange(*upstream, &wire) {
                Some(full) => response = full,
                None => continue,
            }
        }
        if response.metadata.id == outgoing.metadata.id
            && response.metadata.message_type == MessageType::Response
            && same_question(&response, request)
        {
            return Some(response);
        }
    }
    None
}

fn udp_exchange(upstream: SocketAddr, wire: &[u8]) -> Option<Message> {
    let bind: SocketAddr = if upstream.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.set_read_timeout(Some(UPSTREAM_TIMEOUT)).ok()?;
    socket.connect(upstream).ok()?;
    socket.send(wire).ok()?;
    let mut buffer = vec![0u8; 65_535];
    let n = socket.recv(&mut buffer).ok()?;
    Message::from_vec(&buffer[..n]).ok()
}

fn tcp_exchange(upstream: SocketAddr, wire: &[u8]) -> Option<Message> {
    let mut stream = TcpStream::connect_timeout(&upstream, UPSTREAM_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(UPSTREAM_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(UPSTREAM_TIMEOUT)).ok()?;
    let len = u16::try_from(wire.len()).ok()?;
    stream.write_all(&len.to_be_bytes()).ok()?;
    stream.write_all(wire).ok()?;
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).ok()?;
    let mut body = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
    stream.read_exact(&mut body).ok()?;
    Message::from_vec(&body).ok()
}

fn same_question(response: &Message, request: &Message) -> bool {
    let key = |m: &Message| m.queries.iter().map(|q| (q.name.to_lowercase(), q.query_type, q.query_class)).collect::<Vec<_>>();
    key(response) == key(request)
}

/// Keep only records owned by the question or its CNAME chain; every alias must pass the alias
/// check and every address the resolved-address check. Addresses that survive are granted to this
/// guest for (at most) their TTL. Drop authority/additional sections: their addresses were never
/// validated.
fn validate(request: &Message, upstream: Message, name: &str, guest: &str, policy: &Arc<Mutex<Policy>>) -> Message {
    let Ok(mut policy) = policy.lock() else {
        return header_reply(request, ResponseCode::ServFail);
    };
    let wants_address = matches!(request.queries[0].query_type, RecordType::A | RecordType::AAAA | RecordType::ANY);
    let mut chain = vec![name.to_string()];
    let (mut kept, mut denied, mut addresses) = (Vec::new(), false, Vec::new());
    for mut record in upstream.answers {
        let Some(owner) = normalize_domain(&record.name.to_ascii()) else { continue };
        if !chain.contains(&owner) {
            continue;
        }
        record.ttl = record.ttl.min(MAX_TTL);
        match &record.data {
            RData::CNAME(target) => {
                let Some(target) = normalize_domain(&target.0.to_ascii()) else { continue };
                if policy.check_alias(&target).is_err() {
                    return header_reply(request, ResponseCode::Refused);
                }
                chain.push(target);
                kept.push(record);
            }
            RData::A(A(ip)) => keep_address(&mut policy, name, IpAddr::V4(*ip), record, &mut kept, &mut addresses, &mut denied),
            RData::AAAA(AAAA(ip)) => keep_address(&mut policy, name, IpAddr::V6(*ip), record, &mut kept, &mut addresses, &mut denied),
            _ => kept.push(record),
        }
    }
    if wants_address && denied && addresses.is_empty() {
        return header_reply(request, ResponseCode::Refused);
    }
    for (ip, ttl) in addresses {
        if policy.observe(guest, name, ip, ttl).is_err() {
            return header_reply(request, ResponseCode::ServFail);
        }
    }
    let mut reply = header_reply(request, upstream.metadata.response_code);
    reply.metadata.authoritative = false;
    reply.answers = kept;
    reply.edns = upstream.edns;
    reply
}

fn keep_address(
    policy: &mut Policy,
    name: &str,
    ip: IpAddr,
    record: Record,
    kept: &mut Vec<Record>,
    addresses: &mut Vec<(IpAddr, u32)>,
    denied: &mut bool,
) {
    if policy.check_resolved(name, ip).is_ok() {
        addresses.push((ip, record.ttl));
        kept.push(record);
    } else {
        *denied = true;
    }
}

/// A response to `request` with its question echoed and no records.
fn header_reply(request: &Message, code: ResponseCode) -> Message {
    let mut reply = Message::response(request.metadata.id, request.metadata.op_code);
    reply.metadata.recursion_desired = request.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    reply.metadata.response_code = code;
    reply.queries = request.queries.clone();
    reply
}

fn authoritative(request: &Message, code: ResponseCode, answers: Vec<Record>) -> Message {
    let mut reply = header_reply(request, code);
    reply.metadata.authoritative = true;
    reply.answers = answers;
    reply
}

fn failure(request: &Message, code: ResponseCode) -> Vec<u8> {
    let [a, b] = request.metadata.id.to_be_bytes();
    vec![a, b, 0x80, 0x80 | code.low(), 0, 0, 0, 0, 0, 0, 0, 0]
}

/// A header-only FORMERR for bytes that do not parse as a message (id echoed when present).
fn form_error(query: &[u8]) -> Vec<u8> {
    let (a, b) = (query.first().copied().unwrap_or(0), query.get(1).copied().unwrap_or(0));
    vec![a, b, 0x80, 0x81, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// An upstream query id nobody on the path can predict (process-random keys, a counter and the
/// clock), so an off-path spoofer cannot race the real answer.
fn fresh_id() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish() as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_net::{Destination, EgressPolicy};
    use hickory_proto::op::Query;
    use hickory_proto::rr::rdata::CNAME;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn nic(name: &str, last: u8) -> Nic {
        Nic {
            name: name.into(),
            mac: [2, 0, 0, 0, 0, last],
            ipv4: Ipv4Addr::new(10, 85, 1, last),
            ipv6: Ipv6Addr::new(0xfd85, 1, 1, 0, 0, 0, 0, u16::from(last)),
            gateway_v4: Ipv4Addr::new(10, 85, 1, 1),
            gateway_v6: Ipv6Addr::new(0xfd85, 1, 1, 0, 0, 0, 0, 1),
            prefix_v4: 24,
            prefix_v6: 64,
        }
    }

    fn query(name: &str, qtype: RecordType) -> Message {
        let mut m = Message::new(0x4242, MessageType::Query, OpCode::Query);
        m.metadata.recursion_desired = true;
        m.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
        m
    }

    fn policy(allow: &[&str], deny: &[&str]) -> Arc<Mutex<Policy>> {
        let mut config = NetworkConfig::new("t");
        config.egress = EgressPolicy {
            allow: allow.iter().map(|d| Destination::parse(d).unwrap()).collect(),
            deny: deny.iter().map(|d| Destination::parse(d).unwrap()).collect(),
        };
        Arc::new(Mutex::new(Policy::new(&config)))
    }

    fn upstream(request: &Message, answers: Vec<Record>) -> Message {
        let mut m = header_reply(request, ResponseCode::NoError);
        m.answers = answers;
        m.additionals =
            vec![Record::from_rdata(Name::from_ascii("glue.example.").unwrap(), 300, RData::A(A(Ipv4Addr::new(93, 184, 216, 34))))];
        m
    }

    fn a(name: &str, ip: [u8; 4], ttl: u32) -> Record {
        Record::from_rdata(Name::from_ascii(name).unwrap(), ttl, RData::A(A(Ipv4Addr::from(ip))))
    }

    #[test]
    fn local_names_answer_from_this_network_and_unknown_names_are_nxdomain() {
        let (me, db) = (nic("sandbox", 2), nic("vm-db", 3));
        let nics = [me.clone(), db.clone()];
        let p = policy(&[], &[]);
        let config = NetworkConfig::new("t");
        let out =
            Message::from_vec(&resolve(&query("vm-db.aho.", RecordType::A).to_vec().unwrap(), &me, &nics, &config, &p, false)).unwrap();
        assert_eq!(out.metadata.response_code, ResponseCode::NoError);
        assert!(out.metadata.authoritative);
        assert_eq!(out.answers[0].data, RData::A(A(db.ipv4)));
        let six =
            Message::from_vec(&resolve(&query("VM-DB.aho.", RecordType::AAAA).to_vec().unwrap(), &me, &nics, &config, &p, false)).unwrap();
        assert_eq!(six.answers[0].data, RData::AAAA(AAAA(db.ipv6)), "local names are case-insensitive");
        let gw =
            Message::from_vec(&resolve(&query("gateway.aho.", RecordType::A).to_vec().unwrap(), &me, &nics, &config, &p, false)).unwrap();
        assert_eq!(gw.answers[0].data, RData::A(A(me.gateway_v4)));
        let missing =
            Message::from_vec(&resolve(&query("ghost.aho.", RecordType::A).to_vec().unwrap(), &me, &nics, &config, &p, false)).unwrap();
        assert_eq!(missing.metadata.response_code, ResponseCode::NXDomain);
    }

    #[test]
    fn a_denied_local_name_is_refused() {
        let (me, db) = (nic("sandbox", 2), nic("vm-db", 3));
        let p = policy(&[], &["vm-db.aho"]);
        let wire =
            resolve(&query("vm-db.aho.", RecordType::A).to_vec().unwrap(), &me, &[me.clone(), db], &NetworkConfig::new("t"), &p, false);
        assert!(refused(&wire));
    }

    #[test]
    fn a_domain_outside_the_allow_list_is_refused_before_any_upstream_is_asked() {
        let me = nic("sandbox", 2);
        let mut config = NetworkConfig::new("t");
        config.dns_upstream = vec![]; // asking would SERVFAIL; a refusal proves nothing was asked
        let p = policy(&["crates.io"], &[]);
        let wire = resolve(&query("evil.example.", RecordType::A).to_vec().unwrap(), &me, std::slice::from_ref(&me), &config, &p, false);
        assert!(refused(&wire));
        assert_eq!(question_name(&query("Evil.Example.", RecordType::A).to_vec().unwrap()).as_deref(), Some("evil.example"));
    }

    #[test]
    fn validated_addresses_are_granted_and_glue_and_off_chain_records_are_dropped() {
        let p = policy(&["crates.io"], &[]);
        let request = query("crates.io.", RecordType::A);
        let answers = vec![
            Record::from_rdata(
                Name::from_ascii("crates.io.").unwrap(),
                300,
                RData::CNAME(CNAME(Name::from_ascii("cdn.crates.io.").unwrap())),
            ),
            a("cdn.crates.io.", [93, 184, 216, 34], 86_400),
            a("unrelated.example.", [93, 184, 216, 35], 300),
        ];
        let reply = validate(&request, upstream(&request, answers), "crates.io", "sandbox", &p);
        assert_eq!(reply.metadata.response_code, ResponseCode::NoError);
        assert_eq!(reply.answers.len(), 2, "the off-chain record is dropped");
        assert!(reply.additionals.is_empty(), "unvalidated glue never reaches the guest");
        assert!(reply.answers.iter().all(|r| r.ttl <= MAX_TTL), "TTLs are capped at the grant lifetime");
        let p = p.lock().unwrap();
        assert!(p.check("sandbox", IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), None).is_ok(), "the guest may now connect");
        assert!(p.check("other-guest", IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), None).is_err(), "grants are per guest");
        assert!(p.check("sandbox", IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)), None).is_err(), "dropped records grant nothing");
    }

    #[test]
    fn a_denied_alias_or_lan_answer_is_refused() {
        let p = policy(&["crates.io"], &["tracker.example"]);
        let request = query("crates.io.", RecordType::A);
        let laundered = vec![Record::from_rdata(
            Name::from_ascii("crates.io.").unwrap(),
            300,
            RData::CNAME(CNAME(Name::from_ascii("tracker.example.").unwrap())),
        )];
        assert_eq!(
            validate(&request, upstream(&request, laundered), "crates.io", "sandbox", &p).metadata.response_code,
            ResponseCode::Refused
        );
        let rebound = vec![a("crates.io.", [192, 168, 1, 10], 300)];
        assert_eq!(
            validate(&request, upstream(&request, rebound), "crates.io", "sandbox", &p).metadata.response_code,
            ResponseCode::Refused,
            "DNS rebinding to the host LAN is refused"
        );
    }

    #[test]
    fn oversized_udp_replies_are_truncated_but_tcp_replies_are_not() {
        let request = query("crates.io.", RecordType::A);
        let many: Vec<Record> = (0..120).map(|i| a("crates.io.", [93, 184, 216, i as u8], 300)).collect();
        let reply = validate(&request, upstream(&request, many), "crates.io", "sandbox", &policy(&[], &[]));
        let udp = Message::from_vec(&encode(&request, &reply, false)).unwrap();
        assert!(udp.metadata.truncation && udp.answers.is_empty(), "the guest is told to retry over TCP");
        assert_eq!(udp.metadata.id, request.metadata.id);
        let tcp = Message::from_vec(&encode(&request, &reply, true)).unwrap();
        assert!(!tcp.metadata.truncation);
        assert_eq!(tcp.answers.len(), 120);
    }

    #[test]
    fn malformed_queries_get_a_formerr_with_their_id() {
        let me = nic("sandbox", 2);
        let wire = resolve(&[0xab, 0xcd, 0x01], &me, std::slice::from_ref(&me), &NetworkConfig::new("t"), &policy(&[], &[]), false);
        assert_eq!(&wire[..4], &[0xab, 0xcd, 0x80, 0x81]);
        assert!(!refused(&wire));
    }
}
