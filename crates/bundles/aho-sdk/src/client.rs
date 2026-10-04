//! The Rust rpc client: the same protocol the TS and Python SDKs speak, over any duplex, such as
//! an in-process server ([`crate::Sdk::open_session`]), a child `aho rpc` ([`Client::spawn`]),
//! or a unix socket (`aho rpc --socket`, [`Client::connect_unix`]).
//!
//! One reader task routes frames: a frame whose `reply_to` names a waiting request resolves it;
//! everything else becomes an [`Event`] in wire order. Requests are multiplexed by id, so a
//! `prompt` that waits on an approval card does not block the `approve` that answers it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aho_log::{RewindMode, Transition};
use aho_session::AbortScope;
use aho_types::*;
use serde::de::DeserializeOwned;
use serde_json::json;
use surface_rpc::wire::{AbortParams, JobParams, PromptParams, RewindParams, SteerParams, SubscribeParams};
use surface_rpc::{ClientFrame, Hello, HelloOk, ServerFrame};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::SdkError;
use crate::events::{Answer, Answered, Event, Fold, Job, JobList, PromptReply, RewindReply, Subscribed};

type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// The `hello` params for a client instance, speaking every version this build speaks.
pub fn hello(client_instance: &str) -> Hello {
    Hello {
        min: surface_rpc::VERSION_MIN,
        max: surface_rpc::VERSION_MAX,
        client_instance: client_instance.to_string(),
        has_local_history: false,
        allow_takeover: false,
    }
}

/// Server launch options: `[program, globals…, args…, "rpc", --session <id>]`, the
/// argv `aho rpc` takes (and the TS/Python `spawn` options build).
#[derive(Clone, Debug, Default)]
pub struct Spawn {
    pub program: PathBuf,
    /// `--project <dir>`.
    pub project: Option<PathBuf>,
    /// `--set KEY=VALUE`; values are the raw `--set` text (a TOML scalar).
    pub sets: Vec<(String, String)>,
    /// `--disable <id>`.
    pub disable: Vec<String>,
    /// Resume this session (`--session <id>`).
    pub session: Option<String>,
    /// Extra arguments before `rpc`.
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

impl Spawn {
    pub fn new(program: impl Into<PathBuf>) -> Spawn {
        Spawn { program: program.into(), ..Spawn::default() }
    }

