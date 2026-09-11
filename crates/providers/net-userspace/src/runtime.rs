use crate::{
    device::EthernetDevice,
    dns, packet,
    policy::Policy,
    services,
    store::{self, Saved, Store},
};
use aho_net::{
    Access, Attachment, FlowEvent, FlowSink, GATEWAY_MAC, MAX_FRAME, NetError, NetworkConfig, NetworkStatus, Nic, PortMap, Transport,
};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    socket::tcp,
    time::Instant as StackInstant,
    wire::{EthernetAddress, IpCidr, IpEndpoint},
};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc, LazyLock, Mutex,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) enum Command {
    Attach(String, Sender<Result<Attachment, NetError>>),
    Status(Sender<Result<NetworkStatus, NetError>>),
    Configure(NetworkConfig, Sender<Result<(), NetError>>),
    Sink(Option<FlowSink>, Sender<Result<(), NetError>>),
    CrossFrame { network: String, frame: Vec<u8> },
    Stop,
}
#[derive(Clone)]
struct Peer {
    config: NetworkConfig,
    nics: Vec<Nic>,
    tx: SyncSender<Command>,
}
fn fabric() -> &'static Mutex<HashMap<String, Peer>> {
    static FABRIC: LazyLock<Mutex<HashMap<String, Peer>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    &FABRIC
}

pub(crate) fn start(path: &Path, config: NetworkConfig) -> Result<(SyncSender<Command>, thread::JoinHandle<()>), NetError> {
    store::validate(&config)?;
    let (store, saved) = Store::open(path, config)?;
    store::validate(&saved.config)?;
    for (index, nic) in saved.nics.iter().enumerate() {
        if *nic != store::allocate(&saved.config, &nic.name, index)? || saved.nics[..index].iter().any(|n| n.name == nic.name) {
            return Err(NetError::Invalid("corrupt persisted NIC allocation".into()));
        }
    }
    let (tx, rx) = mpsc::sync_channel(256);
    let runtime = Runtime::new(store, saved, tx.clone())?;
    let id = runtime.saved.config.id.clone();
    {
        let mut peers = fabric().lock().map_err(|_| NetError::Unavailable("network registry poisoned".into()))?;
        if peers.contains_key(&id)
            || peers.values().any(|p| p.config.ipv4 == runtime.saved.config.ipv4 || p.config.ipv6 == runtime.saved.config.ipv6)
        {
            return Err(NetError::Invalid("active network id/subnets collide; select distinct private prefixes".into()));
        }
        peers.insert(id.clone(), Peer { config: runtime.saved.config.clone(), nics: runtime.saved.nics.clone(), tx: tx.clone() });
    }
    match thread::Builder::new().name(format!("aho-net-{id}")).spawn(move || runtime.run(rx)) {
        Ok(handle) => Ok((tx, handle)),
        Err(e) => {
            if let Ok(mut peers) = fabric().lock() {
                peers.remove(&id);
            }
            Err(e.into())
        }
    }
}

