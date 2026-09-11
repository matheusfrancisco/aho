use crate::protocol::{MAX_FRAME, MAX_INFLIGHT, REQUEST_SECONDS, Request, Response, WireError};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

type Pending = Arc<Mutex<HashMap<u64, mpsc::SyncSender<Result<Value, WireError>>>>>;
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) struct Connection {
    writer: Mutex<UnixStream>,
    pending: Pending,
    alive: Arc<AtomicBool>,
}
impl Connection {
    #[allow(clippy::disallowed_methods, reason = "authority: vm-guest-connect")]
    pub(crate) fn connect(path: &Path) -> Result<Self, WireError> {
        let writer = UnixStream::connect(path).map_err(|e| WireError::protocol(format!("cannot connect to guest agent socket `{}`: {e}; install/start vm-guest-agent in the guest and configure QEMU's org.aho.agent virtio port", path.display())))?;
        writer.set_write_timeout(Some(Duration::from_secs(REQUEST_SECONDS)))?;
        let mut reader = writer.try_clone()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let p = pending.clone();
        let a = alive.clone();
        std::thread::Builder::new()
            .name("vm-guest-replies".into())
            .spawn(move || {
                let error = loop {
                    match read_response(&mut reader) {
                        Ok(response) => {
                            if let Some(tx) = lock(&p).remove(&response.id) {
                                let _ = tx.send(response.result);
                            }
                        }
                        Err(error) => break error,
                    }
                };
                a.store(false, Ordering::Release);
                for (_, tx) in lock(&p).drain() {
                    let _ = tx.send(Err(error.clone()));
                }
            })
            .map_err(WireError::from)?;
        Ok(Self { writer: Mutex::new(writer), pending, alive })
    }
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub(crate) fn call(&self, request: &Request) -> Result<Value, WireError> {
        let bytes = serde_json::to_vec(request).map_err(|e| WireError::protocol(e.to_string()))?;
        if bytes.len() > MAX_FRAME {
            return Err(WireError::protocol("guest request exceeds 64 MiB frame limit; split the operation"));
        }
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = lock(&self.pending);
            if !self.alive() {
                return Err(WireError::protocol(
                    "guest agent disconnected; reconnect and retry after checking whether the last effect completed",
                ));
            }
            if pending.len() >= MAX_INFLIGHT {
                return Err(WireError::protocol("guest request concurrency limit reached; wait for outstanding operations"));
            }
            pending.insert(request.id, tx);
        }
        let write = {
            let mut writer = lock(&self.writer);
            writer.write_all(&(bytes.len() as u32).to_be_bytes()).and_then(|_| writer.write_all(&bytes))
        };
        if let Err(e) = write {
            lock(&self.pending).remove(&request.id);
            self.close();
            return Err(e.into());
        }
        match rx.recv_timeout(Duration::from_secs(REQUEST_SECONDS)) {
            Ok(value) => value,
            Err(e) => {
                lock(&self.pending).remove(&request.id);
                self.close();
                Err(WireError::protocol(format!(
                    "guest agent did not answer within {REQUEST_SECONDS}s ({e}); check its console before retrying: the operation may have completed"
                )))
            }
        }
    }
    pub(crate) fn close(&self) {
        self.alive.store(false, Ordering::Release);
        let _ = lock(&self.writer).shutdown(Shutdown::Both);
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}
fn read_response(reader: &mut UnixStream) -> Result<Response, WireError> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let size = u32::from_be_bytes(len) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(WireError::protocol("guest agent sent an invalid frame length; check protocol version"));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| WireError::protocol(format!("invalid guest agent JSON: {e}; install a matching vm-guest-agent")))
}
