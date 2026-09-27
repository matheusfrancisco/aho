//! End to end over the NIC socket: a real userspace guest stack (smoltcp) attaches to the
//! network exactly as QEMU or the namespace NIC would (u32 big-endian length, then an Ethernet
//! frame) and leases, resolves, connects out through the NAT, gets refused, and is reached
//! through a port map. No hypervisor, TAP device or root is involved.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use aho_net::{Access, Destination, FlowEvent, Network, NetworkConfig, Nic, PortMap, Transport};
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use ipnet::{Ipv4Net, Ipv6Net};
use net_userspace::UserspaceNetwork;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint};

const DEADLINE: Duration = Duration::from_secs(10);

// ───────────────────────────── the guest ─────────────────────────────

/// The guest end of an `Attachment`: QEMU's socket framing over the unix stream.
struct Wire {
    stream: UnixStream,
    buffer: Vec<u8>,
    frames: VecDeque<Vec<u8>>,
}

impl Wire {
    fn new(stream: UnixStream) -> Wire {
        stream.set_nonblocking(true).unwrap();
        Wire { stream, buffer: Vec::new(), frames: VecDeque::new() }
    }
    fn pump(&mut self) {
        let mut chunk = [0u8; 65536];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("guest wire: {e}"),
            }
        }
        while self.buffer.len() >= 4 {
            let len = u32::from_be_bytes(self.buffer[..4].try_into().unwrap()) as usize;
            if self.buffer.len() < 4 + len {
                break;
            }
            self.frames.push_back(self.buffer[4..4 + len].to_vec());
            self.buffer.drain(..4 + len);
        }
    }
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut UnixStream);

impl Device for Wire {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        self.pump();
        let frame = self.frames.pop_front()?;
        Some((Rx(frame), Tx(&mut self.stream)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.stream))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let r = f(&mut frame);
        let mut out = (len as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&frame);
        // The network drains its side every couple of milliseconds; a full buffer is transient.
        let mut sent = 0;
        while sent < out.len() {
            match self.0.write(&out[sent..]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => panic!("guest wire: {e}"),
            }
        }
        r
    }
}

struct Guest {
    wire: Wire,
    iface: Interface,
    sockets: SocketSet<'static>,
    nic: Nic,
}

impl Guest {
    fn attach(net: &UserspaceNetwork, name: &str) -> Guest {
        let a = net.attach(name).unwrap();
        let mut wire = Wire::new(a.stream);
        let iface = Interface::new(Config::new(EthernetAddress(a.nic.mac).into()), &mut wire, Instant::now());
        Guest { wire, iface, sockets: SocketSet::new(vec![]), nic: a.nic }
    }
    fn poll(&mut self) {
        self.iface.poll(Instant::now(), &mut self.wire, &mut self.sockets);
        std::thread::sleep(Duration::from_millis(1));
    }
    /// Runs the stack until `done` returns `Some`, or panics with `what` at the deadline.
    fn until<T>(&mut self, what: &str, mut done: impl FnMut(&mut Guest) -> Option<T>) -> T {
        let start = StdInstant::now();
        loop {
            self.poll();
            if let Some(v) = done(self) {
                return v;
            }
            assert!(start.elapsed() < DEADLINE, "guest timed out waiting for {what}");
        }
    }
    /// DHCPv4 to completion; the lease is applied to the interface.
    fn lease(&mut self) -> dhcpv4::Config<'static> {
        let handle = self.sockets.add(dhcpv4::Socket::new());
        let config = self.until("a DHCP lease", |g| match g.sockets.get_mut::<dhcpv4::Socket>(handle).poll() {
            Some(dhcpv4::Event::Configured(c)) => Some(dhcpv4::Config {
                server: c.server,
                address: c.address,
                router: c.router,
                dns_servers: c.dns_servers.clone(),
                packet: None,
            }),
            _ => None,
        });
        self.iface.update_ip_addrs(|a| a.push(IpCidr::Ipv4(config.address)).unwrap());
        if let Some(router) = config.router {
            self.iface.routes_mut().add_default_ipv4_route(router).unwrap();
        }
        config
    }
    /// One DNS question over UDP to the gateway resolver.
    fn ask(&mut self, name: &str) -> Message {
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
        let handle = self.sockets.add(udp::Socket::new(rx, tx));
        self.sockets.get_mut::<udp::Socket>(handle).bind(40_053).unwrap();
        let mut q = Message::new(0x5151, MessageType::Query, OpCode::Query);
        q.metadata.recursion_desired = true;
        q.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
        let gateway = IpEndpoint::new(IpAddress::Ipv4(self.nic.gateway_v4), 53);
        self.sockets.get_mut::<udp::Socket>(handle).send_slice(&q.to_vec().unwrap(), gateway).unwrap();
        let bytes = self.until("a DNS answer", |g| g.sockets.get_mut::<udp::Socket>(handle).recv().ok().map(|(b, _)| b.to_vec()));
        self.sockets.remove(handle);
        Message::from_vec(&bytes).unwrap()
    }
    fn tcp(&mut self) -> SocketHandle {
        let socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 8192]), tcp::SocketBuffer::new(vec![0; 8192]));
        self.sockets.add(socket)
    }
    /// `send` over an established socket, then read until `expect.len()` bytes came back.
    fn echo(&mut self, handle: SocketHandle, send: &[u8]) -> Vec<u8> {
        self.until("the connection", |g| g.sockets.get::<tcp::Socket>(handle).may_send().then_some(()));
        self.sockets.get_mut::<tcp::Socket>(handle).send_slice(send).unwrap();
        let mut got = Vec::new();
        self.until("the echo", |g| {
            let s = g.sockets.get_mut::<tcp::Socket>(handle);
            if s.can_recv() {
                s.recv(|d| {
                    got.extend_from_slice(d);
                    (d.len(), ())
                })
                .unwrap();
            }
            (got.len() >= send.len()).then_some(())
        });
        got
    }
}

