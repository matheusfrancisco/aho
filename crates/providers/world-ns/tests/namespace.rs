//! The namespace world end to end on a real kernel: a busybox recipe, the session's real
//! userspace network, and the properties the sandbox promises (PID 2, a private seeded
//! workspace, stray jobs that die with their command, exit status passthrough, egress through
//! aho-net only, persistence across re-entry without a network, recipe drift refused, destroy).
//!
//! Needs Linux with user namespaces, overlayfs, /dev/net/tun, `ip`, and a static busybox at
//! /bin/busybox (Debian `busybox-static`); anything missing skips with the reason. The Linux CI
//! container runs it privileged: `linux-run.sh --privileged --apt "iproute2 busybox-static"`.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aho_fs::Fs;
use aho_net::{Access, FlowEvent, Network, NetworkConfig};
use aho_proc::{SpawnSpec, Subprocess};
use aho_types::SessionId;
use net_userspace::UserspaceNetwork;
use world_ns::{NamespaceWorld, NsConfig, NsSettings};

fn missing() -> Option<String> {
    for (path, what) in [("/bin/busybox", "a static busybox"), ("/bin/bash", "bash"), ("/dev/net/tun", "/dev/net/tun")] {
        if !Path::new(path).exists() {
            return Some(format!("{what} ({path})"));
        }
    }
    if std::process::Command::new("ip").arg("-V").output().is_err() {
        return Some("iproute2 (`ip`)".into());
    }
    None
}

/// Under the system temp dir: inside a container that is overlayfs, where a sandbox's state
/// must work too (an overlay upper layer there is refused by the kernel).
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aho-ns-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A recipe: busybox and the applets the scenarios use, plus this host's bash with the shared
/// libraries `ldd` names (its `/dev/tcp` is the network client; static busybox's resolver
/// needs glibc NSS modules a minimal recipe does not have).
fn rootfs(base: &Path) -> PathBuf {
    let root = base.join("recipe");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::copy("/bin/busybox", root.join("bin/busybox")).unwrap();
    for applet in ["sh", "cat", "echo", "sleep", "test", "ls", "id", "kill", "true"] {
        std::os::unix::fs::symlink("busybox", root.join("bin").join(applet)).unwrap();
    }
    std::fs::copy("/bin/bash", root.join("bin/bash")).unwrap();
    let ldd = std::process::Command::new("ldd").arg("/bin/bash").output().unwrap();
    for lib in String::from_utf8_lossy(&ldd.stdout).split_whitespace().filter(|w| w.starts_with('/')) {
        let to = root.join(lib.trim_start_matches('/'));
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(lib, to).unwrap();
    }
    root
}

fn private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A non-loopback address of this host (the container's own): what the sandbox reaches via NAT.
fn host_ip() -> Ipv4Addr {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect("192.0.2.1:9").unwrap();
    match s.local_addr().unwrap().ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => panic!("no IPv4 host address"),
    }
}

/// Answers one HTTP GET per connection with `body`.
fn http(bind: Ipv4Addr, body: &'static str) -> SocketAddr {
    let l = TcpListener::bind((bind, 0)).unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b);
            let _ = write!(c, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        }
    });
    addr
}

