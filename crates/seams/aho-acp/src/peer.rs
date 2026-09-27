//! A bidirectional JSON-RPC peer over any byte stream: both ACP sides send requests and
//! notifications and answer the other side's. No process, socket, or file is opened here; the
//! caller hands in the reader and writer (stdio, a child's pipes, an in-memory duplex).
//!
//! * Every outgoing frame goes through one queue, so frames leave in the order they were
//!   produced: a `session/update` enqueued before a `session/prompt` result reaches the client
//!   first. [`Peer::request`] enqueues when called, not when its future is first polled.
//! * Incoming notifications are handled inline, in arrival order, before the next frame is read;
//!   incoming requests run as tasks, so a long `session/prompt` never blocks `session/cancel`.
//! * `$/cancel_request` aborts the named in-flight request and answers it with `-32800`.
//!   Exactly one response is ever sent per request.
//! * At end of input every outgoing request still waiting fails with [`RpcError::closed`];
//!   [`Driver::run`] then waits for the in-flight requests' responses, flushes, and returns.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use crate::jsonrpc::{FrameReader, Message, RequestId, RpcError, write_frame};
use crate::methods;

/// The future a request handler returns: owned, so it can outlive the call that produced it.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value, RpcError>> + Send + 'static>>;

/// The local side's methods.
pub trait Handler: Send + Sync + 'static {
    /// Answers one request; unknown methods answer [`RpcError::method_not_found`].
    fn request(&self, method: String, params: Value) -> HandlerFuture;
    /// Handles one notification, inline and in order. Must not block; unknown methods are ignored
    /// (JSON-RPC 2.0 §4.1: a notification is never answered).
    fn notification(&self, method: String, params: Value);
    /// The peer closed its side. Runs before [`Driver::run`] waits for in-flight requests, so a
    /// handler can wind down long requests (cancel running turns) instead of waiting them out.
    fn input_closed(&self) {}
}

enum Out {
    Frame(String),
    Close,
}

type Pending = HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>;

struct Shared {
    out: mpsc::UnboundedSender<Out>,
    pending: Mutex<Pending>,
    next: AtomicI64,
    closed: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn send(&self, m: &Message) -> Result<(), RpcError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RpcError::closed());
        }
        self.out.send(Out::Frame(m.encode())).map_err(|_| RpcError::closed())
    }
}

/// The sending half: cheap to clone, usable from any task.
#[derive(Clone)]
pub struct Peer {
    shared: Arc<Shared>,
}

impl Peer {
    /// Sends a request now and returns the future of its answer.
    pub fn request(&self, method: &str, params: Value) -> impl Future<Output = Result<Value, RpcError>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let id = RequestId::Number(self.shared.next.fetch_add(1, Ordering::SeqCst));
        lock(&self.shared.pending).insert(id.clone(), tx);
        let sent = self.shared.send(&Message::request(id.clone(), method, params));
        if sent.is_err() {
            lock(&self.shared.pending).remove(&id);
        }
        async move {
            sent?;
            rx.await.unwrap_or_else(|_| Err(RpcError::closed()))
        }
    }

    /// Typed [`Peer::request`]: serializes now, decodes the answer.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> impl Future<Output = Result<R, RpcError>> + Send + 'static {
        let encoded = serde_json::to_value(params).map_err(|e| RpcError::internal(format!("`{method}` params did not serialize: {e}")));
        let fut = encoded.map(|p| self.request(method, p));
        let method = method.to_string();
        async move {
            let v = fut?.await?;
            serde_json::from_value(v)
                .map_err(|e| RpcError::internal(format!("the peer's `{method}` result does not match the ACP v1 schema: {e}")))
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.shared.send(&Message::notification(method, params))
    }

    pub fn notify_typed<P: Serialize>(&self, method: &str, params: &P) -> Result<(), RpcError> {
        let v = serde_json::to_value(params).map_err(|e| RpcError::internal(format!("`{method}` params did not serialize: {e}")))?;
        self.notify(method, v)
    }

    /// True once the input side ended; every further send fails.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

/// The receiving half; [`Driver::run`] owns the streams until the peer closes its side.
pub struct Driver<R, W> {
    reader: R,
    writer: W,
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Out>,
}