// ───────────────────────────── host-side fixtures ─────────────────────────────

/// A private /24 + /64 per test: the in-process network registry refuses colliding prefixes.
fn config(id: &str, third_octet: u8, upstream: SocketAddr) -> NetworkConfig {
    let mut c = NetworkConfig::new(id);
    c.ipv4 = Ipv4Net::new(Ipv4Addr::new(10, 86, third_octet, 0), 24).unwrap();
    c.ipv6 = Ipv6Net::new(Ipv6Addr::new(0xfd86, u16::from(third_octet), 0, 0, 0, 0, 0, 0), 64).unwrap();
    c.dns_upstream = vec![upstream];
    c
}
/// The network keeps its state in a private directory (it refuses anything but 0700).
fn state_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("aho-net-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A non-loopback IPv4 address of this host (host-LAN class), if it has one.
fn lan_ip() -> Option<Ipv4Addr> {
    if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| match i.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
        _ => None,
    })
}

/// Echoes one connection's bytes back, forever (thread per connection).
fn echo_server(bind: Ipv4Addr) -> SocketAddr {
    let listener = TcpListener::bind((bind, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut conn = conn;
                let mut buf = [0u8; 4096];
                while let Ok(n) = conn.read(&mut buf) {
                    if n == 0 || conn.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Whether this host itself can round-trip bytes to `addr` (a host firewall such as the macOS
/// Application Firewall holds inbound LAN connections to unsigned test binaries; loopback is exempt).
fn host_reaches(addr: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut b = [0u8; 5];
    s.write_all(b"probe").is_ok() && s.read_exact(&mut b).is_ok() && &b == b"probe"
}

/// The operator's "upstream" resolver: `svc.test` → `answer`, anything else NXDOMAIN.
fn upstream_dns(answer: Ipv4Addr) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok((n, peer)) = socket.recv_from(&mut buf) {
            let Ok(q) = Message::from_vec(&buf[..n]) else { continue };
            let mut r = Message::response(q.metadata.id, q.metadata.op_code);
            r.queries = q.queries.clone();
            if q.queries.first().is_some_and(|x| x.name.to_ascii().eq_ignore_ascii_case("svc.test.")) {
                r.add_answer(Record::from_rdata(Name::from_ascii("svc.test.").unwrap(), 300, RData::A(A(answer))));
            } else {
                r.metadata.response_code = ResponseCode::NXDomain;
            }
            let _ = socket.send_to(&r.to_vec().unwrap(), peer);
        }
    });
    addr
}

fn sink(net: &UserspaceNetwork) -> Arc<Mutex<Vec<FlowEvent>>> {
    let seen: Arc<Mutex<Vec<FlowEvent>>> = Arc::default();
    let s = seen.clone();
    net.set_flow_sink(Some(Arc::new(move |e| s.lock().unwrap().push(e)))).unwrap();
    seen
}

fn address(m: &Message) -> Option<Ipv4Addr> {
    m.answers.iter().find_map(|r| match r.data {
        RData::A(A(ip)) => Some(ip),
        _ => None,
    })
}

// ───────────────────────────── scenarios ─────────────────────────────

#[test]
fn a_guest_leases_resolves_and_reaches_an_allowed_host_through_the_nat() {
    let Some(lan) = lan_ip() else {
        eprintln!("skipped: this host has no non-loopback IPv4 address to stand in for the LAN");
        return;
    };
    let target = echo_server(lan);
    if !host_reaches(target) {
        eprintln!(
            "skipped: this host cannot reach its own LAN listener {target} (host firewall); run on Linux to exercise the NAT data path"
        );
        return;
    }
    let mut c = config("guest-nat", 11, upstream_dns(lan));
    c.isolation.host_lan = Access::Allow;
    let net = UserspaceNetwork::open(state_dir("nat"), c).unwrap();
    let events = sink(&net);
    let peer = net.attach("vm-db").unwrap().nic;
    let mut g = Guest::attach(&net, "sandbox");

    let lease = g.lease();
    assert_eq!(lease.address, smoltcp::wire::Ipv4Cidr::new(g.nic.ipv4, 24), "DHCP hands out the NIC's stable address");
    assert_eq!(lease.router, Some(g.nic.gateway_v4));
    assert!(lease.dns_servers.contains(&g.nic.gateway_v4), "the only resolver is the network's own");

    assert_eq!(address(&g.ask("vm-db.aho.")), Some(peer.ipv4), "peers are found by name");
    assert_eq!(address(&g.ask("svc.test.")), Some(lan), "other names go to the operator's upstream");

    let h = g.tcp();
    let local = IpEndpoint::new(IpAddress::Ipv4(g.nic.ipv4), 41_000);
    let remote = IpEndpoint::new(IpAddress::Ipv4(lan), target.port());
    g.sockets.get_mut::<tcp::Socket>(h).connect(g.iface.context(), remote, local).unwrap();
    assert_eq!(g.echo(h, b"through the nat"), b"through the nat");
    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| e.allowed && e.guest == "sandbox" && e.destination == SocketAddr::new(IpAddr::V4(lan), target.port())),
        "the flow is reported: {events:?}"
    );
}

