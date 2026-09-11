//! The session layer over the network: subnet allocation across a sessions directory, the
//! operator's control socket, and flow decisions becoming log entries.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_net::{Destination, FlowEvent, Isolation, Network, PortMap, Transport};
use aho_types::{Form, SessionId};
use net_userspace::{NetSettings, Remote, open_session_network, session::log_sink, state_dir};

fn sessions(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("aho-net-sessions-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings() -> NetSettings {
    NetSettings {
        dns_upstream: vec![SocketAddr::from(([127, 0, 0, 1], 53))],
        isolation: Isolation::default(),
        egress: Default::default(),
        max_flows: 64,
    }
}

#[test]
fn a_new_session_network_avoids_a_persisted_neighbours_subnet_and_keeps_its_own_on_resume() {
    let dir = sessions("alloc");
    let s = settings();
    // Two ids whose derived subnets are the same index: the second must move.
    let first = SessionId(0x5eed_0001);
    let a = open_session_network(&dir, first, &s).unwrap();
    let a_prefix = a.status().unwrap().config.ipv4;
    drop(a); // no longer running, but persisted: it may resume next to the next session
    let k = a_prefix.addr().octets()[2];
    let second = (2u128..)
        .map(|n| SessionId(0x5eed_0000 + n))
        .find(|id| aho_net::NetworkConfig::new(format!("{:x}", id.0)).prefix_index() == k)
        .unwrap();
    let b = open_session_network(&dir, second, &s).unwrap();
    let b_prefix = b.status().unwrap().config.ipv4;
    assert_ne!(b_prefix, a_prefix, "a persisted neighbour's subnet is never reused");
    drop(b);

    let resumed = open_session_network(&dir, first, &s).unwrap();
    assert_eq!(resumed.status().unwrap().config.ipv4, a_prefix, "resume keeps the network's own subnet");
    assert!(state_dir(&dir, first).join("network.json").is_file());
}

#[test]
fn the_control_socket_reaches_only_a_running_network_and_changes_persist() {
    let dir = sessions("control");
    let id = SessionId(0xc0de);
    let net = open_session_network(&dir, id, &settings()).unwrap();
    let recorded: Arc<std::sync::Mutex<Vec<aho_net::NetworkConfig>>> = Arc::default();
    let r = recorded.clone();
    net.set_config_sink(Some(Arc::new(move |c: &aho_net::NetworkConfig| r.lock().unwrap().push(c.clone()))));
    let remote = Remote::connect(&state_dir(&dir, id)).expect("a running network answers its control socket");

    let mut config = remote.status().unwrap().config;
    config.egress.deny.push(Destination::parse("*.example.invalid").unwrap());
    config.port_maps.push(PortMap {
        protocol: Transport::Tcp,
        host: "127.0.0.1:0".parse().unwrap(),
        guest: "sandbox".into(),
        guest_port: 3000,
    });
    assert!(remote.configure(config.clone()).is_err(), "port 0 is refused and nothing changes");
    assert!(net.status().unwrap().config.egress.deny.is_empty());
    assert!(recorded.lock().unwrap().is_empty(), "a refused change is not recorded");

    config.port_maps.clear();
    let after = remote.configure(config).unwrap();
    assert_eq!(after.config.egress.deny, vec![Destination::Domain("*.example.invalid".into())]);
    assert_eq!(net.status().unwrap().config.egress.deny, after.config.egress.deny, "the owner sees the operator's change");
    assert_eq!(*recorded.lock().unwrap(), vec![after.config.clone()], "the owner is told once, with the applied config");

    drop(net);
    assert!(Remote::connect(&state_dir(&dir, id)).is_none(), "a stopped network has no control socket");
    let reopened = open_session_network(&dir, id, &settings()).unwrap();
    assert_eq!(reopened.status().unwrap().config.egress.deny, after.config.egress.deny, "the change was persisted");
}

fn event(guest: &str, allowed: bool) -> FlowEvent {
    FlowEvent {
        network: "n".into(),
        guest: guest.into(),
        protocol: Transport::Tcp,
        source: "10.85.1.10:40000".parse().unwrap(),
        destination: "203.0.113.7:443".parse().unwrap(),
        allowed,
        reason: if allowed { "allowed".into() } else { "egress deny".into() },
        unlock: (!allowed).then(|| "aho net allow 203.0.113.7".into()),
        unix_ms: 0,
    }
}

#[test]
fn every_flow_is_logged_and_a_blocked_destination_raises_one_notice_with_its_unlock() {
    let log = Arc::new(MemoryLog::new());
    let sink = log_sink(log.clone());
    sink(event("sandbox", true));
    sink(event("sandbox", false));
    sink(event("sandbox", false)); // the same blocked destination again: no second Notice
    sink(event("vm-db", false)); // another guest's block is its own Notice

    let entries = log.read_from(None);
    let flows = entries.iter().filter(|e| matches!(&e.kind, EntryKind::Custom { ns, name, .. } if ns == "Net" && name == "Flow")).count();
    let notices: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::UserMessage { form: Form::Notice, text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(flows, 4, "every decision is a Net/Flow entry");
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices.iter().all(|t| t.contains("`aho net allow 203.0.113.7`")), "{notices:?}");
    assert!(notices[1].starts_with("network: vm-db"), "{notices:?}");
}
