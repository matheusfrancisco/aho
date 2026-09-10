//! QEMU's reconnecting Unix stream netdev forwards only to the bound aho-net network.
use crate::storage;
use aho_net::Network;
use aho_vm::VmError;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct Proxy {
    stop: Arc<AtomicBool>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: vm-network-listen")]
pub(crate) fn listen(path: &Path, name: String, network: Arc<dyn Network>) -> Result<Proxy, VmError> {
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    let lock = storage::lock(&path.with_extension("lock"))?;
    if path.exists() {
        std::fs::remove_file(path).map_err(storage::error)?;
    }
    let listener = UnixListener::bind(path).map_err(storage::error)?;
    listener.set_nonblocking(true).map_err(storage::error)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    std::thread::spawn(move || {
        let _lock = lock;
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => match network.attach(&name) {
                    Ok(attachment) => {
                        let stopping = stopping.clone();
                        std::thread::spawn(move || relay(stream, attachment.stream, stopping));
                    }
                    Err(e) => eprintln!("VM network attachment refused: {e}"),
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    eprintln!("VM network listener: {e}");
                    return;
                }
            }
        }
    });
    Ok(Proxy { stop })
}
#[cfg(unix)]
fn relay(a: std::os::unix::net::UnixStream, b: std::os::unix::net::UnixStream, stop: Arc<AtomicBool>) {
    use std::net::Shutdown;
    use std::time::Duration;
    let setup = || -> std::io::Result<_> {
        // BSD sockets inherit the listener's O_NONBLOCK on accept: without this the copy loop spins.
        a.set_nonblocking(false)?;
        a.set_read_timeout(Some(Duration::from_millis(200)))?;
        b.set_read_timeout(Some(Duration::from_millis(200)))?;
        a.set_write_timeout(Some(Duration::from_secs(2)))?;
        b.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok((a.try_clone()?, b.try_clone()?))
    };
    let (aa, bb) = match setup() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("VM network streams: {e}");
            return;
        }
    };
    let stopping = stop.clone();
    let forward = std::thread::spawn(move || copy(a, bb, stopping));
    copy(b, aa, stop);
    if forward.join().is_err() {
        eprintln!("VM network forwarding thread panicked");
    }
    fn copy(mut from: std::os::unix::net::UnixStream, mut to: std::os::unix::net::UnixStream, stop: Arc<AtomicBool>) {
        use std::io::{Read, Write};
        let mut buffer = [0u8; 65536];
        while !stop.load(Ordering::Acquire) {
            match from.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = to.write_all(&buffer[..n]) {
                        eprintln!("VM network write: {e}");
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    eprintln!("VM network read: {e}");
                    break;
                }
            }
        }
        if let Err(e) = from.shutdown(Shutdown::Both) {
            eprintln!("VM network shutdown: {e}");
        }
        if let Err(e) = to.shutdown(Shutdown::Both) {
            eprintln!("VM network shutdown: {e}");
        }
    }
}
#[cfg(not(unix))]
pub(crate) fn listen(_: &Path, _: String, _: Arc<dyn Network>) -> Result<Proxy, VmError> {
    Err(VmError::Locked("VM networking requires Linux or macOS".into()))
}