    pub fn set(mut self, key: &str, value: &str) -> Spawn {
        self.sets.push((key.to_string(), value.to_string()));
        self
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        if let Some(p) = &self.project {
            a.extend(["--project".into(), p.clone().into_os_string()]);
        }
        for (k, v) in &self.sets {
            a.extend(["--set".into(), format!("{k}={v}").into()]);
        }
        for id in &self.disable {
            a.extend(["--disable".into(), id.into()]);
        }
        a.extend(self.args.iter().cloned());
        a.push("rpc".into());
        if let Some(s) = &self.session {
            a.extend(["--session".into(), s.into()]);
        }
        a
    }
}

#[derive(Default)]
struct Pending {
    waiting: BTreeMap<u64, oneshot::Sender<ServerFrame>>,
    /// Set once by the reader when the connection ends; no request is accepted after.
    closed: Option<String>,
}

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Server-initiated frames in wire order. Ends when the connection closes. Unbounded: a client
/// that never reads its events keeps them in memory for the life of the connection.
pub struct Events {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl Events {
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
    /// The next event already received, without waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

/// One rpc connection to one session.
pub struct Client {
    v: u16,
    hello: HelloOk,
    next: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    writer: tokio::sync::Mutex<Option<Writer>>,
    events: Mutex<Option<Events>>,
    reader: JoinHandle<()>,
    child: Option<tokio::process::Child>,
}

impl Client {
    /// Handshakes over `reader`/`writer` and starts routing frames. Must run inside a tokio runtime.
    pub async fn connect<R, W>(reader: R, writer: W, hello: Hello) -> Result<Client, SdkError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut writer: Writer = Box::new(writer);
        let mut lines = BufReader::new(reader).lines();
        let params = serde_json::to_value(&hello).map_err(|e| SdkError::Protocol(format!("the hello params do not encode ({e})")))?;
        send(&mut writer, &ClientFrame { v: surface_rpc::VERSION_MAX, id: Some(0), method: "hello".into(), params }).await?;
        let ok = loop {
            let line = lines.next_line().await?.ok_or_else(|| {
                SdkError::Handshake("the server closed the connection before answering hello (did it start? check its stderr)".into())
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let f: ServerFrame = serde_json::from_str(&line)
                .map_err(|e| SdkError::Handshake(format!("the server's first line is not a frame ({e}); is this an aho rpc server?")))?;
            match f.event.as_str() {
                "hello_ok" if f.reply_to == Some(0) => {
                    break serde_json::from_value::<HelloOk>(f.payload)
                        .map_err(|e| SdkError::Protocol(format!("hello_ok does not decode ({e})")))?;
                }
                "error" => {
                    return Err(SdkError::Handshake(f.payload["message"].as_str().unwrap_or("refused without a message").to_string()));
                }
                other => return Err(SdkError::Handshake(format!("the server answered hello with `{other}`"))),
            }
        };
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(route(lines, pending.clone(), tx));
        Ok(Client {
            v: ok.version,
            hello: ok,
            next: AtomicU64::new(1),
            pending,
            writer: tokio::sync::Mutex::new(Some(writer)),
            events: Mutex::new(Some(Events { rx })),
            reader,
            child: None,
        })
    }

    /// Starts `spec` (e.g. `aho rpc`) with piped stdio and handshakes; its stderr is inherited.
    /// The child is killed if the client is dropped without [`Client::close`].
    #[allow(clippy::disallowed_methods, reason = "authority: sdk-client-spawn")]
    pub async fn spawn(spec: &Spawn, hello: Hello) -> Result<Client, SdkError> {
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(spec.argv())
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        if let Some(d) = &spec.cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn().map_err(|e| {
            SdkError::Handshake(format!("could not start {} ({e}); pass the path of an aho binary", spec.program.display()))
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(SdkError::Handshake("the server process has no piped stdio".into()));
        };
        let mut c = Client::connect(stdout, stdin, hello).await?;
        c.child = Some(child);
        Ok(c)
    }

    /// Connects to `aho rpc --socket <path>`.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods, reason = "authority: sdk-client-connect")]
    pub async fn connect_unix(path: &Path, hello: Hello) -> Result<Client, SdkError> {
        let s = tokio::net::UnixStream::connect(path).await.map_err(|e| {
            SdkError::Handshake(format!("nothing answers on {} ({e}); start it with `aho rpc --socket {}`", path.display(), path.display()))
        })?;
        let (r, w) = s.into_split();
        Client::connect(r, w, hello).await
    }

    /// The server's `hello_ok`: negotiated version, methods, session id (hex), log head.
    pub fn hello_ok(&self) -> &HelloOk {
        &self.hello
    }

    /// The event stream. There is one per connection; it holds every event since connect.
    pub fn events(&self) -> Result<Events, SdkError> {
        lk(&self.events)
            .take()
            .ok_or_else(|| SdkError::Config("this connection's event stream was already taken; keep the first `Events`".into()))
    }

    /// Any method by wire name; the reply's payload, or the server's error message.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, SdkError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = lk(&self.pending);
            if let Some(why) = &p.closed {
                return Err(SdkError::Closed(format!("{why}; `{method}` was not sent")));
            }
            p.waiting.insert(id, tx);
        }
        let frame = ClientFrame { v: self.v, id: Some(id), method: method.to_string(), params };
        let sent = match self.writer.lock().await.as_mut() {
            Some(w) => send(w, &frame).await,
            None => Err(SdkError::Closed("this client was closed".into())),
        };
        if let Err(e) = sent {
            lk(&self.pending).waiting.remove(&id);
            return Err(e);
        }
        let reply = match rx.await {
            Ok(f) => f,
            Err(_) => {
                let why = lk(&self.pending).closed.clone().unwrap_or_else(|| "the connection ended".into());
                return Err(SdkError::Closed(format!("{why} before answering `{method}`")));
            }
        };
        match reply.event.as_str() {
            "result" => Ok(reply.payload),
            "error" => Err(SdkError::Rpc {
                method: method.to_string(),
                message: reply.payload["message"].as_str().unwrap_or("the server gave no message").to_string(),
            }),
            other => Err(SdkError::Protocol(format!("the server answered `{method}` with a `{other}` frame, not result or error"))),
        }
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, SdkError> {
        let v = self.request(method, params).await?;
        serde_json::from_value(v).map_err(|e| SdkError::Protocol(format!("the `{method}` reply does not decode ({e})")))
    }

    fn encode<T: serde::Serialize>(method: &str, params: &T) -> Result<Value, SdkError> {
        serde_json::to_value(params).map_err(|e| SdkError::Protocol(format!("the `{method}` params do not encode ({e})")))
    }

    /// Runs one turn to idle and returns its final transition. Entries, stream chunks, and
    /// approval cards arrive on [`Client::events`] meanwhile.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<Transition, SdkError> {
        self.prompt_keyed(text, None).await
    }

    /// [`Client::prompt`] with a dedup key: a prompt whose key the log has seen is not run again.
    pub async fn prompt_keyed(&self, text: impl Into<String>, client_key: Option<String>) -> Result<Transition, SdkError> {
        let params = Self::encode("session.prompt", &PromptParams { text: text.into(), client_key })?;
        self.call::<PromptReply>("session.prompt", params).await.map(|r| r.transition)
    }

    /// Queues input for the running turn; injected at the next request boundary.
    pub async fn steer(&self, text: impl Into<String>, urgent: bool) -> Result<(), SdkError> {
        let params = Self::encode("session.steer", &SteerParams { text: text.into(), urgent })?;
        self.request("session.steer", params).await.map(|_| ())
    }

