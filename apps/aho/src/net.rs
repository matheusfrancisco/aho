//! `aho net`: a session's sandbox network (`status`, `allow <dest>`, `map HOST:GUEST`).
//! A running network is changed through its control socket and records the change in its own
//! session log; a stopped one is opened here, changed, and the change is appended as the
//! operator (refused, before anything changes, while another process drives the session).

use aho_cap::Config;
use aho_net::{Network, NetworkConfig, NetworkStatus, Transport};
use net_userspace::{NetSettings, Remote, open_session_network, operator, session::configured_entry, state_dir};
use serde_json::json;

use crate::cli::NetVerb;
use crate::ctx::{Ctx, Fail, branch, err, find_session, hex};
use crate::record::Writer;
use crate::session::state_of;

/// One operator change to a network config: `Ok(Some(what changed))`, `Ok(None)` when it holds already.
type Edit = Box<dyn Fn(&mut NetworkConfig) -> Result<Option<String>, aho_net::NetError>>;

/// The NIC name a namespace sandbox attaches with; `aho net map` targets it by default.
pub const SANDBOX: &str = "sandbox";

pub async fn run(ctx: &Ctx, sub: &NetVerb, session: Option<&str>, json_out: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    let state = state_dir(&dir, s.id);
    let edit: Edit = match sub {
        NetVerb::Status => return status(&state, s.id, json_out),
        NetVerb::Allow { dest } => {
            let dest = dest.clone();
            Box::new(move |c| operator::allow(c, &dest))
        }
        NetVerb::Map { spec, bind, guest, udp } => {
            let (spec, bind, guest) = (spec.clone(), *bind, guest.clone());
            let protocol = if *udp { Transport::Udp } else { Transport::Tcp };
            Box::new(move |c| operator::map(c, &spec, bind, &guest, protocol))
        }
    };
    let net_err = |e: aho_net::NetError| err(format!("{e}; nothing was changed"));

    if let Some(remote) = Remote::connect(&state) {
        let mut config = remote.status().map_err(net_err)?.config;
        let Some(change) = edit(&mut config).map_err(net_err)? else {
            return report(json_out, s.id, None, "already so; nothing to change", true);
        };
        let after = remote.configure(config).map_err(net_err)?;
        return report(json_out, s.id, Some(&after), &format!("{change} (applied to the running network; its session records it)"), true);
    }

    // No live network: this process changes the state directory, and records the change as the
    // operator, so the writer is secured first: a refused writer leaves everything untouched.
    let (_, entries) = branch(&s.path)?;
    let w = Writer::open(ctx, &dir, &s, &state_of(&entries)?).await.map_err(|why| {
        err(format!("{why}, so the network was not changed: two processes never write one log. Run this from the client that drives it"))
    })?;
    let settings = NetSettings::from_loadout(&p.loadout).map_err(|e| err(e.to_string()))?;
    let net = match open_session_network(&dir, s.id, &settings) {
        Ok(net) => net,
        Err(e) if !Remote::reachable_path(&state) => {
            return Err(err(format!(
                "the network of session {} could not be opened ({e}); if it runs in another process, its state directory {} is too deep for a control socket, so it cannot be changed while it runs. Set `session.dir` to a shorter path",
                hex(s.id),
                state.display()
            )));
        }
        Err(e) => return Err(net_err(e)),
    };
    let mut config = net.status().map_err(net_err)?.config;
    let Some(change) = edit(&mut config).map_err(net_err)? else {
        w.finish(false).await.map_err(err)?;
        return report(json_out, s.id, None, "already so; nothing to change", false);
    };
    net.configure(config).map_err(net_err)?;
    let after = net.status().map_err(net_err)?;
    drop(net);
    let appended = w.append(configured_entry(&after.config, &change)).await;
    let via = w.finish(false).await.map_err(err)?;
    appended.map_err(|e| err(format!("the network was changed ({change}) but the change could not be recorded: {e}")))?;
    let via = via.map(|v| format!(" {v}")).unwrap_or_default();
    report(json_out, s.id, Some(&after), &format!("{change} (by Operator{via}; applies when the sandbox next attaches)"), false)
}

fn report(json_out: bool, id: aho_types::SessionId, status: Option<&NetworkStatus>, what: &str, running: bool) -> Result<(), Fail> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"session": hex(id), "change": what, "running": running, "status": status}))?);
    } else {
        println!("session {}: {what}", hex(id));
    }
    Ok(())
}

fn status(state: &std::path::Path, id: aho_types::SessionId, json_out: bool) -> Result<(), Fail> {
    let (status, running) = if let Some(remote) = Remote::connect(state) {
        (remote.status().map_err(|e| err(e.to_string()))?, true)
    } else if state.join("network.json").is_file() {
        // Stopped: read what is persisted without starting anything (no ports are bound).
        let text = std::fs::read_to_string(state.join("network.json"))?;
        let saved: serde_json::Value = serde_json::from_str(&text)?;
        let config: NetworkConfig = serde_json::from_value(saved["config"].clone())?;
        let nics = serde_json::from_value(saved["nics"].clone()).unwrap_or_default();
        (NetworkStatus { config, nics, attached: vec![], active_flows: 0, enforcement: net_userspace::ENFORCED_BY.into() }, false)
    } else {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({"session": hex(id), "network": null}))?);
        } else {
            println!("session {} has no sandbox network yet; a sandbox world (`seams.world = \"namespace\"` or a VM) starts it", hex(id));
        }
        return Ok(());
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"session": hex(id), "running": running, "network": status}))?);
        return Ok(());
    }
    let c = &status.config;
    let access = |a: aho_net::Access| if a == aho_net::Access::Allow { "allow" } else { "deny" };
    println!("network {} ({})", c.id, if running { "running" } else { "stopped" });
    println!("  subnets     {} · {}", c.ipv4, c.ipv6);
    println!(
        "  isolation   internet {} · host-lan {} · other-sessions {}",
        access(c.isolation.internet),
        access(c.isolation.host_lan),
        access(c.isolation.other_sessions)
    );
    let list = |v: &[aho_net::Destination]| {
        v.iter()
            .map(|d| match d {
                aho_net::Destination::Cidr(n) => n.to_string(),
                aho_net::Destination::Domain(n) => n.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "  egress      allow [{}] · deny [{}]",
        if c.egress.allow.is_empty() { "any".into() } else { list(&c.egress.allow) },
        list(&c.egress.deny)
    );
    println!("  resolvers   {}", c.dns_upstream.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
    for m in &c.port_maps {
        let kind = if m.protocol == Transport::Udp { "udp" } else { "tcp" };
        println!("  map         {kind} {} → {}:{}", m.host, m.guest, m.guest_port);
    }
    for n in &status.nics {
        let on = if status.attached.contains(&n.name) { "attached" } else { "detached" };
        println!("  nic         {} {} · {} ({on})", n.name, n.ipv4, n.ipv6);
    }
    if running {
        println!("  flows       {} active", status.active_flows);
    }
    println!("  enforced    {}", status.enforcement);
    Ok(())
}