async fn run(world: &NamespaceWorld, script: &str) -> (i32, String, Duration) {
    let start = Instant::now();
    let child =
        world.subprocess().spawn(SpawnSpec { timeout_ms: Some(20_000), ..SpawnSpec::new(&["/bin/sh", "-c", script]) }).await.unwrap();
    let exit = child.wait().await.unwrap();
    let mut out = Vec::new();
    let mut cursor = 0;
    loop {
        let (bytes, next, done) = child.output(cursor);
        out.extend(bytes);
        cursor = next;
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (exit.code, String::from_utf8_lossy(&out).into_owned(), start.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_sandbox_is_confined_persistent_networked_only_through_aho_net_and_destroyable() {
    if let Some(what) = missing() {
        eprintln!("skipped: the namespace world needs {what}");
        return;
    }
    let base = scratch("e2e");
    let recipe = rootfs(&base);
    let project = base.join("project");
    std::fs::create_dir_all(project.join(".aho/sessions")).unwrap();
    std::fs::write(project.join("seed.txt"), "seeded\n").unwrap();
    std::fs::write(project.join(".aho/secret"), "host-only\n").unwrap();
    let sessions = project.join(".aho/sessions");

    let net_dir = sessions.join("5e55.net");
    private(&net_dir);
    let mut nc = NetworkConfig::with_prefix("5e55", 77);
    nc.isolation.host_lan = Access::Allow; // the "internet" here is this host's own address
    let net = UserspaceNetwork::open(&net_dir, nc).unwrap();
    let flows: Arc<Mutex<Vec<FlowEvent>>> = Arc::default();
    let f = flows.clone();
    net.set_flow_sink(Some(Arc::new(move |e| f.lock().unwrap().push(e)))).unwrap();

    let settings =
        NsSettings { rootfs: recipe.clone(), workspace: None, helper: env!("CARGO_BIN_EXE_aho-ns-helper").into(), shell: "/bin/sh".into() };
    let id = SessionId(0x5e55);
    let cfg = NsConfig::new(&settings, &sessions, id, &project);
    let world = match NamespaceWorld::open(cfg.clone(), Arc::new(net.clone())) {
        Ok(w) => w,
        Err(e) => {
            let e = format!("{e:?}");
            assert!(e.contains("could not start"), "an unexpected failure, not a missing kernel feature: {e}");
            eprintln!("skipped: this kernel/container refuses unprivileged namespaces: {e}");
            return;
        }
    };
    assert_eq!(world.status().nic.as_ref().map(|n| n.name.as_str()), Some("sandbox"));

    // PID 2 in its own PID namespace; the seed arrived, aho's own state did not.
    let (code, out, _) =
        run(&world, "echo pid=$$; cat /workspace/seed.txt; test -e /workspace/.aho && echo leaked || echo clean; id -u").await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "pid=2\nseeded\nclean\n0\n");

    // Files through the Fs seam land inside, not in the project.
    world.fs().write("made.txt", "inside\n", None).await.unwrap();
    assert!(!project.join("made.txt").exists(), "the host project is untouched");
    assert_eq!(run(&world, "cat /workspace/made.txt").await.1, "inside\n");

    // A stray background job dies with its command instead of holding the call.
    let (code, out, took) = run(&world, "sleep 30 & echo started").await;
    assert_eq!((code, out.as_str()), (0, "started\n"));
    assert!(took < Duration::from_secs(10), "a backgrounded sleep held the call for {took:?}");

    // Exit status passes through, a signal as 128+n.
    assert_eq!(run(&world, "exit 7").await.0, 7);
    assert_eq!(run(&world, "kill -9 $$").await.0, 137);

    // The only way out is aho-net's NAT: the host's address answers through it and it is logged.
    let server = http(host_ip(), "through aho-net");
    let get = format!(
        "exec bash -c 'exec 3<>/dev/tcp/{}/{} && printf \"GET / HTTP/1.0\\r\\n\\r\\n\" >&3 && cat <&3'",
        server.ip(),
        server.port()
    );
    let (code, out, _) = run(&world, &get).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.ends_with("\r\n\r\nthrough aho-net"), "{out:?}");
    assert!(
        flows.lock().unwrap().iter().any(|e| e.allowed && e.destination == server && e.guest == "sandbox"),
        "the flow was a Net/Flow decision"
    );

    // Re-entry without the network (the operator's verbs): the workspace persisted.
    drop(world);
    let again = NamespaceWorld::existing(cfg.clone()).unwrap();
    assert!(again.status().running);
    assert_eq!(run(&again, "cat /workspace/made.txt").await.1, "inside\n");

    // A different recipe/shell for the same sandbox is refused, never silently mixed.
    let drifted = NsConfig { shell: "/bin/ash".into(), ..cfg.clone() };
    let e = format!("{:?}", NamespaceWorld::existing(drifted).err().unwrap());
    assert!(e.contains("another recipe"), "{e}");

    // Destroy stops everything and removes the sandbox; nothing is left to enter.
    drop(again);
    assert!(world_ns::destroy(&cfg.dir).unwrap());
    assert!(!cfg.dir.exists());
    assert!(!world_ns::destroy(&cfg.dir).unwrap(), "destroying nothing is not an error");
    let e = format!("{:?}", NamespaceWorld::existing(cfg).err().unwrap());
    assert!(e.contains("no sandbox"), "{e}");
}