/// Splits a connection into its sending [`Peer`] and its [`Driver`].
pub fn connection<R, W>(reader: R, writer: W) -> (Peer, Driver<R, W>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared { out, pending: Mutex::new(HashMap::new()), next: AtomicI64::new(1), closed: AtomicBool::new(false) });
    (Peer { shared: shared.clone() }, Driver { reader, writer, shared, rx })
}

async fn write_loop<W: AsyncWrite + Unpin>(mut w: W, mut rx: mpsc::UnboundedReceiver<Out>) -> std::io::Result<()> {
    while let Some(o) = rx.recv().await {
        match o {
            Out::Frame(f) => write_frame(&mut w, &f).await?,
            Out::Close => break,
        }
    }
    w.shutdown().await
}

type Running = Arc<Mutex<HashMap<RequestId, AbortHandle>>>;

impl<R, W> Driver<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Reads and dispatches until end of input (or a write failure), then fails what is still
    /// waiting, lets in-flight requests answer, flushes, and closes the writer.
    pub async fn run(self, handler: Arc<dyn Handler>) -> std::io::Result<()> {
        let Driver { reader, writer, shared, rx } = self;
        let mut writer = tokio::spawn(write_loop(writer, rx));
        let mut frames = FrameReader::new(BufReader::new(reader));
        let mut inflight: JoinSet<()> = JoinSet::new();
        let running: Running = Arc::new(Mutex::new(HashMap::new()));
        let mut writer_done = None;
        let read = loop {
            tokio::select! {
                frame = frames.next() => match frame {
                    Ok(Some(f)) => dispatch(f.decode(), &shared, &handler, &mut inflight, &running),
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                },
                Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
                w = &mut writer => {
                    writer_done = Some(w);
                    break Ok(());
                }
            }
        };
        shared.closed.store(true, Ordering::SeqCst);
        handler.input_closed();
        let waiting: Vec<_> = lock(&shared.pending).drain().map(|(_, tx)| tx).collect();
        for tx in waiting {
            // A caller that dropped its future no longer waits for this.
            tx.send(Err(RpcError::closed())).unwrap_or_default();
        }
        while inflight.join_next().await.is_some() {}
        let written = match writer_done {
            Some(w) => w,
            None => {
                // Fails only when the writer already stopped; awaiting it reports why.
                shared.out.send(Out::Close).unwrap_or_default();
                writer.await
            }
        };
        let written = written.map_err(std::io::Error::other).and_then(|r| r);
        read.and(written)
    }
}

fn respond(shared: &Shared, id: RequestId, result: Result<Value, RpcError>) {
    // After close the writer may be gone; the peer stopped reading anyway.
    shared.out.send(Out::Frame(Message::response(id, result).encode())).unwrap_or_default();
}

fn dispatch(
    msg: Result<Message, crate::jsonrpc::Rejected>,
    shared: &Arc<Shared>,
    handler: &Arc<dyn Handler>,
    inflight: &mut JoinSet<()>,
    running: &Running,
) {
    match msg {
        Err(r) => {
            if let Some(id) = r.reply_to {
                respond(shared, id, Err(r.error));
            }
        }
        Ok(Message::Response { id, result }) => {
            let waiter = lock(&shared.pending).remove(&id);
            if let Some(tx) = waiter {
                tx.send(result).unwrap_or_default();
            }
        }
        Ok(Message::Notification { method, params }) if method == methods::CANCEL_REQUEST => {
            let Some(id) = params.get("requestId").cloned().and_then(|v| serde_json::from_value::<RequestId>(v).ok()) else {
                return;
            };
            let mut g = lock(running);
            if let Some(h) = g.remove(&id) {
                h.abort();
                respond(shared, id, Err(RpcError::request_cancelled()));
            }
        }
        Ok(Message::Notification { method, params }) => handler.notification(method, params),
        Ok(Message::Request { id, method, params }) => {
            let mut g = lock(running);
            if g.contains_key(&id) {
                drop(g);
                respond(
                    shared,
                    id.clone(),
                    Err(RpcError::invalid_request(format!("request id {id} is already in flight; ids must be unique"))),
                );
                return;
            }
            let fut = handler.request(method, params);
            let (s, r, key) = (shared.clone(), running.clone(), id.clone());
            // The task removes itself under the same lock `$/cancel_request` takes, so exactly one
            // of them answers.
            let h = inflight.spawn(async move {
                let result = fut.await;
                if lock(&r).remove(&key).is_none() {
                    return;
                }
                respond(&s, key, result);
            });
            g.insert(id, h);
        }
    }
}
