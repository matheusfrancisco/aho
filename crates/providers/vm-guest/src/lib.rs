//! Host-side VM providers. All effects cross the guarded guest channel; no local execution.
#![forbid(unsafe_code)]

mod connection;
mod pipes;
pub mod protocol;
mod providers;

use connection::{Connection, lock};
use protocol::{Request, VERSION, WireError, field};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Guard = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct Guest {
    socket: PathBuf,
    workspace: String,
    guard: Guard,
    session: String,
    sequence: AtomicU64,
    channel: Mutex<Option<Arc<Connection>>>,
    boot: Mutex<Option<String>>,
}
impl Guest {
    pub fn connect(socket: PathBuf, workspace: String, guard: Guard) -> Result<Arc<Self>, String> {
        guard()?;
        if !workspace.starts_with('/') {
            return Err("guest workspace must be an explicit absolute path".into());
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
        let session = format!("{nonce:x}-{:x}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed));
        let guest = Arc::new(Self {
            socket,
            workspace,
            guard,
            session,
            sequence: AtomicU64::new(1),
            channel: Mutex::new(None),
            boot: Mutex::new(None),
        });
        guest.probe()?;
        Ok(guest)
    }
    pub fn fs(self: &Arc<Self>) -> Arc<dyn aho_fs::Fs> {
        Arc::new(providers::Remote(self.clone()))
    }
    pub fn subprocess(self: &Arc<Self>) -> Arc<dyn aho_proc::Subprocess> {
        Arc::new(providers::Remote(self.clone()))
    }
    pub fn terminals(self: &Arc<Self>) -> Arc<dyn aho_term::Terminals> {
        Arc::new(providers::Remote(self.clone()))
    }
    pub fn probe(&self) -> Result<(), String> {
        self.call_sync("probe", json!({})).map(|_| ()).map_err(|e| e.message)
    }
    pub fn disconnect(&self) {
        if let Some(channel) = lock(&self.channel).take() {
            channel.close();
        }
    }
    fn request(&self, op: &str, args: Value) -> Request {
        Request {
            id: self.sequence.fetch_add(1, Ordering::Relaxed),
            version: VERSION,
            session: self.session.clone(),
            workspace: self.workspace.clone(),
            op: op.into(),
            args,
        }
    }
    fn check(&self) -> Result<(), WireError> {
        (self.guard)().map_err(WireError::denied)
    }
    fn connection(&self) -> Result<Arc<Connection>, WireError> {
        let mut slot = lock(&self.channel);
        if let Some(channel) = slot.as_ref().filter(|c| c.alive()) {
            return Ok(channel.clone());
        }
        self.check()?;
        let channel = Arc::new(Connection::connect(&self.socket)?);
        let hello = channel.call(&self.request("hello", json!({})))?;
        let version: u32 = field(&hello, "version")?;
        let boot: String = field(&hello, "boot")?;
        if version != VERSION {
            return Err(WireError::protocol("guest protocol version mismatch; install matching vm-guest-agent"));
        }
        let mut previous = lock(&self.boot);
        if previous.as_ref().is_some_and(|old| old != &boot) {
            return Err(WireError::protocol(
                "guest agent restarted; prior process and terminal handles are gone. Reopen the VM world to start a new session",
            ));
        }
        *previous = Some(boot);
        *slot = Some(channel.clone());
        Ok(channel)
    }
    pub(crate) fn call_sync(&self, op: &str, args: Value) -> Result<Value, WireError> {
        self.check()?;
        let channel = self.connection()?;
        self.check()?;
        channel.call(&self.request(op, args))
    }
    pub(crate) async fn call(self: &Arc<Self>, op: &'static str, args: Value) -> Result<Value, WireError> {
        self.check()?;
        let guest = self.clone();
        tokio::task::spawn_blocking(move || guest.call_sync(op, args))
            .await
            .map_err(|e| WireError::protocol(format!("guest request worker failed: {e}")))?
    }
    pub(crate) fn release(self: &Arc<Self>, op: &'static str, id: u64) {
        if self.check().is_err() {
            return;
        }
        let guest = self.clone();
        let _ = std::thread::Builder::new().name("vm-guest-release".into()).spawn(move || {
            let _ = guest.call_sync(op, json!({"id":id}));
        });
    }
}
impl Drop for Guest {
    fn drop(&mut self) {
        self.disconnect();
    }
}
