//! Audited Linux boundary. This module runs both the host-side lifecycle and the
//! dedicated, single-threaded helper. No untrusted command executes before pivot_root,
//! a fresh PID namespace, a private procfs and irrevocable capability removal.
use crate::{Core, Keeper, NIC, NsConfig};
use aho_net::{Network, Nic};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

type Result<T, E = String> = std::result::Result<T, E>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn cv(value: libc::c_int) -> Result<()> {
    if value < 0 { Err(err(io::Error::last_os_error())) } else { Ok(()) }
}
fn c(s: impl AsRef<std::ffi::OsStr>) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(s.as_ref().as_bytes()).map_err(err)
}
fn lock(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(err)?;
    // SAFETY: flock only reads this live descriptor.
    cv(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) })?;
    Ok(f)
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(err)?;
    let meta = fs::symlink_metadata(path).map_err(err)?;
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{} must be a directory owned by the current user", path.display()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(err)
}
fn start_time(pid: u32) -> Result<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(err)?;
    s.rsplit_once(')')
        .and_then(|(_, s)| s.split_whitespace().nth(19))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "invalid process start identity".into())
}
pub(crate) fn check_keeper(k: &Keeper) -> Result<()> {
    if start_time(k.pid)? != k.start {
        return Err("namespace keeper identity changed; refusing PID reuse".into());
    }
    Ok(())
}
fn atomic_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let tmp = path.with_extension("new");
    fs::write(&tmp, serde_json::to_vec(value).map_err(err)?).map_err(err)?;
    fs::rename(tmp, path).map_err(err)
}
/// Copies `source` into `target`, never descending into `skip` (aho's own state inside a
/// project seed). Sockets, FIFOs and device nodes are not copied: no host resource crosses.
fn copy_tree(source: &Path, target: &Path, skip: &[PathBuf]) -> Result<()> {
    fs::create_dir_all(target).map_err(err)?;
    for entry in fs::read_dir(source).map_err(err)? {
        let entry = entry.map_err(err)?;
        let from = entry.path();
        if skip.contains(&from) {
            continue;
        }
        let to = target.join(entry.file_name());
        let meta = fs::symlink_metadata(&from).map_err(err)?;
        if meta.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from).map_err(err)?, to).map_err(err)?;
        } else if meta.is_dir() {
            copy_tree(&from, &to, skip)?;
            fs::set_permissions(&to, meta.permissions()).map_err(err)?;
        } else if meta.is_file() {
            fs::copy(&from, &to).map_err(err)?;
        }
    }
    Ok(())
}
fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))
}
fn validate(cfg: &NsConfig) -> Result<()> {
    if !cfg.rootfs.is_dir() || !cfg.workspace.is_dir() || cfg.rootfs == Path::new("/") {
        return Err(
            "the namespace world needs an unpacked recipe directory (`world.ns.rootfs`, never /) and a workspace seed directory".into()
        );
    }
    if cfg.dir.starts_with(&cfg.rootfs) || cfg.rootfs.starts_with(&cfg.dir) {
        return Err("the sandbox state must be outside the recipe".into());
    }
    if cfg.workspace.starts_with(&cfg.dir) {
        return Err("the workspace seed cannot be inside the sandbox state".into());
    }
    for p in [&cfg.rootfs, &cfg.workspace, &cfg.dir] {
        if p.as_os_str().as_encoded_bytes().iter().any(|b| matches!(b, b':' | b',' | b'\\' | b'\n')) {
            return Err("namespace paths cannot contain overlayfs option delimiters".into());
        }
    }
    Ok(())
}

fn helper_path(path: &Path) -> Result<PathBuf> {
    if path.components().count() > 1 {
        return canonical(path);
    }
    let paths = std::env::var_os("PATH").ok_or("PATH is unset; configure world.ns.helper absolutely")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join(path);
        if p.is_file() {
            return canonical(&p);
        }
    }
    Err(format!("{} is not on PATH; install aho-ns-helper or set world.ns.helper", path.display()))
}