#[test]
fn the_host_lan_is_denied_by_default_and_the_block_names_its_unlock() {
    let Some(lan) = lan_ip() else {
        eprintln!("skipped: this host has no non-loopback IPv4 address to stand in for the LAN");
        return;
    };
    let target = echo_server(lan);
    let net = UserspaceNetwork::open(state_dir("lan"), config("guest-lan", 12, upstream_dns(lan))).unwrap();
    let events = sink(&net);
    let mut g = Guest::attach(&net, "sandbox");
    g.lease();
    let h = g.tcp();
    let local = IpEndpoint::new(IpAddress::Ipv4(g.nic.ipv4), 41_001);
    g.sockets.get_mut::<tcp::Socket>(h).connect(g.iface.context(), IpEndpoint::new(IpAddress::Ipv4(lan), target.port()), local).unwrap();
    let blocked = g.until("the block to be reported", |_| {
        events.lock().unwrap().iter().find(|e| !e.allowed && e.destination.ip() == IpAddr::V4(lan)).cloned()
    });
    // Isolation, not the egress list, blocked it: the unlock names the class to open.
    assert_eq!(blocked.unlock.as_deref(), Some("aho net allow host-lan"));
    for _ in 0..300 {
        g.poll();
    }
    assert!(!g.sockets.get::<tcp::Socket>(h).may_send(), "no connection to the host LAN was ever established");
}

#[test]
fn a_name_outside_the_allow_list_is_refused_and_reported() {
    let mut c = config("guest-dns", 13, upstream_dns(Ipv4Addr::new(10, 1, 2, 3)));
    c.egress.allow = vec![Destination::parse("crates.io").unwrap()];
    let net = UserspaceNetwork::open(state_dir("dns"), c).unwrap();
    let events = sink(&net);
    let mut g = Guest::attach(&net, "sandbox");
    g.lease();
    let answer = g.ask("svc.test.");
    assert_eq!(answer.metadata.response_code, ResponseCode::Refused);
    assert!(answer.answers.is_empty());
    let blocked = g.until("the refusal to be reported", |_| events.lock().unwrap().iter().find(|e| !e.allowed).cloned());
    assert_eq!(blocked.unlock.as_deref(), Some("aho net allow svc.test"));
}

#[test]
fn a_port_map_is_the_inbound_path_to_a_guest_service() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = probe.local_addr().unwrap();
    drop(probe);
    let mut c = config("guest-map", 14, upstream_dns(Ipv4Addr::new(10, 1, 2, 3)));
    c.port_maps = vec![PortMap { protocol: Transport::Tcp, host, guest: "sandbox".into(), guest_port: 8080 }];
    let net = UserspaceNetwork::open(state_dir("map"), c).unwrap();
    let mut g = Guest::attach(&net, "sandbox");
    g.lease();
    let h = g.tcp();
    g.sockets.get_mut::<tcp::Socket>(h).listen(8080).unwrap();
    let client = std::thread::spawn(move || {
        let mut s = TcpStream::connect_timeout(&host, DEADLINE).unwrap();
        s.set_read_timeout(Some(DEADLINE)).unwrap();
        s.write_all(b"hello guest").unwrap();
        let mut buf = [0u8; 32];
        let n = s.read(&mut buf).unwrap();
        buf[..n].to_vec()
    });
    // The guest service echoes whatever arrives.
    let mut got = Vec::new();
    g.until("the mapped connection's bytes", |g| {
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        if s.can_recv() {
            s.recv(|d| {
                got.extend_from_slice(d);
                (d.len(), ())
            })
            .unwrap();
        }
        (got.len() >= 11).then_some(())
    });
    g.sockets.get_mut::<tcp::Socket>(h).send_slice(&got).unwrap();
    let reply = loop {
        g.poll();
        if client.is_finished() {
            break client.join().unwrap();
        }
    };
    assert_eq!(reply, b"hello guest");
}