    /// Cancels the running turn's operations; session jobs keep running ([`Client::abort_scope`]).
    pub async fn abort(&self) -> Result<(), SdkError> {
        self.request("session.abort", json!({})).await.map(|_| ())
    }

    /// Cancels by scope: the turn (jobs keep running), everything including jobs, or one op.
    pub async fn abort_scope(&self, scope: AbortScope) -> Result<(), SdkError> {
        let p = match scope {
            AbortScope::Turn => AbortParams { scope: Some("turn".into()), op: None },
            AbortScope::All => AbortParams { scope: Some("all".into()), op: None },
            AbortScope::Op(op) => AbortParams { scope: Some("op".into()), op: Some(op) },
        };
        let params = Self::encode("session.abort", &p)?;
        self.request("session.abort", params).await.map(|_| ())
    }

    /// Open session jobs: running, or finished and waiting for delivery.
    pub async fn list_jobs(&self) -> Result<Vec<Job>, SdkError> {
        self.call::<JobList>("job.list", json!({})).await.map(|l| l.jobs)
    }

    /// Cancels one running job; a turn op is refused.
    pub async fn kill_job(&self, op: OpId) -> Result<(), SdkError> {
        let params = Self::encode("job.kill", &JobParams { op })?;
        self.request("job.kill", params).await.map(|_| ())
    }

    /// Detaches a running turn op into a session job; refused when its tool cannot run as one.
    pub async fn detach_job(&self, op: OpId) -> Result<(), SdkError> {
        let params = Self::encode("job.detach", &JobParams { op })?;
        self.request("job.detach", params).await.map(|_| ())
    }

    /// Replays entries after `from` (all when `None`) as [`Event::Entry`], then streams live events.
    pub async fn subscribe(&self, from: Option<EntryId>) -> Result<Subscribed, SdkError> {
        let params = Self::encode("session.subscribe", &SubscribeParams { from })?;
        self.call("session.subscribe", params).await
    }

    pub async fn fold(&self) -> Result<Fold, SdkError> {
        self.call("session.fold", json!({})).await
    }

    /// Rewinds chat and/or code to a checkpoint; returns the entry the branch continues from.
    pub async fn rewind(&self, checkpoint: CheckpointId, mode: RewindMode) -> Result<EntryId, SdkError> {
        let mode = match mode {
            RewindMode::Both => "both",
            RewindMode::ChatOnly => "chat",
            RewindMode::CodeOnly => "code",
        };
        let params = Self::encode("session.rewind", &RewindParams { checkpoint: checkpoint.0, mode: Some(mode.into()) })?;
        self.call::<RewindReply>("session.rewind", params).await.map(|r| r.entry)
    }

    /// Answers approval card `id` (the card's [`crate::ApprovalCard::id`]). A card is answered once.
    pub async fn approve(&self, id: u64, answer: Answer) -> Result<Answered, SdkError> {
        let params = Self::encode("approval.answer", &answer.params(id))?;
        self.call("approval.answer", params).await
    }

    /// Closes the sending side and waits until the server answered what was in flight and hung
    /// up; for a spawned server, also waits for it to exit and returns its status.
    pub async fn close(self) -> Result<Option<ExitStatus>, SdkError> {
        if let Some(mut w) = self.writer.lock().await.take() {
            w.shutdown().await?;
        }
        self.reader.await.map_err(|e| SdkError::Server(format!("the client's reader task failed ({e})")))?;
        match self.child {
            Some(mut c) => Ok(Some(c.wait().await?)),
            None => Ok(None),
        }
    }
}

async fn send(w: &mut Writer, f: &ClientFrame) -> Result<(), SdkError> {
    let mut line = serde_json::to_string(f).map_err(|e| SdkError::Protocol(format!("a frame does not encode ({e})")))?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

async fn route<R: AsyncRead + Unpin>(mut lines: Lines<BufReader<R>>, pending: Arc<Mutex<Pending>>, events: mpsc::UnboundedSender<Event>) {
    let why = loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break "the server closed the connection".to_string(),
            Err(e) => break format!("reading from the server failed ({e})"),
        };
        if line.trim().is_empty() {
            continue;
        }
        let ev = match serde_json::from_str::<ServerFrame>(&line) {
            Ok(f) => match f.reply_to.and_then(|id| lk(&pending).waiting.remove(&id)) {
                Some(waiter) => {
                    // A requester that stopped waiting (its future was dropped) needs nothing.
                    drop(waiter.send(f));
                    continue;
                }
                None => Event::from_frame(f),
            },
            Err(e) => Event::Error(format!("the server sent a line that is not a frame ({e}); it was skipped")),
        };
        // Nobody holds the event stream: the event is dropped, replies still route.
        drop(events.send(ev));
    };
    let mut p = lk(&pending);
    p.closed = Some(why);
    p.waiting.clear();
}