pub(crate) fn open(mut cfg: NsConfig, network: Option<Arc<dyn Network>>) -> Result<Core> {
    cfg.rootfs = canonical(&cfg.rootfs)?;
    cfg.workspace = canonical(&cfg.workspace)?;
    cfg.helper = helper_path(&cfg.helper)?;
    let saved = cfg.dir.join("config.json");
    if network.is_none() && !saved.is_file() {
        return Err(format!("no sandbox at {} yet; it is created when its session first runs", cfg.dir.display()));
    }
    private_dir(&cfg.dir)?;
    cfg.dir = canonical(&cfg.dir)?;
    validate(&cfg)?;
    let dir = cfg.dir.clone();
    let _lock = lock(&dir.join("lifecycle.lock"))?;
    if saved.exists() {
        let previous: NsConfig = serde_json::from_slice(&fs::read(&saved).map_err(err)?).map_err(err)?;
        if previous != cfg {
            return Err(
                "this sandbox was created from another recipe, seed, helper or shell; `aho sandbox destroy` it or use a new session".into(),
            );
        }
    } else {
        // Failure leaves no 'config.json' success marker and therefore never resumes a partial image.
        if dir.join("image").exists() {
            return Err("incomplete sandbox creation; `aho sandbox destroy` it before retrying".into());
        }
        copy_tree(&cfg.rootfs, &dir.join("image"), &[])?;
        // aho's own state inside the project (sessions, worktrees, sockets) never enters the seed.
        let skip = [cfg.workspace.join(".aho"), dir.parent().map(Path::to_path_buf).unwrap_or_default()];
        copy_tree(&cfg.workspace, &dir.join("workspace"), &skip)?;
        for name in ["volume", "shadow", "root", "staging"] {
            private_dir(&dir.join(name))?;
        }
        atomic_json(&saved, &cfg)?;
    }
    let attachment = network.map(|n| n.attach(NIC)).transpose().map_err(err)?;
    let nic_path = dir.join("nic.json");
    match (&attachment, fs::read(&nic_path).ok()) {
        (Some(a), Some(bytes)) => {
            let nic: Nic = serde_json::from_slice(&bytes).map_err(err)?;
            if nic != a.nic {
                return Err("the sandbox's persisted NIC differs from its network; destroy the sandbox before changing its subnet".into());
            }
        }
        (Some(a), None) => atomic_json(&nic_path, &a.nic)?,
        (None, Some(_)) => {}
        (None, None) => return Err("the sandbox was never attached to its network; open its session first".into()),
    }
    let keeper_path = dir.join("keeper.json");
    let existing = fs::read(&keeper_path).ok().and_then(|b| serde_json::from_slice::<Keeper>(&b).ok()).filter(|k| check_keeper(k).is_ok());
    let keeper = match existing {
        Some(k) => k,
        None => start_keeper(&cfg, &dir, &keeper_path)?,
    };
    let bridge = match attachment {
        Some(a) => Some(bridge(&dir, a.stream)?),
        None => None,
    };
    Ok(Core { cfg, keeper, bridge })
}

fn start_keeper(cfg: &NsConfig, dir: &Path, keeper_path: &Path) -> Result<Keeper> {
    let mut child = Command::new(&cfg.helper)
        .arg("keeper")
        .arg(dir)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start the namespace helper: {e}"))?;
    let stdout = child.stdout.take().ok_or("helper stdout missing")?;
    let stderr = child.stderr.take().ok_or("helper stderr missing")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let r = io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(r);
    });
    let ready = rx.recv_timeout(Duration::from_secs(20));
    if !matches!(&ready, Ok(Ok(s)) if s.trim() == "READY") {
        let _ = child.kill();
        let _ = child.wait();
        let mut text = String::new();
        let _ = io::BufReader::new(stderr).read_to_string(&mut text);
        return Err(format!(
            "the sandbox could not start (it needs unprivileged user namespaces, overlayfs, /dev/net/tun and ip): {}",
            text.trim()
        ));
    }
    // Reap if it later exits; the detached keeper outlives the process that started it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    drop(stderr);
    serde_json::from_slice(&fs::read(keeper_path).map_err(err)?).map_err(err)
}