struct Port {
    nic: Nic,
    stream: UnixStream,
    header: [u8; 4],
    header_len: usize,
    frame: Vec<u8>,
    frame_len: usize,
    pending: VecDeque<(Vec<u8>, usize)>,
    dead: bool,
}
impl Port {
    fn new(nic: Nic, stream: UnixStream) -> Self {
        Self { nic, stream, header: [0; 4], header_len: 0, frame: vec![], frame_len: 0, pending: VecDeque::new(), dead: false }
    }
    fn receive(&mut self) -> io::Result<Option<Vec<u8>>> {
        while self.header_len < 4 {
            match self.stream.read(&mut self.header[self.header_len..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.header_len += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.frame.is_empty() {
            let len = u32::from_be_bytes(self.header) as usize;
            if !(14..=MAX_FRAME).contains(&len) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.frame.resize(len, 0);
        }
        while self.frame_len < self.frame.len() {
            match self.stream.read(&mut self.frame[self.frame_len..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.frame_len += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.header_len = 0;
        self.frame_len = 0;
        Ok(Some(std::mem::take(&mut self.frame)))
    }
    fn send(&mut self, frame: &[u8]) {
        if frame.is_empty() || frame.len() > MAX_FRAME {
            return;
        }
        if self.pending.len() >= 256 {
            self.dead = true;
            return;
        }
        let mut encoded = Vec::with_capacity(4 + frame.len());
        encoded.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        encoded.extend_from_slice(frame);
        self.pending.push_back((encoded, 0));
    }
    fn flush(&mut self) {
        while let Some((bytes, offset)) = self.pending.front_mut() {
            match self.stream.write(&bytes[*offset..]) {
                Ok(0) => {
                    self.dead = true;
                    break;
                }
                Ok(n) => {
                    *offset += n;
                    if *offset == bytes.len() {
                        self.pending.pop_front();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
    }
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    guest: String,
    protocol: Transport,
    source: SocketAddr,
    destination: SocketAddr,
}
struct TcpFlow {
    key: Key,
    handle: SocketHandle,
    host: TcpStream,
    connecting: bool,
    read_eof: bool,
    write_eof: bool,
    touched: Instant,
}
struct UdpFlow {
    socket: UdpSocket,
    mac: [u8; 6],
    touched: Instant,
}
enum Listener {
    Tcp(TcpListener),
    Udp(Arc<UdpSocket>),
}
struct Mapping {
    config: PortMap,
    listener: Listener,
}
struct UdpInbound {
    guest: String,
    source: SocketAddr,
    destination: SocketAddr,
    host: Arc<UdpSocket>,
    peer: SocketAddr,
    touched: Instant,
}
struct DnsReply {
    generation: u64,
    guest: String,
    mac: [u8; 6],
    source: SocketAddr,
    destination: SocketAddr,
    response: Vec<u8>,
    tcp: Option<SocketHandle>,
    question: Option<String>,
}
struct DnsTcp {
    key: Key,
    handle: SocketHandle,
    received: Vec<u8>,
    pending: bool,
    response: Vec<u8>,
    sent: usize,
    touched: Instant,
}

struct Runtime {
    saved: Saved,
    store: Store,
    tx: SyncSender<Command>,
    ports: HashMap<String, Port>,
    device: EthernetDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    tcp: Vec<TcpFlow>,
    dns_tcp: Vec<DnsTcp>,
    udp: HashMap<Key, UdpFlow>,
    maps: Vec<Mapping>,
    inbound_udp: Vec<UdpInbound>,
    policy: Arc<Mutex<Policy>>,
    sink: Option<FlowSink>,
    events: HashMap<Key, Instant>,
    dns_tx: Sender<DnsReply>,
    dns_rx: Receiver<DnsReply>,
    dns_pending: usize,
    generation: u64,
    ephemeral: u16,
    start: Instant,
    last_ra: Instant,
}
impl Runtime {
    fn new(store: Store, saved: Saved, tx: SyncSender<Command>) -> Result<Self, NetError> {
        let start = Instant::now();
        let mut device = EthernetDevice::default();
        let mut config = Config::new(EthernetAddress(GATEWAY_MAC).into());
        config.random_seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        let mut iface = Interface::new(config, &mut device, StackInstant::from_millis(0));
        let gateway = store::allocate(&saved.config, "gateway", 0)?;
        let mut full = false;
        iface.update_ip_addrs(|addresses| {
            for cidr in [
                IpCidr::new(gateway.gateway_v4.into(), 24),
                IpCidr::new(gateway.gateway_v6.into(), 64),
                IpCidr::new(router_link_local().into(), 64),
            ] {
                full |= addresses.push(cidr).is_err();
            }
        });
        if full {
            return Err(NetError::Unavailable("the gateway interface cannot hold its three addresses".into()));
        }
        iface.set_any_ip(true);
        iface.routes_mut().add_default_ipv4_route(gateway.gateway_v4).map_err(|e| NetError::Unavailable(format!("IPv4 route: {e:?}")))?;
        iface.routes_mut().add_default_ipv6_route(gateway.gateway_v6).map_err(|e| NetError::Unavailable(format!("IPv6 route: {e:?}")))?;
        let maps = bind_maps(&saved.config.port_maps, &[])?;
        let policy = new_policy(&saved.config)?;
        let (dns_tx, dns_rx) = mpsc::channel();
        Ok(Self {
            saved,
            store,
            tx,
            ports: HashMap::new(),
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            tcp: vec![],
            dns_tcp: vec![],
            udp: HashMap::new(),
            maps,
            inbound_udp: vec![],
            policy,
            sink: None,
            events: HashMap::new(),
            dns_tx,
            dns_rx,
            dns_pending: 0,
            generation: 0,
            ephemeral: 49152,
            start,
            last_ra: start,
        })
    }
    fn now(&self) -> StackInstant {
        StackInstant::from_millis(self.start.elapsed().as_millis() as i64)
    }
    fn run(mut self, rx: Receiver<Command>) {
        loop {
            let mut stop = false;
            for _ in 0..64 {
                match rx.try_recv() {
                    Ok(Command::Stop) => {
                        stop = true;
                        break;
                    }
                    Ok(command) => self.command(command),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                break;
            }
            self.refresh_peers();
            let names: Vec<_> = self.ports.keys().cloned().collect();
            for name in names {
                for _ in 0..32 {
                    let frame = match self.ports.get_mut(&name).map(Port::receive) {
                        Some(Ok(Some(frame))) => frame,
                        Some(Err(_)) => {
                            if let Some(port) = self.ports.get_mut(&name) {
                                port.dead = true;
                            }
                            break;
                        }
                        _ => break,
                    };
                    self.frame(&name, frame);
                    self.poll_stack();
                }
            }
            self.accept_maps();
            self.pump_tcp();
            self.pump_udp();
            self.pump_dns_tcp();
            while let Ok(reply) = self.dns_rx.try_recv() {
                self.dns_pending = self.dns_pending.saturating_sub(1);
                if reply.generation == self.generation {
                    if dns::refused(&reply.response)
                        && let Some(sink) = &self.sink
                    {
                        let event = FlowEvent {
                            network: self.saved.config.id.clone(),
                            guest: reply.guest.clone(),
                            protocol: if reply.tcp.is_some() { Transport::Tcp } else { Transport::Udp },
                            source: reply.destination,
                            destination: reply.source,
                            allowed: false,
                            reason: "DNS destination denied by network policy".into(),
                            unlock: reply.question.as_ref().map(|name| format!("aho net allow {name}")),
                            unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
                        };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(event)));
                    }
                    if let Some(handle) = reply.tcp {
                        if let Some(flow) = self.dns_tcp.iter_mut().find(|f| f.handle == handle) {
                            flow.response.extend_from_slice(&(reply.response.len() as u16).to_be_bytes());
                            flow.response.extend_from_slice(&reply.response);
                            flow.pending = false;
                        }
                    } else {
                        let frame = packet::udp_frame(GATEWAY_MAC, reply.mac, reply.source, reply.destination, &reply.response);
                        if let Some(port) = self.ports.get_mut(&reply.guest) {
                            port.send(&frame);
                        }
                    }
                }
            }
            self.poll_stack();
            if self.last_ra.elapsed() >= Duration::from_secs(30) {
                for port in self.ports.values_mut() {
                    port.send(&services::advertisement(&port.nic));
                }
                self.last_ra = Instant::now();
                self.events.retain(|_, time| time.elapsed() < Duration::from_secs(60));
            }
            for port in self.ports.values_mut() {
                port.flush();
            }
            let dead: Vec<_> = self.ports.iter().filter(|(_, p)| p.dead).map(|(name, _)| name.clone()).collect();
            for name in dead {
                self.detach(&name);
            }
            thread::sleep(Duration::from_millis(2));
        }
        if let Ok(mut peers) = fabric().lock() {
            peers.remove(&self.saved.config.id);
        }
    }
    fn command(&mut self, command: Command) {
        match command {
            Command::Attach(name, reply) => {
                let result = self.attach(name);
                let _ = reply.send(result);
            }
            Command::Status(reply) => {
                let status = NetworkStatus {
                    config: self.saved.config.clone(),
                    nics: self.saved.nics.clone(),
                    attached: self.ports.keys().cloned().collect(),
                    active_flows: self.tcp.len() + self.udp.len() + self.inbound_udp.len(),
                    enforcement: crate::ENFORCED_BY.into(),
                };
                let _ = reply.send(Ok(status));
            }
            Command::Configure(config, reply) => {
                let result = self.configure(config);
                let _ = reply.send(result);
            }
            Command::Sink(sink, reply) => {
                self.sink = sink;
                let _ = reply.send(Ok(()));
            }
            Command::CrossFrame { network, mut frame } => {
                let Some(packet) = packet::parse(&frame) else {
                    return;
                };
                let source = packet.source.ip();
                let destination = packet.destination.ip();
                let permitted = fabric().lock().ok().and_then(|peers| peers.get(&network).cloned()).is_some_and(|peer| {
                    peer.config.isolation.other_sessions == Access::Allow && peer.nics.iter().any(|nic| ip_belongs(nic, source))
                });
                if !permitted || self.saved.config.isolation.other_sessions != Access::Allow {
                    return;
                }
                let Some(nic) = self.saved.nics.iter().find(|nic| ip_belongs(nic, destination)).cloned() else {
                    return;
                };
                if self.policy.lock().map_or(true, |p| p.check_other_session(&nic.name, source).is_err()) {
                    return;
                }
                frame[..6].copy_from_slice(&nic.mac);
                frame[6..12].copy_from_slice(&GATEWAY_MAC);
                if let Some(port) = self.ports.get_mut(&nic.name) {
                    port.send(&frame);
                }
            }
            Command::Stop => {}
        }
    }
    fn attach(&mut self, name: String) -> Result<Attachment, NetError> {
        store::valid_name(&name)?;
        let nic = if let Some(nic) = self.saved.nics.iter().find(|nic| nic.name == name) {
            nic.clone()
        } else {
            let nic = store::allocate(&self.saved.config, &name, self.saved.nics.len())?;
            let mut changed = self.saved.clone();
            changed.nics.push(nic.clone());
            self.store.save(&changed)?;
            self.saved = changed;
            self.update_registry();
            nic
        };
        let (guest, host) = UnixStream::pair()?;
        host.set_nonblocking(true)?;
        self.detach(&name);
        let mut port = Port::new(nic.clone(), host);
        port.send(&services::advertisement(&nic));
        self.ports.insert(name, port);
        Ok(Attachment { stream: guest, nic })
    }
    fn detach(&mut self, name: &str) {
        self.ports.remove(name);
        let mut index = 0;
        while index < self.tcp.len() {
            if self.tcp[index].key.guest == name {
                let flow = self.tcp.swap_remove(index);
                self.sockets.remove(flow.handle);
            } else {
                index += 1;
            }
        }
        self.udp.retain(|key, _| key.guest != name);
        self.inbound_udp.retain(|flow| flow.guest != name);
        let mut index = 0;
        while index < self.dns_tcp.len() {
            if self.dns_tcp[index].key.guest == name {
                let flow = self.dns_tcp.swap_remove(index);
                self.sockets.remove(flow.handle);
            } else {
                index += 1;
            }
        }
    }
    fn configure(&mut self, config: NetworkConfig) -> Result<(), NetError> {
        store::validate(&config)?;
        if config.id != self.saved.config.id || config.ipv4 != self.saved.config.ipv4 || config.ipv6 != self.saved.config.ipv6 {
            return Err(NetError::Invalid("network id/address prefixes are immutable; create a separate network".into()));
        }
        let maps = bind_maps(&config.port_maps, &self.maps)?;
        let policy = new_policy(&config)?;
        let mut changed = self.saved.clone();
        changed.config = config;
        self.store.save(&changed)?;
        for flow in &self.tcp {
            self.sockets.get_mut::<tcp::Socket>(flow.handle).abort();
        }
        self.poll_stack();
        self.sockets = SocketSet::new(vec![]);
        self.tcp.clear();
        self.dns_tcp.clear();
        self.udp.clear();
        self.inbound_udp.clear();
        self.events.clear();
        self.saved = changed;
        self.maps = maps;
        self.policy = policy;
        self.generation += 1;
        self.update_registry();
        Ok(())
    }
    fn update_registry(&self) {
        if let Ok(mut peers) = fabric().lock() {
            peers.insert(
                self.saved.config.id.clone(),
                Peer { config: self.saved.config.clone(), nics: self.saved.nics.clone(), tx: self.tx.clone() },
            );
        }
    }
    fn refresh_peers(&self) {
        if let (Ok(peers), Ok(mut policy)) = (fabric().lock(), self.policy.lock()) {
            policy.set_other_networks(
                peers
                    .iter()
                    .filter(|(id, _)| *id != &self.saved.config.id)
                    .flat_map(|(_, peer)| [peer.config.ipv4.into(), peer.config.ipv6.into()])
                    .collect(),
            );
        }
    }
    fn frame(&mut self, name: &str, frame: Vec<u8>) {
        let Some(nic) = self.ports.get(name).map(|port| port.nic.clone()) else {
            return;
        };
        if frame.len() < 14 || frame[6..12] != nic.mac {
            return;
        }
        if let Some(response) = services::handle(&frame, &nic) {
            if let Some(port) = self.ports.get_mut(name) {
                port.send(&response);
            }
            return;
        }
        if services::is_service(&frame) {
            return;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        if ethertype == 0x0806 {
            if !valid_arp(&frame, &nic) {
                return;
            }
            self.switch_control(name, &frame);
            self.device.incoming.push_back(frame);
            return;
        }
        if ethertype == 0x86dd && frame.len() >= 78 && frame[20] == 58 {
            if !valid_ndp(&frame, &nic) {
                return;
            }
            self.switch_control(name, &frame);
            self.device.incoming.push_back(frame);
            return;
        }
        let Some(packet) = packet::parse(&frame) else {
            return;
        };
        if !ip_belongs(&nic, packet.source.ip()) {
            return;
        }
        let key = Key { guest: name.into(), protocol: packet.protocol, source: packet.source, destination: packet.destination };
        if self.udp_map_reply(&key, packet.payload) {
            return;
        }
        if packet.destination.port() == 53 {
            if is_gateway(&nic, packet.destination.ip()) {
                if packet.protocol == Transport::Udp {
                    self.dns(&nic, &key, packet.payload, None);
                } else if self.dns_tcp.iter().any(|f| f.key == key) {
                    self.device.incoming.push_back(frame);
                } else if packet.tcp_syn && self.flow_count() < self.saved.config.max_flows {
                    let mut socket = tcp_socket();
                    if socket.listen(endpoint(key.destination)).is_ok() {
                        let handle = self.sockets.add(socket);
                        self.dns_tcp.push(DnsTcp {
                            key,
                            handle,
                            received: vec![],
                            pending: false,
                            response: vec![],
                            sent: 0,
                            touched: Instant::now(),
                        });
                        self.device.incoming.push_back(frame);
                    }
                }
            } else {
                self.event(&key, Err("DNS must use the network's controlled gateway resolver".into()));
            }
            return;
        }
        if let Some(target) = self.saved.nics.iter().find(|n| ip_belongs(n, packet.destination.ip())) {
            let mut forwarded = frame;
            forwarded[..6].copy_from_slice(&target.mac);
            forwarded[6..12].copy_from_slice(&GATEWAY_MAC);
            if let Some(port) = self.ports.get_mut(&target.name) {
                port.send(&forwarded);
            }
            return;
        }
        let other = fabric().lock().ok().and_then(|peers| {
            peers
                .iter()
                .find(|(id, peer)| *id != &self.saved.config.id && peer.nics.iter().any(|n| ip_belongs(n, key.destination.ip())))
                .map(|(_, p)| p.clone())
        });
        if let Some(other) = other {
            let decision =
                self.policy.lock().map_or_else(|_| Err("policy unavailable".into()), |p| p.check_other_session(name, key.destination.ip()));
            self.event(&key, decision.clone());
            if decision.is_ok() && other.config.isolation.other_sessions == Access::Allow {
                let _ = other.tx.try_send(Command::CrossFrame { network: self.saved.config.id.clone(), frame });
            }
            return;
        }
        // Replies to explicit inbound TCP maps are consumed by their existing smoltcp socket.
        if self.tcp.iter().any(|f| f.key.guest == name && f.key.source == key.source && f.key.destination == key.destination) {
            self.device.incoming.push_back(frame);
            return;
        }
        if is_gateway(&nic, key.destination.ip()) {
            self.event(&key, Err("gateway service is not exposed".into()));
            return;
        }
        let decision = self.policy.lock().map_or_else(|_| Err("policy unavailable".into()), |p| p.check(name, key.destination.ip(), None));
        self.event(&key, decision.clone());
        if decision.is_err() {
            return;
        }
        match packet.protocol {
            Transport::Tcp => {
                if !packet.tcp_syn || self.flow_count() >= self.saved.config.max_flows {
                    return;
                }
                if let Ok(host) = connect_tcp(key.destination) {
                    let mut socket = tcp_socket();
                    if socket.listen(endpoint(key.destination)).is_err() {
                        return;
                    }
                    let handle = self.sockets.add(socket);
                    self.tcp.push(TcpFlow {
                        key,
                        handle,
                        host,
                        connecting: true,
                        read_eof: false,
                        write_eof: false,
                        touched: Instant::now(),
                    });
                    self.device.incoming.push_back(frame);
                }
            }
            Transport::Udp => {
                if !self.udp.contains_key(&key) {
                    if self.flow_count() >= self.saved.config.max_flows {
                        return;
                    }
                    if let Ok(socket) = udp_socket(key.destination) {
                        self.udp.insert(key.clone(), UdpFlow { socket, mac: nic.mac, touched: Instant::now() });
                    }
                }
                if let Some(flow) = self.udp.get_mut(&key)
                    && flow.socket.send(packet.payload).is_ok()
                {
                    flow.touched = Instant::now();
                }
            }
        }
    }
    fn switch_control(&mut self, source: &str, frame: &[u8]) {
        let broadcast = frame[0] & 1 != 0;
        for (name, port) in &mut self.ports {
            if name != source && (broadcast || frame[..6] == port.nic.mac) {
                port.send(frame);
            }
        }
    }
    fn dns(&mut self, nic: &Nic, key: &Key, query: &[u8], tcp: Option<SocketHandle>) {
        if self.dns_pending >= 32 {
            return;
        }
        let query = query.to_vec();
        let nic = nic.clone();
        let nics = self.saved.nics.clone();
        let config = self.saved.config.clone();
        let policy = self.policy.clone();
        let tx = self.dns_tx.clone();
        let mut reply = DnsReply {
            generation: self.generation,
            guest: nic.name.clone(),
            mac: nic.mac,
            source: key.destination,
            destination: key.source,
            response: vec![],
            tcp,
            question: dns::question_name(&query),
        };
        if thread::Builder::new()
            .name("aho-net-dns".into())
            .spawn(move || {
                reply.response = dns::resolve(&query, &nic, &nics, &config, &policy, reply.tcp.is_some());
                let _ = tx.send(reply);
            })
            .is_ok()
        {
            self.dns_pending += 1;
        }
    }
    fn event(&mut self, key: &Key, decision: Result<(), String>) {
        if self.events.get(key).is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
            return;
        }
        if self.events.len() >= 4096 {
            self.events.clear();
        }
        self.events.insert(key.clone(), Instant::now());
        if let Some(sink) = &self.sink {
            let allowed = decision.is_ok();
            let unlock = if allowed { None } else { self.policy.lock().ok().and_then(|p| p.unlock(key.destination.ip())) };
            let event = FlowEvent {
                network: self.saved.config.id.clone(),
                guest: key.guest.clone(),
                protocol: key.protocol,
                source: key.source,
                destination: key.destination,
                allowed,
                reason: decision.err().unwrap_or_else(|| "network policy allowed".into()),
                unlock,
                unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            };
            // A misbehaving observer must not unwind the network worker or bypass policy.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(event)));
        }
    }
    fn flow_count(&self) -> usize {
        self.tcp.len() + self.dns_tcp.len() + self.udp.len() + self.inbound_udp.len()
    }
    fn poll_stack(&mut self) {
        self.iface.poll(self.now(), &mut self.device, &mut self.sockets);
        while let Some(frame) = self.device.outgoing.pop_front() {
            if frame.len() < 14 {
                continue;
            }
            let broadcast = frame[0] & 1 != 0;
            for port in self.ports.values_mut() {
                if broadcast || frame[..6] == port.nic.mac {
                    port.send(&frame);
                }
            }
        }
    }
    fn pump_dns_tcp(&mut self) {
        let mut index = 0;
        while index < self.dns_tcp.len() {
            let flow = &mut self.dns_tcp[index];
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if socket.can_recv() && flow.received.len() < 65537 {
                let remaining = 65537 - flow.received.len();
                let _ = socket.recv(|data| {
                    let n = data.len().min(remaining);
                    flow.received.extend_from_slice(&data[..n]);
                    flow.touched = Instant::now();
                    (n, ())
                });
            }
            if socket.can_send() && !flow.response.is_empty() {
                if let Ok(n) = socket.send_slice(&flow.response[flow.sent..]) {
                    flow.sent += n;
                }
                if flow.sent == flow.response.len() {
                    flow.response.clear();
                    flow.sent = 0;
                }
            }
            if flow.touched.elapsed() > Duration::from_secs(30)
                || (!socket.may_recv() && socket.state() == tcp::State::CloseWait && !flow.pending && flow.response.is_empty())
            {
                socket.close();
            }
            if socket.state() == tcp::State::Closed {
                let flow = self.dns_tcp.swap_remove(index);
                self.sockets.remove(flow.handle);
                continue;
            }
            if !flow.pending && flow.response.is_empty() && flow.received.len() >= 2 && self.dns_pending < 32 {
                let len = u16::from_be_bytes([flow.received[0], flow.received[1]]) as usize;
                if len < 12 {
                    socket.abort();
                } else if flow.received.len() >= len + 2 {
                    let query = flow.received[2..len + 2].to_vec();
                    flow.received.drain(..len + 2);
                    flow.pending = true;
                    let key = flow.key.clone();
                    let handle = flow.handle;
                    if let Some(nic) = self.saved.nics.iter().find(|nic| nic.name == key.guest).cloned() {
                        self.dns(&nic, &key, &query, Some(handle));
                    }
                }
            }
            index += 1;
        }
    }
    fn pump_tcp(&mut self) {
        let mut buffer = [0u8; 16384];
        let mut index = 0;
        while index < self.tcp.len() {
            let flow = &mut self.tcp[index];
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            let mut failed = false;
            if flow.connecting {
                match flow.host.take_error() {
                    Ok(Some(_)) | Err(_) => failed = true,
                    Ok(None) => {
                        if flow.host.peer_addr().is_ok() {
                            flow.connecting = false;
                        }
                    }
                }
                if flow.touched.elapsed() > Duration::from_secs(15) {
                    failed = true;
                }
            }
            if !flow.connecting && !failed {
                if socket.can_recv() {
                    let received = socket.recv(|data| match flow.host.write(data) {
                        Ok(n) => {
                            flow.touched = Instant::now();
                            (n, false)
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => (0, false),
                        Err(_) => (0, true),
                    });
                    failed |= received.unwrap_or(false);
                }
                if !socket.may_recv()
                    && !flow.write_eof
                    && socket.state() != tcp::State::Listen
                    && socket.state() != tcp::State::SynSent
                    && socket.state() != tcp::State::SynReceived
                {
                    let _ = flow.host.shutdown(Shutdown::Write);
                    flow.write_eof = true;
                }
                if socket.can_send() && !flow.read_eof {
                    let capacity = socket.send_capacity().saturating_sub(socket.send_queue()).min(buffer.len());
                    if capacity > 0 {
                        match flow.host.read(&mut buffer[..capacity]) {
                            Ok(0) => {
                                flow.read_eof = true;
                                socket.close();
                            }
                            Ok(n) => {
                                if socket.send_slice(&buffer[..n]).is_err() {
                                    failed = true;
                                }
                                flow.touched = Instant::now();
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => failed = true,
                        }
                    }
                }
            }
            if flow.touched.elapsed() > Duration::from_secs(300) {
                failed = true;
            }
            if failed {
                socket.abort();
            }
            if failed || socket.state() == tcp::State::Closed {
                let flow = self.tcp.swap_remove(index);
                // Poll before removal so abort emits a real TCP reset where possible.
                self.poll_stack();
                self.sockets.remove(flow.handle);
            } else {
                index += 1;
            }
        }
    }
    fn pump_udp(&mut self) {
        let mut buffer = [0u8; 65535];
        for (key, flow) in &mut self.udp {
            for _ in 0..16 {
                match flow.socket.recv(&mut buffer) {
                    Ok(n) => {
                        let frame = packet::udp_frame(GATEWAY_MAC, flow.mac, key.destination, key.source, &buffer[..n]);
                        if let Some(port) = self.ports.get_mut(&key.guest) {
                            port.send(&frame);
                        }
                        flow.touched = Instant::now();
                    }
                    Err(_) => break,
                }
            }
        }
        self.udp.retain(|_, flow| flow.touched.elapsed() < Duration::from_secs(60));
        self.inbound_udp.retain(|flow| flow.touched.elapsed() < Duration::from_secs(60));
    }
    fn next_port(&mut self) -> u16 {
        let port = self.ephemeral;
        self.ephemeral = if port == 65535 { 49152 } else { port + 1 };
        port
    }
    fn accept_maps(&mut self) {
        // Clone nonblocking descriptors, not socket state, to keep listener and flow borrows disjoint.
        for index in 0..self.maps.len() {
            let config = self.maps[index].config.clone();
            let Some(nic) = self.saved.nics.iter().find(|n| n.name == config.guest).cloned() else {
                continue;
            };
            if !self.ports.contains_key(&nic.name) {
                continue;
            }
            for _ in 0..16 {
                if self.flow_count() >= self.saved.config.max_flows {
                    break;
                }
                match &self.maps[index].listener {
                    Listener::Tcp(listener) => {
                        let Ok((host, peer)) = listener.accept() else {
                            break;
                        };
                        if host.set_nonblocking(true).is_err() {
                            continue;
                        }
                        let port = self.next_port();
                        let (guest_ip, gateway_ip) = if peer.is_ipv4() {
                            (IpAddr::V4(nic.ipv4), IpAddr::V4(nic.gateway_v4))
                        } else {
                            (IpAddr::V6(nic.ipv6), IpAddr::V6(nic.gateway_v6))
                        };
                        let guest = SocketAddr::new(guest_ip, config.guest_port);
                        let local = SocketAddr::new(gateway_ip, port);
                        let mut socket = tcp_socket();
                        if socket.connect(self.iface.context(), endpoint(guest), endpoint(local)).is_err() {
                            continue;
                        }
                        let handle = self.sockets.add(socket);
                        let key = Key { guest: nic.name.clone(), protocol: Transport::Tcp, source: guest, destination: local };
                        self.event(&Key { guest: nic.name.clone(), protocol: Transport::Tcp, source: peer, destination: guest }, Ok(()));
                        self.tcp.push(TcpFlow {
                            key,
                            handle,
                            host,
                            connecting: false,
                            read_eof: false,
                            write_eof: false,
                            touched: Instant::now(),
                        });
                    }
                    Listener::Udp(listener) => {
                        let listener = listener.clone();
                        let mut buffer = [0u8; 65535];
                        let Ok((len, peer)) = listener.recv_from(&mut buffer) else {
                            break;
                        };
                        let (guest_ip, gateway_ip) = if peer.is_ipv4() {
                            (IpAddr::V4(nic.ipv4), IpAddr::V4(nic.gateway_v4))
                        } else {
                            (IpAddr::V6(nic.ipv6), IpAddr::V6(nic.gateway_v6))
                        };
                        let destination = SocketAddr::new(guest_ip, config.guest_port);
                        let source = if let Some(flow) = self.inbound_udp.iter_mut().find(|f| {
                            f.guest == nic.name && f.peer == peer && f.destination == destination && Arc::ptr_eq(&f.host, &listener)
                        }) {
                            flow.touched = Instant::now();
                            flow.source
                        } else {
                            let source = SocketAddr::new(gateway_ip, self.next_port());
                            self.inbound_udp.push(UdpInbound {
                                guest: nic.name.clone(),
                                source,
                                destination,
                                host: listener,
                                peer,
                                touched: Instant::now(),
                            });
                            self.event(&Key { guest: nic.name.clone(), protocol: Transport::Udp, source: peer, destination }, Ok(()));
                            source
                        };
                        let frame = packet::udp_frame(GATEWAY_MAC, nic.mac, source, destination, &buffer[..len]);
                        if let Some(port) = self.ports.get_mut(&nic.name) {
                            port.send(&frame);
                        }
                    }
                }
            }
        }
    }
    fn udp_map_reply(&mut self, key: &Key, payload: &[u8]) -> bool {
        if key.protocol != Transport::Udp {
            return false;
        }
        if let Some(flow) =
            self.inbound_udp.iter_mut().find(|f| f.guest == key.guest && f.destination == key.source && f.source == key.destination)
        {
            if flow.host.send_to(payload, flow.peer).is_ok() {
                flow.touched = Instant::now();
            }
            true
        } else {
            false
        }
    }
}

fn router_link_local() -> Ipv6Addr {
    Ipv6Addr::new(0xfe80, 0, 0, 0, 0x61, 0x68ff, 0xfe6f, 1)
}
fn ip_belongs(nic: &Nic, ip: IpAddr) -> bool {
    ip == IpAddr::V4(nic.ipv4) || ip == IpAddr::V6(nic.ipv6)
}
fn is_gateway(nic: &Nic, ip: IpAddr) -> bool {
    ip == IpAddr::V4(nic.gateway_v4) || ip == IpAddr::V6(nic.gateway_v6) || ip == IpAddr::V6(router_link_local())
}
fn endpoint(addr: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(addr.ip().into(), addr.port())
}
fn tcp_socket() -> tcp::Socket<'static> {
    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536]));
    socket.set_timeout(Some(smoltcp::time::Duration::from_secs(300)));
    socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
    socket
}
fn connect_tcp(destination: SocketAddr) -> io::Result<TcpStream> {
    let socket = Socket::new(Domain::for_address(destination), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    match socket.connect(&destination.into()) {
        Ok(()) => {}
        Err(e)
            if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::Interrupted
                || matches!(e.raw_os_error(), Some(36 | 115)) => {}
        Err(e) => return Err(e),
    }
    let stream: TcpStream = socket.into();
    stream.set_nodelay(true)?;
    Ok(stream)
}
fn udp_socket(destination: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(if destination.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    })?;
    socket.connect(destination)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}
fn bind_maps(configs: &[PortMap], old: &[Mapping]) -> Result<Vec<Mapping>, NetError> {
    configs
        .iter()
        .map(|config| {
            let existing = old.iter().find(|map| map.config.protocol == config.protocol && map.config.host == config.host);
            let listener = match (config.protocol, existing.map(|m| &m.listener)) {
                (Transport::Tcp, Some(Listener::Tcp(listener))) => Listener::Tcp(listener.try_clone()?),
                (Transport::Udp, Some(Listener::Udp(listener))) => Listener::Udp(listener.clone()),
                (Transport::Tcp, _) => {
                    let socket = Socket::new(Domain::for_address(config.host), Type::STREAM, Some(Protocol::TCP))?;
                    if config.host.is_ipv6() {
                        socket.set_only_v6(true)?;
                    }
                    socket.set_reuse_address(true)?;
                    socket.bind(&config.host.into())?;
                    socket.listen(128)?;
                    socket.set_nonblocking(true)?;
                    Listener::Tcp(socket.into())
                }
                (Transport::Udp, _) => {
                    let socket = Socket::new(Domain::for_address(config.host), Type::DGRAM, Some(Protocol::UDP))?;
                    if config.host.is_ipv6() {
                        socket.set_only_v6(true)?;
                    }
                    socket.bind(&config.host.into())?;
                    socket.set_nonblocking(true)?;
                    Listener::Udp(Arc::new(socket.into()))
                }
            };
            Ok(Mapping { config: config.clone(), listener })
        })
        .collect()
}
fn new_policy(config: &NetworkConfig) -> Result<Arc<Mutex<Policy>>, NetError> {
    let mut policy = Policy::new(config);
    let addresses = if_addrs::get_if_addrs()
        .map_err(|e| NetError::Unavailable(format!("cannot identify host interfaces for egress isolation: {e}")))?;
    policy.set_host_ips(addresses.iter().map(|address| address.ip()).collect());
    policy.set_host_networks(
        addresses
            .into_iter()
            .filter_map(|address| match address.addr {
                if_addrs::IfAddr::V4(a) => ipnet::Ipv4Net::with_netmask(a.ip, a.netmask).ok().map(|net| ipnet::IpNet::V4(net.trunc())),
                if_addrs::IfAddr::V6(a) => ipnet::Ipv6Net::with_netmask(a.ip, a.netmask).ok().map(|net| ipnet::IpNet::V6(net.trunc())),
            })
            .collect(),
    );
    Ok(Arc::new(Mutex::new(policy)))
}
fn valid_arp(frame: &[u8], nic: &Nic) -> bool {
    frame.len() >= 42
        && frame[14..20] == [0, 1, 8, 0, 6, 4]
        && matches!(&frame[20..22], [0, 1] | [0, 2])
        && frame[22..28] == nic.mac
        && (frame[28..32] == nic.ipv4.octets() || frame[28..32] == [0, 0, 0, 0])
}
fn valid_ndp(frame: &[u8], nic: &Nic) -> bool {
    if frame.len() < 78 || frame[21] != 255 || !matches!(frame[54], 135 | 136) {
        return false;
    }
    let v6 = |at: usize| frame.get(at..at + 16).and_then(|b| <[u8; 16]>::try_from(b).ok()).map(Ipv6Addr::from);
    let (Some(source), Some(target), Some(destination)) = (v6(22), v6(62), v6(38)) else { return false };
    let packet = smoltcp::wire::Icmpv6Packet::new_checked(&frame[54..]);
    if packet.is_err() || !packet.is_ok_and(|packet| packet.verify_checksum(&source, &destination)) {
        return false;
    }
    if source != nic.ipv6 && !source.is_unicast_link_local() && !source.is_unspecified() {
        return false;
    }
    if source == router_link_local() || source == nic.gateway_v6 {
        return false;
    }
    if frame[54] == 136 && target != nic.ipv6 {
        return false;
    }
    // Never relay unvalidated option lengths or an advertised MAC different from the port.
    let payload_len = u16::from_be_bytes([frame[18], frame[19]]) as usize;
    if payload_len < 24 || frame.len() < 54 + payload_len {
        return false;
    }
    let mut options = &frame[78..54 + payload_len];
    while !options.is_empty() {
        if options.len() < 2 || options[1] == 0 {
            return false;
        }
        let size = options[1] as usize * 8;
        if size > options.len() || (matches!(options[0], 1 | 2) && (size != 8 || options[2..8] != nic.mac)) {
            return false;
        }
        options = &options[size..];
    }
    true
}
