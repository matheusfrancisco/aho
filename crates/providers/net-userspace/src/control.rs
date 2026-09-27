//! The operator's path into a running network: `<state_dir>/control.sock`, one JSON request
//! line and one JSON reply line per connection. The socket lives in the private (0700) state
//! directory, so only the owning user reaches it: the same trust as the daemon socket. If no
//! process runs the network, `aho net` opens the state directory itself instead.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use aho_net::{NetError, NetworkConfig, NetworkStatus};
use serde::{Deserialize, Serialize};

use crate::runtime::Command;

/// Told of every configuration an operator applied through the control socket (the owner
/// records it; the operator's process may not write the owner's log).
pub type ConfigSink = Arc<dyn Fn(&NetworkConfig) + Send + Sync>;
pub(crate) type ConfigSlot = Arc<Mutex<Option<ConfigSink>>>;

const SOCKET: &str = "ctl";
/// `sun_path` is 104 bytes on macOS and 108 on Linux; stay under both with the terminator.
const MAX_SOCKET_PATH: usize = 100;
const MAX_REQUEST: u64 = 4 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Request {
    Status,
    Configure { config: Box<NetworkConfig> },
}

#[derive(Serialize, Deserialize)]
struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<NetworkStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Serves the control socket until `stopping`; the listener is removed on the way out.
/// `Ok(None)` when the state directory is too deep for a unix socket path: the network runs,
/// and `aho net` reports it cannot reach it rather than changing anything.
pub(crate) fn serve(
    dir: &Path,
    tx: SyncSender<Command>,
    observer: ConfigSlot,
    stopping: Arc<AtomicBool>,
) -> Result<Option<thread::JoinHandle<()>>, NetError> {
    let path = dir.join(SOCKET);
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Ok(None);
    }
    // The state lock is held, so a socket file here is a dead owner's leftover.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() || m.is_dir() => return Err(NetError::Invalid("network control path is not a socket".into())),
        Ok(_) => std::fs::remove_file(&path)?,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let handle = thread::Builder::new().name("aho-net-control".into()).spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => answer(stream, &tx, &observer),
                Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = std::fs::remove_file(&path);
    })?;
    Ok(Some(handle))
}

fn answer(stream: UnixStream, tx: &SyncSender<Command>, observer: &ConfigSlot) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let Ok(read) = stream.try_clone() else { return };
    let reply = match BufReader::new(read.take(MAX_REQUEST)).read_line(&mut line) {
        Ok(_) => match serde_json::from_str::<Request>(&line) {
            Ok(request) => run(request, tx, observer),
            Err(e) => Reply { status: None, error: Some(format!("bad control request: {e}")) },
        },
        Err(e) => Reply { status: None, error: Some(format!("control request: {e}")) },
    };
    let mut stream = stream;
    if let Ok(mut bytes) = serde_json::to_vec(&reply) {
        bytes.push(b'\n');
        let _ = stream.write_all(&bytes);
    }
}

fn run(request: Request, tx: &SyncSender<Command>, observer: &ConfigSlot) -> Reply {
    let stopped = || Reply { status: None, error: Some("network worker stopped".into()) };
    match request {
        Request::Status => {
            let (reply, rx) = mpsc::channel();
            if tx.send(Command::Status(reply)).is_err() {
                return stopped();
            }
            match rx.recv() {
                Ok(Ok(status)) => Reply { status: Some(status), error: None },
                Ok(Err(e)) => Reply { status: None, error: Some(e.to_string()) },
                Err(_) => stopped(),
            }
        }
        Request::Configure { config } => {
            let (reply, rx) = mpsc::channel();
            if tx.send(Command::Configure(*config, reply)).is_err() {
                return stopped();
            }
            match rx.recv() {
                Ok(Ok(())) => {
                    let reply = run(Request::Status, tx, observer);
                    let sink = observer.lock().ok().and_then(|slot| slot.clone());
                    if let (Some(sink), Some(status)) = (sink, &reply.status) {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(&status.config)));
                    }
                    reply
                }
                Ok(Err(e)) => Reply { status: None, error: Some(e.to_string()) },
                Err(_) => stopped(),
            }
        }
    }
}

/// A network another process runs, reached through its control socket.
pub struct Remote {
    socket: PathBuf,
}

impl Remote {
    /// `None` when no live process serves `state_dir` (no socket, or nobody listening).
    pub fn connect(state_dir: &Path) -> Option<Remote> {
        let socket = state_dir.join(SOCKET);
        UnixStream::connect(&socket).ok().map(|_| Remote { socket })
    }

    /// Whether a network in `state_dir` could serve a control socket at all.
    pub fn reachable_path(state_dir: &Path) -> bool {
        state_dir.join(SOCKET).as_os_str().len() <= MAX_SOCKET_PATH
    }

    fn call(&self, request: &Request) -> Result<NetworkStatus, NetError> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut bytes = serde_json::to_vec(request).map_err(|e| NetError::Invalid(e.to_string()))?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut line = String::new();
        BufReader::new(stream.take(MAX_REQUEST)).read_line(&mut line)?;
        let reply: Reply = serde_json::from_str(&line).map_err(|e| NetError::Unavailable(format!("network control reply: {e}")))?;
        match (reply.status, reply.error) {
            (_, Some(e)) => Err(NetError::Invalid(e)),
            (Some(status), None) => Ok(status),
            (None, None) => Err(NetError::Unavailable("empty network control reply".into())),
        }
    }

    pub fn status(&self) -> Result<NetworkStatus, NetError> {
        self.call(&Request::Status)
    }

    /// Replaces the running network's configuration; returns the status after the change.
    pub fn configure(&self, config: NetworkConfig) -> Result<NetworkStatus, NetError> {
        self.call(&Request::Configure { config: Box::new(config) })
    }
}