/// Plugs the keeper's in-namespace NIC into the network attachment. The socket is reached
/// through `/proc/self/fd/<dir>`, so the state directory's depth never hits `sun_path`'s limit.
fn bridge(dir: &Path, mut local: UnixStream) -> Result<UnixStream> {
    let handle = File::open(dir).map_err(err)?;
    let peer = UnixStream::connect(format!("/proc/self/fd/{}/net.sock", handle.as_raw_fd()))
        .map_err(|e| format!("the sandbox NIC socket: {e}"))?;
    drop(handle);
    let bridge = peer.try_clone().map_err(err)?;
    let mut local_read = local.try_clone().map_err(err)?;
    let mut peer_write = peer.try_clone().map_err(err)?;
    let mut peer_read = peer;
    std::thread::spawn(move || {
        let _ = io::copy(&mut local_read, &mut peer_write);
        let _ = peer_write.shutdown(std::net::Shutdown::Both);
        let _ = local_read.shutdown(std::net::Shutdown::Both);
    });
    std::thread::spawn(move || {
        let _ = io::copy(&mut peer_read, &mut local);
        let _ = local.shutdown(std::net::Shutdown::Both);
        let _ = peer_read.shutdown(std::net::Shutdown::Both);
    });
    Ok(bridge)
}

