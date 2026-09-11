//! In-process, unprivileged Ethernet switch and host-socket NAT for sandbox worlds.
//! TCP is terminated by smoltcp; no host interface is exposed to a guest.
#![forbid(unsafe_code)]

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: net-userspace-control")]
mod control;
mod device;
#[allow(clippy::disallowed_methods, reason = "authority: net-userspace-dns")]
mod dns;
pub mod operator;
mod packet;
mod policy;
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: net-userspace-runtime")]
mod runtime;
mod services;
#[cfg(unix)]
pub mod session;
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: net-userspace-store")]
pub(crate) mod store;
#[cfg(unix)]
pub use control::{ConfigSink, Remote};
#[cfg(unix)]
pub use session::{NetSettings, UserspaceNet, open_session_network, state_dir};
#[cfg(unix)]
pub use store::remove as remove_stopped;

use aho_net::{Attachment, FlowSink, NetError, Network, NetworkConfig, NetworkStatus};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub const ENFORCED_BY: &str = "aho-net Ethernet-only NIC; smoltcp TCP and policy-gated host-socket NAT44/NAT66";

#[derive(Clone)]
pub struct UserspaceNetwork {
    #[cfg(unix)]
    client: Arc<Client>,
}
#[cfg(unix)]
struct Client {
    tx: std::sync::mpsc::SyncSender<runtime::Command>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    control: Mutex<Option<std::thread::JoinHandle<()>>>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    observer: control::ConfigSlot,
}
#[cfg(unix)]
impl Drop for Client {
    fn drop(&mut self) {
        self.stopping.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.tx.send(runtime::Command::Stop);
        for thread in [&mut self.control, &mut self.thread] {
            if let Ok(handle) = thread.get_mut()
                && let Some(handle) = handle.take()
            {
                let _ = handle.join();
            }
        }
    }
}
impl UserspaceNetwork {
    /// Opens or resumes a locked private state directory. Persisted settings win on resume;
    /// use Network::configure for an explicit operator replacement. While open, the network
    /// answers `Remote` on `<state_dir>/control.sock`.
    pub fn open(state_dir: impl AsRef<Path>, config: NetworkConfig) -> Result<Self, NetError> {
        #[cfg(unix)]
        {
            let (tx, thread) = runtime::start(state_dir.as_ref(), config)?;
            let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observer = control::ConfigSlot::default();
            let client = Client {
                tx: tx.clone(),
                thread: Mutex::new(Some(thread)),
                control: Mutex::new(None),
                stopping: stopping.clone(),
                observer: observer.clone(),
            };
            // On failure `client` drops here and stops the worker it already started.
            let control = control::serve(state_dir.as_ref(), tx, observer, stopping)?;
            if let Ok(mut slot) = client.control.lock() {
                *slot = control;
            }
            Ok(Self { client: Arc::new(client) })
        }
        #[cfg(not(unix))]
        {
            let _ = (state_dir, config);
            Err(NetError::Unavailable("aho-net requires Unix Ethernet socketpairs".into()))
        }
    }
    /// Told of each configuration applied through `Remote` (not of `Network::configure` here).
    #[cfg(unix)]
    pub fn set_config_sink(&self, sink: Option<ConfigSink>) {
        if let Ok(mut slot) = self.client.observer.lock() {
            *slot = sink;
        }
    }
    #[cfg(unix)]
    fn request<T>(&self, command: impl FnOnce(std::sync::mpsc::Sender<Result<T, NetError>>) -> runtime::Command) -> Result<T, NetError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.client.tx.send(command(tx)).map_err(|_| NetError::Unavailable("network worker stopped".into()))?;
        rx.recv().map_err(|_| NetError::Unavailable("network worker stopped".into()))?
    }
}
impl Network for UserspaceNetwork {
    fn attach(&self, name: &str) -> Result<Attachment, NetError> {
        #[cfg(unix)]
        {
            self.request(|reply| runtime::Command::Attach(name.into(), reply))
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(NetError::Unavailable("Unix required".into()))
        }
    }
    fn status(&self) -> Result<NetworkStatus, NetError> {
        #[cfg(unix)]
        {
            self.request(runtime::Command::Status)
        }
        #[cfg(not(unix))]
        {
            Err(NetError::Unavailable("Unix required".into()))
        }
    }
    fn configure(&self, config: NetworkConfig) -> Result<(), NetError> {
        #[cfg(unix)]
        {
            self.request(|reply| runtime::Command::Configure(config, reply))
        }
        #[cfg(not(unix))]
        {
            let _ = config;
            Err(NetError::Unavailable("Unix required".into()))
        }
    }
    fn set_flow_sink(&self, sink: Option<FlowSink>) -> Result<(), NetError> {
        #[cfg(unix)]
        {
            self.request(|reply| runtime::Command::Sink(sink, reply))
        }
        #[cfg(not(unix))]
        {
            let _ = sink;
            Err(NetError::Unavailable("Unix required".into()))
        }
    }
}