/// (created, live keeper pid, NIC) of the sandbox in `dir`, starting nothing.
pub(crate) fn inspect(dir: &Path) -> (bool, Option<u32>, Option<Nic>) {
    let exists = dir.join("config.json").is_file();
    let keeper =
        fs::read(dir.join("keeper.json")).ok().and_then(|b| serde_json::from_slice::<Keeper>(&b).ok()).filter(|k| check_keeper(k).is_ok());
    let nic = fs::read(dir.join("nic.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    (exists, keeper.map(|k| k.pid), nic)
}

pub(crate) fn destroy(dir: &Path) -> Result<bool> {
    match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(err(e)),
        Ok(m) if !m.is_dir() || m.file_type().is_symlink() => return Err(format!("{} is not a sandbox directory", dir.display())),
        Ok(_) => {}
    }
    let _lock = lock(&dir.join("lifecycle.lock"))?;
    let keeper =
        fs::read(dir.join("keeper.json")).ok().and_then(|b| serde_json::from_slice::<Keeper>(&b).ok()).filter(|k| check_keeper(k).is_ok());
    if let Some(keeper) = keeper {
        let fd = pidfd(&keeper)?;
        // SAFETY: this pidfd identifies the checked keeper, not a recycled PID.
        cv(unsafe {
            libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0) as i32
        })?;
        let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll receives one initialized descriptor.
        if unsafe { libc::poll(&mut poll, 1, 5000) } <= 0 {
            return Err("namespace keeper did not stop; persistent storage was not removed".into());
        }
    }
    // Enter controllers monitor the keeper pidfd; wait for their shared lifetime locks.
    let _users = lock(&dir.join("users.lock"))?;
    fs::remove_dir_all(dir).map_err(err)?;
    Ok(true)
}
fn pidfd(k: &Keeper) -> Result<OwnedFd> {
    // SAFETY: pidfd_open has no pointer arguments.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, k.pid, 0) as i32 };
    if raw < 0 {
        return Err(err(io::Error::last_os_error()));
    }
    // SAFETY: the successful syscall transfers this descriptor to us.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    check_keeper(k)?;
    Ok(fd)
}
fn mount(source: Option<&Path>, target: &Path, kind: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<()> {
    let src = source.map(c).transpose()?;
    let dst = c(target)?;
    let kind = kind.map(c).transpose()?;
    let data = data.map(c).transpose()?;
    // SAFETY: all optional C strings stay alive for mount; null denotes an omitted argument.
    cv(unsafe {
        libc::mount(
            src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            dst.as_ptr(),
            kind.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref().map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    })
    .map_err(|e| format!("mount {}: {e}", target.display()))
}
fn bind(source: &Path, target: &Path, readonly: bool) -> Result<()> {
    mount(Some(source), target, None, libc::MS_BIND, None)?;
    mount(
        None,
        target,
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV | if readonly { libc::MS_RDONLY } else { 0 },
        None,
    )
}
fn tmpfs(path: &Path, mode: &str) -> Result<()> {
    mount(None, path, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some(mode))
}
fn ensure_real_dir(root: &Path, name: &str) -> Result<()> {
    let p = root.join(name);
    if fs::symlink_metadata(&p).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        return Err(format!("recipe reserved mountpoint /{name} is not a real directory"));
    }
    fs::create_dir_all(p).map_err(err)
}
fn setup_mounts(dir: &Path, nic: &Nic) -> Result<()> {
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let staging = dir.join("staging");
    tmpfs(&staging, "mode=0700")?;
    for n in ["upper", "work"] {
        fs::create_dir(staging.join(n)).map_err(err)?;
    }
    let root = dir.join("root");
    mount(
        None,
        &root,
        Some("overlay"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            dir.join("image").display(),
            staging.join("upper").display(),
            staging.join("work").display()
        )),
    )?;
    for n in ["workspace", "session", "tmp", "dev", "proc", "checkpoint", ".oldroot", "etc"] {
        ensure_real_dir(&root, n)?;
    }
    let resolv = root.join("etc/resolv.conf");
    if fs::symlink_metadata(&resolv).is_ok() {
        fs::remove_file(&resolv).map_err(err)?;
    }
    fs::write(&resolv, format!("nameserver {}\noptions timeout:2 attempts:2\n", nic.gateway_v4)).map_err(err)?;
    mount(None, &root, None, libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)?;
    // The workspace is the sandbox's own copy of the seed, bound read-write: no overlay upper
    // layer on the state filesystem, which may itself be overlayfs (aho inside a container).
    bind(&dir.join("workspace"), &root.join("workspace"), false)?;
    bind(&dir.join("volume"), &root.join("session"), false)?;
    tmpfs(&root.join("tmp"), "mode=1777")?;
    // /dev permits only these bound character devices; no host /dev tree or tun device.
    mount(None, &root.join("dev"), Some("tmpfs"), libc::MS_NOSUID | libc::MS_NOEXEC, Some("mode=0755"))?;
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let target = root.join("dev").join(name);
        File::create(&target).map_err(err)?;
        mount(Some(&Path::new("/dev").join(name)), &target, None, libc::MS_BIND, None)?;
    }
    fs::create_dir(root.join("dev/shm")).map_err(err)?;
    tmpfs(&root.join("dev/shm"), "mode=1777")?;
    fs::create_dir(root.join("dev/pts")).map_err(err)?;
    mount(None, &root.join("dev/pts"), Some("devpts"), libc::MS_NOSUID | libc::MS_NOEXEC, Some("newinstance,ptmxmode=0666,mode=0620"))?;
    for (name, target) in [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        std::os::unix::fs::symlink(target, root.join("dev").join(name)).map_err(err)?;
    }
    Ok(())
}
fn user_namespace() -> Result<()> {
    // SAFETY: namespace calls happen only in the dedicated, single-threaded helper.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    cv(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
    fs::write("/proc/self/setgroups", "deny").map_err(err)?;
    fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).map_err(err)?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).map_err(err)?;
    cv(unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) })
}
#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    data: [u8; 24],
}
fn tap(nic: &Nic) -> Result<File> {
    let f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open("/dev/net/tun").map_err(err)?;
    let mut req = IfReq { name: [0; libc::IFNAMSIZ], data: [0; 24] };
    for (d, s) in req.name.iter_mut().zip(b"eth0") {
        *d = *s as libc::c_char;
    }
    let flags = (libc::IFF_TAP | libc::IFF_NO_PI) as i16;
    req.data[..2].copy_from_slice(&flags.to_ne_bytes());
    // SAFETY: Linux TUNSETIFF reads/writes a correctly sized ifreq buffer.
    cv(unsafe { libc::ioctl(f.as_raw_fd(), 0x400454ca as libc::c_ulong, &mut req) })?;
    let mac = nic.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
    let run = |args: &[&str]| -> Result<()> {
        let o = Command::new("ip").args(args).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").output().map_err(err)?;
        if o.status.success() { Ok(()) } else { Err(format!("ip {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr))) }
    };
    run(&["link", "set", "lo", "up"])?;
    run(&["link", "set", "eth0", "address", &mac])?;
    run(&["link", "set", "eth0", "up"])?;
    run(&["addr", "add", &format!("{}/{}", nic.ipv4, nic.prefix_v4), "dev", "eth0"])?;
    run(&["-6", "addr", "add", &format!("{}/{}", nic.ipv6, nic.prefix_v6), "dev", "eth0", "nodad"])?;
    run(&["route", "add", "default", "via", &nic.gateway_v4.to_string(), "dev", "eth0"])?;
    run(&["-6", "route", "add", "default", "via", &nic.gateway_v6.to_string(), "dev", "eth0"])?;
    Ok(f)
}
fn relay(listener: UnixListener, mut tap: File) -> Result<()> {
    listener.set_nonblocking(true).map_err(err)?;
    let mut peer: Option<UnixStream> = None;
    let mut incoming = Vec::new();
    let mut outgoing = VecDeque::new();
    let mut bytes = [0u8; 65536];
    loop {
        let mut poll = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: tap.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd {
                fd: peer.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN | if outgoing.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
        ];
        // SAFETY: poll has the exact initialized array length.
        let n = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err(io::Error::last_os_error()));
        }
        if poll[0].revents & libc::POLLIN != 0
            && let Ok((p, _)) = listener.accept()
        {
            p.set_nonblocking(true).map_err(err)?;
            peer = Some(p);
            incoming.clear();
            outgoing.clear();
        }
        if poll[1].revents & libc::POLLIN != 0
            && let Ok(n) = tap.read(&mut bytes)
            && n >= 14
            && peer.is_some()
            && outgoing.len() + n + 4 <= 2 << 20
        {
            outgoing.extend((n as u32).to_be_bytes());
            outgoing.extend(&bytes[..n]);
        }
        let mut disconnect = poll[2].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        if let Some(p) = peer.as_mut() {
            if poll[2].revents & libc::POLLIN != 0 {
                match p.read(&mut bytes) {
                    Ok(0) => disconnect = true,
                    Ok(n) => incoming.extend_from_slice(&bytes[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => disconnect = true,
                }
            }
            let mut consumed = 0;
            while incoming.len() - consumed >= 4 {
                let len = u32::from_be_bytes(incoming[consumed..consumed + 4].try_into().map_err(err)?) as usize;
                if !(14..=65535).contains(&len) {
                    disconnect = true;
                    break;
                }
                if incoming.len() - consumed < len + 4 {
                    break;
                }
                let _ = tap.write(&incoming[consumed + 4..consumed + 4 + len]);
                consumed += len + 4;
            }
            incoming.drain(..consumed);
            if incoming.len() > 65539 {
                disconnect = true;
            }
            if poll[2].revents & libc::POLLOUT != 0 && !outgoing.is_empty() {
                match p.write(outgoing.as_slices().0) {
                    Ok(n) => {
                        outgoing.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => disconnect = true,
                }
            }
        }
        if disconnect {
            peer = None;
            incoming.clear();
            outgoing.clear();
        }
    }
}
fn keeper(dir: &Path) -> Result<()> {
    let _owner = lock(&dir.join("keeper.lock"))?;
    let nic: Nic = serde_json::from_slice(&fs::read(dir.join("nic.json")).map_err(err)?).map_err(err)?;
    // Bound relative to the state directory: a deep project path never exceeds `sun_path`.
    std::env::set_current_dir(dir).map_err(err)?;
    if Path::new("net.sock").exists() {
        fs::remove_file("net.sock").map_err(err)?;
    }
    let listener = UnixListener::bind("net.sock").map_err(err)?;
    user_namespace()?;
    let tap = tap(&nic)?;
    setup_mounts(dir, &nic)?;
    let pid = std::process::id();
    atomic_json(&dir.join("keeper.json"), &Keeper { pid, start: start_time(pid)? })?;
    println!("READY");
    io::stdout().flush().map_err(err)?;
    // Readiness pipes must not pin a daemon or receive later output.
    let null = File::options().read(true).write(true).open("/dev/null").map_err(err)?;
    // SAFETY: dup2 replaces only stdio using a live descriptor.
    for fd in [0, 1, 2] {
        cv(unsafe { libc::dup2(null.as_raw_fd(), fd) })?;
    }
    relay(listener, tap)
}

fn drop_privileges() -> Result<()> {
    // NOROOT, NOROOT_LOCKED, NO_SETUID_FIXUP and NO_SETUID_FIXUP_LOCKED prevent
    // exec of a uid-0 binary from regaining capabilities. All bounding caps go too.
    cv(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, 0x0f, 0, 0, 0) })?;
    for cap in 0..64 {
        let r = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
            return Err(err(io::Error::last_os_error()));
        }
    }
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header { version: 0x20080522, pid: 0 };
    let data = [Data { effective: 0, permitted: 0, inheritable: 0 }, Data { effective: 0, permitted: 0, inheritable: 0 }];
    // SAFETY: capset's v3 ABI expects one header and two data words.
    cv(unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) as i32 })?;
    cv(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    // Prevent inherited descriptors (including host directories) from crossing the boundary.
    cv(unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) as i32 })
}
fn pivot(dir: &Path, filesystem: bool) -> Result<()> {
    let root = dir.join("root");
    if filesystem {
        bind(&dir.join("shadow"), &root.join("checkpoint"), false)?;
    }
    std::env::set_current_dir(&root).map_err(err)?;
    let new = c(".")?;
    let old = c(".oldroot")?;
    // SAFETY: both paths are real directories in a private mount namespace; root is a mountpoint.
    cv(unsafe { libc::syscall(libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) as i32 })?;
    std::env::set_current_dir("/").map_err(err)?;
    // A user namespace may mount a new procfs only while a fully visible one is still in its
    // mount namespace (the kernel's mnt_too_revealing check): mount it before the old root goes.
    mount(None, Path::new("/proc"), Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY, None)?;
    let old = c("/.oldroot")?;
    cv(unsafe { libc::umount2(old.as_ptr(), libc::MNT_DETACH) })?;
    Ok(())
}
fn payload(mode: &str, data: &str) -> Result<()> {
    drop_privileges()?;
    // The helper is single-threaded here; erase every host environment variable before
    // starting the filesystem runtime or executing a recipe command.
    let keys: Vec<_> = std::env::vars_os().map(|(k, _)| k).collect();
    for key in keys {
        unsafe { std::env::remove_var(key) };
    }
    for (k, v) in
        [("PATH", "/usr/local/bin:/usr/bin:/bin:/session/bin"), ("HOME", "/session"), ("TMPDIR", "/tmp"), ("TERM", "xterm-256color")]
    {
        unsafe { std::env::set_var(k, v) };
    }
    if mode == "fs" {
        let request: crate::fs::Request = serde_json::from_reader(io::stdin()).map_err(err)?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(err)?;
        let response = rt.block_on(crate::fs::serve(request));
        serde_json::to_writer(io::stdout(), &response).map_err(err)?;
        io::stdout().flush().map_err(err)?;
        Ok(())
    } else if mode == "exec" {
        let spec: crate::proc::Command = serde_json::from_str(data).map_err(err)?;
        let program = spec.argv.first().ok_or("empty namespace command")?;
        let error = Command::new(program).args(&spec.argv[1..]).current_dir(&spec.cwd).envs(spec.env).exec();
        Err(format!("cannot execute {program}: {error}"))
    } else {
        Err("unknown namespace helper operation".into())
    }
}
fn mirror(status: i32) -> ! {
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        };
        std::process::exit(128 + sig);
    }
    std::process::exit(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 125 })
}
fn enter(dir: &Path, k: Keeper, mode: &str, data: &str) -> Result<()> {
    let users = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("users.lock"))
        .map_err(err)?;
    cv(unsafe { libc::flock(users.as_raw_fd(), libc::LOCK_SH) })?;
    let keeper_fd = pidfd(&k)?;
    // Open every namespace before setns. File descriptors pin the checked namespaces.
    let spaces = ["user", "mnt", "net", "ipc", "uts"]
        .into_iter()
        .map(|n| File::open(format!("/proc/{}/ns/{n}", k.pid)).map_err(err))
        .collect::<Result<Vec<_>>>()?;
    check_keeper(&k)?;
    let _snapshot_lock = if mode == "fs" { Some(lock(&dir.join("fs.lock"))?) } else { None };
    for space in &spaces {
        cv(unsafe { libc::setns(space.as_raw_fd(), 0) })?;
    }
    cv(unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) })?;
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let mut pipe = [-1; 2];
    cv(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let read_pipe = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
    let write_pipe = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
    let controller = Keeper { pid: std::process::id(), start: start_time(std::process::id())? };
    let controller_fd = pidfd(&controller)?;
    let init = unsafe { libc::fork() };
    if init < 0 {
        return Err(err(io::Error::last_os_error()));
    }
    if init == 0 {
        drop(read_pipe);
        // The outer controller dying (kill, timeout, daemon crash) must kill namespace PID1.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } < 0 {
            unsafe { libc::_exit(125) }
        }
        // Parent PIDs are hidden across this boundary; pidfd closes the fork/prctl race.
        let mut parent = libc::pollfd { fd: controller_fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut parent, 1, 0) } != 0 {
            unsafe { libc::_exit(125) }
        }
        drop(controller_fd);
        if let Err(e) = pivot(dir, mode == "fs") {
            eprintln!("{e}");
            unsafe { libc::_exit(125) }
        }
        let child = unsafe { libc::fork() };
        if child < 0 {
            unsafe { libc::_exit(125) }
        }
        if child == 0 {
            drop(write_pipe);
            drop(spaces);
            drop(keeper_fd);
            drop(users);
            drop(_snapshot_lock);
            let code = match payload(mode, data) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    126
                }
            };
            unsafe { libc::_exit(code) }
        }
        let mut status = 0;
        loop {
            let got = unsafe { libc::waitpid(child, &mut status, 0) };
            if got == child {
                break;
            }
            if got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                status = 125 << 8;
                break;
            }
        }
        // Killing namespace PID1 kills all descendants, even setsid()/double-fork escapes.
        // Explicit kill/reap first prevents output pipes from waiting on an orphan.
        unsafe {
            libc::kill(-1, libc::SIGKILL);
        }
        loop {
            let got = unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) };
            if got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
        let raw = status.to_ne_bytes();
        unsafe {
            libc::write(write_pipe.as_raw_fd(), raw.as_ptr().cast(), raw.len());
            libc::_exit(0)
        }
    }
    drop(controller_fd);
    drop(write_pipe);
    drop(spaces);
    let mut status = 0;
    loop {
        let got = unsafe { libc::waitpid(init, &mut status, libc::WNOHANG) };
        if got == init {
            break;
        }
        if got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(err(io::Error::last_os_error()));
        }
        let mut p = libc::pollfd { fd: keeper_fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 20) } > 0 {
            unsafe {
                libc::kill(init, libc::SIGKILL);
            }
        }
    }
    let mut raw = [0u8; 4];
    let mut pipe = File::from(read_pipe);
    if pipe.read_exact(&mut raw).is_ok() {
        status = i32::from_ne_bytes(raw);
    }
    mirror(status)
}

/// Entrypoint for the dedicated executable. Never call this inside the daemon.
pub fn helper_main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("keeper") if args.len() == 3 => keeper(Path::new(&args[2])),
        Some("enter") if args.len() == 7 => enter(
            Path::new(&args[2]),
            Keeper { pid: args[3].parse().map_err(err)?, start: args[4].parse().map_err(err)? },
            &args[5],
            &args[6],
        ),
        _ => Err("usage: aho-ns-helper keeper STATE | enter STATE PID START exec|fs PAYLOAD (internal protocol)".into()),
    }
}
