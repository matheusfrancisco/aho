//! `12` "Process topology: one daemon, thin surfaces" — `aho serve`. One process owns every
//! session under [`SessionHost::sessions_dir`]; clients attach over a unix socket speaking
//! [`wire`] frames.
//!
//! Methods: `hello`, `session.list`, `session.open{id?}`,
//! `session.attach{session, client_instance?, has_local_history, from?, watch?}`,
//! `session.subscribe{session, from?}`, `session.detach{session}`,
//! `session.prompt{session, text, client_key?, wait?}`, `session.steer{session, text, urgent}`,
//! `session.abort{session}`, `session.resume{session}`, `session.command{session, kind}`,
//! `session.call{session, method, params}`, `session.reopen{session}`,
//! `approval.answer{id, allow | outcome, scope?}`, `daemon.reload{binary?, session?}`, `daemon.shutdown`.
//! Pushed events: `entry{session, entry}`, `stream{session, event}`, `idle{session, turn,
//! transition}`, `error{session, message}`, `approval.request{id, session, tool, cap, class, args,
//! rendered, why, reason, world, enforced_by, suggested_cut, preset_cut, epoch_next}`, `owner.lost{session, by}`.
//!
//! Authentication: the socket is `0600`, so every connection is this user; *driving* a session
//! (prompt, steer, abort, operator commands, host calls, reopen) needs the ownership an attach was
//! allowed (`12` takeover). A connection whose attach was refused is read-only: it watches.
//!
//! Reload is exec-in-place: intents are persisted, turns wound down and closed, the host's
//! [`SessionHost::before_exec`] runs, and the new image is exec'd with the initiator's connection
//! as its readiness descriptor — one byte on it tells the client that asked that the new binary
//! serves. Every other descriptor is close-on-exec, so other clients see end of file and reconnect.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_approval::{Approval, ApprovalRequest, ChannelApproval, Decided};
use aho_llm::StreamEvent;
use aho_log::{CancelReason, Entry, EntryId, EntryKind, GrantScope, LogStore, ProcessIdentity, RecoveryRole, SessionIndex, Takeover};
use aho_session::{Session, SessionEvent};
use aho_types::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::crash::{self, CrashScanner};
use crate::ready::{self, ReadyFd};
use crate::recovery;
use crate::takeover::{self, AttachRequest, Owner};
use crate::wire::{self, ClientFrame, ServerFrame};

pub mod client;

/// What the app provides: where logs live and how a session boots (composition, plan, loadout),
/// plus the hooks around what the host runs next to a session (drivers).
pub trait SessionHost: Send + Sync + 'static {
    /// Session logs live at `<dir>/<id as lowercase hex>.jsonl`, the name every CLI projection reads.
    fn sessions_dir(&self) -> PathBuf;
    /// Boots a session over `log`. The daemon opens the log itself so it can repair it (close a
    /// turn cut short by a reload, fork past a crash) before boot.
    fn boot(&self, id: SessionId, log: Arc<dyn LogStore>, approval: Arc<dyn Approval>) -> BoxFuture<'_, Result<Arc<Session>, String>>;
    /// `session.call{session, method, params}`: a method the host serves over a live session (the
    /// app: `caps.*`, `session.fold`, `session.rewind`, `plan-mode.enter`). The daemon has already
    /// checked that the caller drives the session. The default serves nothing.
    fn call<'a>(&'a self, _session: &'a Arc<Session>, method: &'a str, _params: Value) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Err(format!("this daemon serves no host method `{method}`, so nothing was done")) })
    }
    /// A host call that changes the booted plan. The daemon reserves turn admission before
    /// calling it and reopens afterward, including after a partially recorded failure.
    fn mutates(&self, _method: &str, _params: &Value) -> bool {
        false
    }
    /// The caller accepted this preset on a validated pending approval card. The host records
    /// and persists that preset; the daemon then answers the card, without reopening its turn.
    fn approval_preset<'a>(&'a self, _session: &'a Arc<Session>, _preset: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Err("this host does not support preset approval answers; allow, reject or cancel instead".into()) })
    }
    /// The daemon stops hosting session `id`: when it exits (shutdown or idle) and before
    /// `session.reopen` boots it again. Stop what runs next to the session; the returned problems
    /// go to the client that asked, or to the daemon's stderr.
    fn closing(&self, _id: SessionId) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// A reload is about to exec the new image: nothing of this process runs after it, not even
    /// destructors. Problems are reported in `reload.started`.
    fn before_exec(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// The reload's exec failed and this image keeps serving every live session: restart what
    /// [`SessionHost::before_exec`] stopped. Problems are reported in `reload.failed`.
    fn exec_failed(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
}

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub socket: PathBuf,
    /// Exit after this long with no clients and no running turns.
    pub idle_exit: Option<Duration>,
    /// How long a reload (or shutdown) lets running turns wind down after `abort()`.
    pub reload_grace: Duration,
    /// Interval between crash scans (a crash is confirmed on the second scan).
    pub scan_interval: Duration,
}

impl ServeOptions {
    /// Arguments the reloaded image is exec'd with.
    pub fn argv(&self) -> Vec<String> {
        vec!["serve".to_string(), "--socket".to_string(), self.socket.to_string_lossy().into_owned()]
    }
}

/// Why [`serve`] returned. A successful reload never returns: the process image is replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeExit {
    Idle,
    Shutdown,
}

type ConnId = u64;

enum Out {
    Frame(ServerFrame),
    /// Acknowledged once every frame queued before it is written.
    Flushed(oneshot::Sender<()>),
}

type Tx = mpsc::UnboundedSender<Out>;

struct Conn {
    tx: Tx,
    /// Sessions this connection attached to, as owner or read-only (a `watch: false` attach streams nothing).
    attached: BTreeSet<SessionId>,
    attachments: BTreeMap<SessionId, Stream>,
    /// Ends the connection (other clients during a reload).
    kick: Option<oneshot::Sender<()>>,
}

/// One session streamed to one connection; `last` is the newest entry it sent, so the stream can
/// be moved onto a rebooted session (`session.reopen`) without a gap or a repeat.
struct Stream {
    task: JoinHandle<()>,
    last: Arc<Mutex<Option<EntryId>>>,
}

struct OwnerSlot {
    conn: ConnId,
    client_instance: String,
    live: bool,
}

struct Live {
    session: Arc<Session>,
    owner: Option<OwnerSlot>,
    /// The running turn task (a prompt or a reload continuation) and its generation.
    turn: Option<(u64, JoinHandle<()>)>,
    /// Held across operator requests until replacement boot; shares Session's driver lock.
    mutation: Option<Mutation>,
    approvals: JoinHandle<()>,
}

struct Mutation {
    conn: ConnId,
    _guard: tokio::sync::OwnedMutexGuard<()>,
    head: Option<EntryId>,
}

struct PendingApproval {
    session: SessionId,
    conn: ConnId,
    preset: Option<String>,
    reply: oneshot::Sender<Decided>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<SessionId, Live>,
    crashed: BTreeSet<SessionId>,
    problems: BTreeMap<SessionId, String>,
    conns: BTreeMap<ConnId, Conn>,
    approvals: BTreeMap<u64, PendingApproval>,
    next_approval: u64,
    next_turn: u64,
    /// A reload or shutdown is under way: no new turns, attachments or approvals.
    closing: bool,
}

enum Control {
    Shutdown,
    Listener(UnixListener),
}

struct Daemon {
    opts: ServeOptions,
    host: Arc<dyn SessionHost>,
    dir: PathBuf,
    index: Arc<SessionIndex>,
    me: ProcessIdentity,
    state: Mutex<State>,
    scanner: Mutex<CrashScanner>,
    /// Serializes everything that opens a log file outside a live session (open, resume, scan):
    /// two `JsonlLog` instances appending to one file would reuse entry ids.
    files: tokio::sync::Mutex<()>,
    control: mpsc::UnboundedSender<Control>,
    next_conn: AtomicU64,
    id_seed: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs the daemon until it idles out or is shut down. See the module docs for the protocol.
///
/// Startup: refuses a socket another daemon answers on, removes a stale one, binds; resumes every
/// session with unconsumed reload intents (closes its interrupted turn, boots it, runs the
/// continuation); scans for crashed sessions now and every `scan_interval`; then signals `ready`.
#[allow(clippy::disallowed_methods, reason = "authority: daemon-serve-lifecycle")]
pub async fn serve(opts: ServeOptions, host: Arc<dyn SessionHost>, ready: Option<ReadyFd>) -> Result<ServeExit, String> {
    let dir = host.sessions_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!("could not create the sessions directory {} ({e}), so the daemon did not start. Check its permissions", dir.display())
    })?;
    let index = Arc::new(
        SessionIndex::open(dir.join("index.sqlite3"), &dir)
            .map_err(|e| format!("could not open the session index ({e}), so the daemon did not start"))?,
    );
    let mut listener = bind(&opts.socket).await?;
    let (control, mut controls) = mpsc::unbounded_channel();
    let d = Arc::new(Daemon {
        opts: opts.clone(),
        host,
        dir,
        index,
        me: ProcessIdentity { pid: std::process::id(), start_time: Timestamp::now() },
        state: Mutex::new(State::default()),
        scanner: Mutex::new(CrashScanner::new()),
        files: tokio::sync::Mutex::new(()),
        control,
        next_conn: AtomicU64::new(1),
        id_seed: AtomicU64::new(0),
    });
    d.resume_reloaded().await;
    d.scan().await;
    let scans = {
        let d = d.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(d.opts.scan_interval).await;
                d.scan().await;
            }
        })
    };
    if let Some(r) = ready {
        // A parent that stopped waiting must not stop the daemon.
        let _ = r.signal();
    }

    let mut conns = JoinSet::new();
    let tick = opts.idle_exit.map(|t| (t / 4).clamp(Duration::from_millis(10), Duration::from_secs(1)));
    let mut idle_since: Option<Instant> = None;
    let exit = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let id = d.next_conn.fetch_add(1, Ordering::Relaxed);
                    conns.spawn(d.clone().connection(id, stream));
                }
                // Out of descriptors and the like: back off and keep serving the clients we have.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
            Some(c) = controls.recv() => match c {
                Control::Shutdown => break ServeExit::Shutdown,
                Control::Listener(l) => listener = l,
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            _ = tokio::time::sleep(tick.unwrap_or(Duration::from_secs(1))), if tick.is_some() => {
                let limit = opts.idle_exit.unwrap_or_default();
                if d.busy() {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= limit {
                    break ServeExit::Idle;
                }
            }
        }
    };

    scans.abort();
    if exit == ServeExit::Shutdown {
        let _ = d.quiesce(CancelReason::Aborted, None).await;
    }
    let tasks = d.take_tasks();
    for t in &tasks {
        t.abort();
    }
    conns.shutdown().await;
    for t in tasks {
        let _ = t.await;
    }
    let hosted: Vec<SessionId> = d.st().sessions.keys().copied().collect();
    for sid in hosted {
        // Nobody is left to tell: the daemon's stderr is its log.
        for p in d.host.closing(sid).await {
            eprintln!("aho serve: session {}: {p}", sid.0);
        }
    }
    if std::fs::symlink_metadata(&opts.socket).is_ok_and(|m| m.file_type().is_socket()) {
        let _ = std::fs::remove_file(&opts.socket);
    }
    Ok(exit)
}

mod conn;
mod operator;
mod reload;
use self::conn::*;

impl Daemon {
    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn log_path(&self, sid: SessionId) -> PathBuf {
        self.dir.join(format!("{:x}.jsonl", sid.0))
    }

    fn busy(&self) -> bool {
        let st = self.st();
        !st.conns.is_empty() || st.sessions.values().any(|l| l.turn.is_some())
    }

    /// Every task the daemon started for sessions: turn tasks and attachment streams are handed
    /// back to be aborted and awaited; approval forwarders are aborted in place.
    fn take_tasks(&self) -> Vec<JoinHandle<()>> {
        let mut st = self.st();
        let mut out = Vec::new();
        for l in st.sessions.values_mut() {
            if let Some((_, h)) = l.turn.take() {
                out.push(h);
            }
            l.approvals.abort();
        }
        for c in st.conns.values_mut() {
            out.extend(std::mem::take(&mut c.attachments).into_values().map(|s| s.task));
        }
        out
    }

    /// A unique, never-used id that fits in 53 bits (exact in every JSON number implementation).
    #[allow(clippy::disallowed_methods, reason = "authority: daemon-fresh-id")]
    fn fresh_id(&self, st: &State) -> SessionId {
        loop {
            let n = self.id_seed.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let h = Hash::of(format!("{}:{nanos}:{n}", self.me.pid).as_bytes());
            let mut b = [0u8; 8];
            b.copy_from_slice(&h.0[..8]);
            let sid = SessionId(u128::from(u64::from_le_bytes(b) & ((1u64 << 53) - 1)));
            if sid.0 != 0 && !st.sessions.contains_key(&sid) && !st.crashed.contains(&sid) && !self.log_path(sid).exists() {
                return sid;
            }
        }
    }

    async fn boot_live(self: &Arc<Self>, sid: SessionId, log: Arc<dyn LogStore>) -> Result<Arc<Session>, String> {
        let (approval, requests) = ChannelApproval::new(16);
        let session = self.host.boot(sid, log, Arc::new(approval)).await?;
        let approvals = tokio::spawn(forward_approvals(self.clone(), sid, requests));
        let mut st = self.st();
        st.problems.remove(&sid);
        st.sessions.insert(sid, Live { session: session.clone(), owner: None, turn: None, mutation: None, approvals });
        Ok(session)
    }

    /// Runs `fut` as the session's turn task; the slot clears itself when it finishes.
    fn spawn_turn<F>(self: &Arc<Self>, st: &mut State, sid: SessionId, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        st.next_turn += 1;
        let generation = st.next_turn;
        let d = self.clone();
        let h = tokio::spawn(async move {
            fut.await;
            let mut st = d.st();
            if let Some(l) = st.sessions.get_mut(&sid).filter(|l| l.turn.as_ref().is_some_and(|(g, _)| *g == generation)) {
                l.turn = None;
            }
        });
        if let Some(l) = st.sessions.get_mut(&sid) {
            l.turn = Some((generation, h));
        }
    }

    fn start_consume(self: &Arc<Self>, sid: SessionId, session: Arc<Session>) {
        let jobs = running_jobs(&session);
        let d = self.clone();
        let fut = async move {
            if let Err(e) = recovery::consume(&session, &jobs).await {
                d.st().problems.insert(sid, e);
            }
        };
        let mut st = self.st();
        self.spawn_turn(&mut st, sid, fut);
    }

    /// Sessions a reload interrupted: close the cut turn, boot, run the continuation.
    async fn resume_reloaded(self: &Arc<Self>) {
        let _files = self.files.lock().await;
        for (sid, path) in list_logs(&self.dir) {
            let log = match self.index.open_log(sid) {
                Ok(l) => l,
                Err(e) => {
                    self.st()
                        .problems
                        .insert(sid, format!("the log {} could not be read ({e}); the session was not resumed", path.display()));
                    continue;
                }
            };
            if recovery::unconsumed(&log).is_empty() {
                continue;
            }
            if let Err(e) = recovery::close_open_turn(&log, CancelReason::Reload) {
                self.st().problems.insert(sid, e);
                continue;
            }
            match self.boot_live(sid, Arc::new(log)).await {
                Ok(session) => self.start_consume(sid, session),
                Err(e) => {
                    self.st()
                        .problems
                        .insert(sid, format!("the session could not boot after the reload ({e}); open it again once that is fixed"));
                }
            }
        }
    }

    /// One crash scan over logs no live session holds; confirmed identities are marked crashed.
    async fn scan(self: &Arc<Self>) {
        let _files = self.files.lock().await;
        let skip: BTreeSet<SessionId> = {
            let st = self.st();
            st.sessions.keys().chain(st.crashed.iter()).copied().collect()
        };
        let d = self.clone();
        let Ok((crashed, problems)) = tokio::task::spawn_blocking(move || d.scan_blocking(&skip)).await else {
            return;
        };
        let mut st = self.st();
        for sid in crashed {
            if !st.sessions.contains_key(&sid) {
                st.crashed.insert(sid);
            }
        }
        st.problems.extend(problems);
    }

    fn scan_blocking(&self, skip: &BTreeSet<SessionId>) -> (Vec<SessionId>, Vec<(SessionId, String)>) {
        let mut logs = BTreeMap::new();
        let mut candidates = Vec::new();
        let mut crashed = Vec::new();
        let mut problems = Vec::new();
        for (sid, _) in list_logs(&self.dir) {
            if skip.contains(&sid) {
                continue;
            }
            // A log that does not open is reported to whoever opens it.
            let Ok(log) = self.index.open_log(sid) else {
                continue;
            };
            if crash::crashed_marker(&log).is_some() {
                crashed.push(sid);
            } else if let Some(p) = crash::candidate(&log, self.me) {
                candidates.push((sid, p));
                logs.insert(sid, log);
            }
        }
        let confirmed = lock(&self.scanner).scan(&candidates, &crash::identity_alive);
        for (sid, p) in confirmed {
            if let Some(log) = logs.get(&sid) {
                match crash::mark_crashed(log, p) {
                    Ok(_) => crashed.push(sid),
                    Err(e) => problems.push((sid, e)),
                }
            }
        }
        (crashed, problems)
    }

    /// Reload/shutdown steps 2–4: abort running turns and give them `reload_grace`; abort what is
    /// left, every attachment stream and every other connection; close turns still open.
    async fn quiesce(&self, reason: CancelReason, keep: Option<ConnId>) -> Vec<String> {
        let (turns, sessions) = {
            let mut st = self.st();
            // Dropped reply slots answer `Unavailable`, so tools waiting on a human return now.
            st.approvals.clear();
            let mut turns = Vec::new();
            for l in st.sessions.values_mut() {
                if let Some((_, h)) = l.turn.take() {
                    l.session.abort();
                    turns.push(h);
                }
            }
            (turns, st.sessions.values().map(|l| l.session.clone()).collect::<Vec<_>>())
        };
        let deadline = tokio::time::Instant::now() + self.opts.reload_grace;
        let mut late = Vec::new();
        for mut h in turns {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                late.push(h);
            }
        }
        for h in &late {
            h.abort();
        }
        for h in late {
            let _ = h.await;
        }
        let (kicks, streams) = {
            let mut st = self.st();
            let mut kicks = Vec::new();
            let mut streams = Vec::new();
            for (id, c) in st.conns.iter_mut() {
                if Some(*id) != keep {
                    kicks.extend(c.kick.take());
                }
                streams.extend(std::mem::take(&mut c.attachments).into_values().map(|s| s.task));
            }
            (kicks, streams)
        };
        for k in kicks {
            let _ = k.send(());
        }
        for s in streams {
            s.abort();
        }
        let mut problems = Vec::new();
        for s in sessions {
            if let Err(e) = recovery::close_open_turn(&**s.log(), reason) {
                problems.push(format!("session {}: {e}", s.id.0));
            }
        }
        problems
    }

    async fn connection(self: Arc<Self>, id: ConnId, stream: UnixStream) {
        let (rd, wr) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        let (kick, kicked) = oneshot::channel();
        let writer = tokio::spawn(write_loop(wr, rx));
        self.st().conns.insert(id, Conn { tx: tx.clone(), attached: BTreeSet::new(), attachments: BTreeMap::new(), kick: Some(kick) });
        let _guard = ConnGuard { d: self.clone(), id, writer };
        let mut reader = BufReader::new(rd);
        tokio::select! {
            _ = kicked => {}
            _ = self.serve_conn(id, &tx, &mut reader) => {}
        }
    }

    async fn serve_conn(self: &Arc<Self>, conn: ConnId, tx: &Tx, reader: &mut BufReader<OwnedReadHalf>) {
        let mut hello: Option<String> = None;
        loop {
            let frame: ClientFrame = match wire::read_frame(reader).await {
                Ok(Some(f)) => f,
                Ok(None) => return,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    send(
                        tx,
                        ServerFrame::error(
                            None,
                            format!(
                                "a line was not a valid frame ({e}), so we skipped it. Send one JSON object per line: {{\"v\": 1, \"id\": <n>, \"method\": \"…\", \"params\": {{…}}}}"
                            ),
                        ),
                    );
                    continue;
                }
                Err(_) => return,
            };
            let id = frame.id;
            let method = frame.method.as_str();
            if method == "hello" {
                match parse::<wire::Hello>(method, frame.params).and_then(|h| wire::negotiate(h.min, h.max).map(|v| (v, h.client_instance)))
                {
                    Ok((v, instance)) => {
                        hello = Some(instance);
                        send(tx, ServerFrame::reply(id, "hello_ok", wire::hello_ok(v)));
                    }
                    Err(e) => send(tx, ServerFrame::error(Some(id), e)),
                }
                continue;
            }
            let Some(instance) = hello.as_deref() else {
                send(
                    tx,
                    ServerFrame::error(
                        Some(id),
                        "this connection has not said hello, so we did nothing. Send `hello{min, max, client_instance}` first",
                    ),
                );
                continue;
            };
            match method {
                "daemon.reload" => {
                    let fd = reader.get_ref().as_ref().as_fd();
                    self.reload(conn, id, tx, frame.params, fd).await;
                }
                "daemon.shutdown" => {
                    let claimed = !std::mem::replace(&mut self.st().closing, true);
                    if claimed {
                        send(tx, ServerFrame::reply(id, "result", json!({ "shutdown": true })));
                        flush(tx).await;
                        let _ = self.control.send(Control::Shutdown);
                    } else {
                        send(tx, ServerFrame::error(Some(id), CLOSING));
                    }
                }
                "session.attach" => {
                    if let Err(e) = parse(method, frame.params).and_then(|p| self.attach(conn, id, tx, instance, p)) {
                        send(tx, ServerFrame::error(Some(id), e));
                    }
                }
                "session.subscribe" => {
                    let subscribed = match parse(method, frame.params) {
                        Ok(p) => self.subscribe(conn, id, tx, p).await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = subscribed {
                        send(tx, ServerFrame::error(Some(id), e));
                    }
                }
                "session.prompt" => {
                    if let Err(e) = parse(method, frame.params).and_then(|p| self.prompt(conn, id, tx, p)) {
                        send(tx, ServerFrame::error(Some(id), e));
                    }
                }
                _ => match self.call(conn, method, frame.params).await {
                    Ok((event, payload)) => send(tx, ServerFrame::reply(id, event, payload)),
                    Err(e) => send(tx, ServerFrame::error(Some(id), e)),
                },
            }
        }
    }

    async fn call(self: &Arc<Self>, conn: ConnId, method: &str, params: Value) -> Result<(&'static str, Value), String> {
        match method {
            "session.list" => Ok(("result", self.list())),
            "session.open" => Ok(("result", self.open(parse(method, params)?).await?)),
            "session.detach" => Ok(("result", self.detach(conn, parse::<SessionParams>(method, params)?.session))),
            "session.mutation" => Ok(("result", self.begin_mutation(conn, parse::<SessionParams>(method, params)?.session)?)),
            "session.command" => Ok(("result", self.command(conn, parse(method, params)?)?)),
            "session.call" => Ok(("result", self.host_call(conn, parse(method, params)?).await?)),
            "session.reopen" => Ok(("result", self.reopen(conn, parse::<SessionParams>(method, params)?.session).await?)),
            "session.steer" => {
                let p: SteerParams = parse(method, params)?;
                self.owned(conn, p.session)?.steer(p.text, p.urgent);
                Ok(("result", json!({ "session": p.session })))
            }
            "session.abort" => {
                let sid = parse::<SessionParams>(method, params)?.session;
                self.owned(conn, sid)?.abort();
                Ok(("result", json!({ "session": sid })))
            }
            "session.resume" => Ok(("result", self.resume(parse::<SessionParams>(method, params)?.session).await?)),
            "approval.answer" => Ok(("result", self.answer(conn, parse(method, params)?).await?)),
            other => Err(format!("unknown method `{other}`, so we did nothing. This daemon answers: {}", wire::METHODS.join(", "))),
        }
    }

    fn list(&self) -> Value {
        let on_disk = list_logs(&self.dir);
        let st = self.st();
        let ids: BTreeSet<SessionId> =
            on_disk.iter().map(|(s, _)| *s).chain(st.sessions.keys().copied()).chain(st.crashed.iter().copied()).collect();
        let rows: Vec<Value> = ids
            .into_iter()
            .map(|sid| {
                let (status, owner) = match st.sessions.get(&sid) {
                    Some(l) => (
                        if l.turn.is_some() { "running" } else { "idle" },
                        l.owner.as_ref().map(|o| json!({ "client_instance": o.client_instance, "live": o.live })),
                    ),
                    None if st.crashed.contains(&sid) => ("crashed", None),
                    None if st.problems.contains_key(&sid) => ("error", None),
                    None => ("closed", None),
                };
                json!({ "session": sid, "status": status, "owner": owner, "problem": st.problems.get(&sid) })
            })
            .collect();
        json!({ "sessions": rows })
    }

    #[allow(clippy::disallowed_methods, reason = "authority: daemon-open-session")]
    async fn open(self: &Arc<Self>, p: OpenParams) -> Result<Value, String> {
        let _files = self.files.lock().await;
        let sid = {
            let st = self.st();
            if st.closing {
                return Err(CLOSING.to_string());
            }
            let sid = match p.id {
                // A client-chosen *new* id must stay SDK-safe; logs already on disk (CLI sessions
                // use 128-bit ids) open under the id they have.
                Some(id) if id.0 > u128::from(u64::MAX) && !self.log_path(id).exists() => {
                    return Err(format!(
                        "session id {} does not fit in 64 bits, so we did not create it. Pick a smaller id, or omit it to get a fresh one",
                        id.0
                    ));
                }
                Some(id) => id,
                None => self.fresh_id(&st),
            };
            if st.sessions.contains_key(&sid) {
                return Ok(json!({ "session": sid, "created": false }));
            }
            if st.crashed.contains(&sid) {
                return Err(format!(
                    "session {} crashed with work in flight, so we did not open it as it is. Resume it with `session.resume` (it continues from the last completed turn)",
                    sid.0
                ));
            }
            sid
        };
        let path = self.log_path(sid);
        let created = !path.exists();
        let log = self
            .index
            .open_log(sid)
            .map_err(|e| format!("the log {} could not be opened ({e}), so the session was not opened", path.display()))?;
        let intents = !recovery::unconsumed(&log).is_empty();
        if intents {
            recovery::close_open_turn(&log, CancelReason::Reload)?;
        } else if let Some(turn) =
            open_turn(&log).map_err(|e| format!("the log {} {e}, so the session was not opened. Inspect the log", path.display()))?
        {
            let who = log.tail_meta().and_then(|m| match m.kind {
                EntryKind::SessionMeta { process: Some(p), .. } => Some(format!("process {}", p.pid)),
                _ => None,
            });
            return Err(format!(
                "session {} has turn {} in flight that belongs to {}, so we did not open it: two processes never drive one log. If that process is gone, the crash scan marks the session within two scans and `session.resume` recovers it",
                sid.0,
                turn.0,
                who.unwrap_or_else(|| "another process".to_string())
            ));
        }
        let session = self.boot_live(sid, Arc::new(log)).await?;
        if intents {
            self.start_consume(sid, session);
        }
        Ok(json!({ "session": sid, "created": created }))
    }

    fn attach(self: &Arc<Self>, conn: ConnId, id: u64, tx: &Tx, hello: &str, p: AttachParams) -> Result<(), String> {
        let sid = p.session;
        let instance = p.client_instance.unwrap_or_else(|| hello.to_string());
        let (session, decision, displaced) = {
            let mut st = self.st();
            if st.closing {
                return Err(CLOSING.to_string());
            }
            let live = st.sessions.get_mut(&sid).ok_or_else(|| not_open(sid))?;
            if live.mutation.as_ref().is_some_and(|m| m.conn != conn) {
                return Err("an operator mutation is pending; attach after it finishes or the session closes".into());
            }
            let decision = match &live.owner {
                Some(o) if o.conn == conn && o.live => Takeover::Allowed,
                other => {
                    let owner = other.as_ref().map(|o| Owner { client_instance: o.client_instance.clone(), live: o.live });
                    takeover::decide(
                        owner.as_ref(),
                        &AttachRequest { client_instance: instance.clone(), has_local_history: p.has_local_history },
                    )
                }
            };
            live.session
                .log()
                .append(
                    None,
                    Principal::system("daemon"),
                    EntryKind::SurfaceAttached {
                        client_instance: instance.clone(),
                        has_local_history: p.has_local_history,
                        takeover: decision.clone(),
                    },
                )
                .map_err(|e| format!("could not record the attach in the session log ({e}), so we did not attach"))?;
            let mut displaced = None;
            if decision == Takeover::Allowed {
                displaced = live.owner.as_ref().filter(|o| o.live && o.conn != conn).map(|o| o.conn);
                live.owner = Some(OwnerSlot { conn, client_instance: instance.clone(), live: true });
            }
            let session = live.session.clone();
            if let Some(old) = displaced {
                st.approvals.retain(|_, a| !(a.conn == old && a.session == sid));
            }
            if let Some(c) = st.conns.get_mut(&conn) {
                c.attached.insert(sid);
            }
            (session, decision, displaced.and_then(|c| st.conns.get(&c).map(|c| c.tx.clone())))
        };
        let rx = p.watch.then(|| (session.subscribe(), session.subscribe_from(p.from)));
        let payload = match &decision {
            Takeover::Allowed => json!({ "session": sid, "takeover": "allowed", "owner": true, "head": session.log().head() }),
            Takeover::Refused(reason) => {
                json!({ "session": sid, "takeover": "refused", "reason": reason, "owner": false, "head": session.log().head() })
            }
        };
        send(tx, ServerFrame::reply(id, "result", payload));
        if let Some(old) = displaced {
            send(&old, ServerFrame::event("owner.lost", json!({ "session": sid, "by": instance })));
        }
        if let Some((rx, replay)) = rx {
            self.start_stream(conn, tx, session, rx, p.from, replay);
        }
        Ok(())
    }

    fn detach(self: &Arc<Self>, conn: ConnId, sid: SessionId) -> Value {
        let (stream, attached, released) = {
            let mut st = self.st();
            let (stream, attached) = match st.conns.get_mut(&conn) {
                Some(c) => (c.attachments.remove(&sid), c.attached.remove(&sid)),
                None => (None, false),
            };
            let mut released = false;
            if let Some(l) = st.sessions.get_mut(&sid).filter(|l| l.owner.as_ref().is_some_and(|o| o.conn == conn)) {
                l.owner = None;
                released = true;
            }
            st.approvals.retain(|_, a| !(a.conn == conn && a.session == sid));
            (stream, attached, released)
        };
        let attached = attached || stream.is_some();
        if let Some(s) = stream {
            s.task.abort();
        }
        self.abandon_mutations(conn);
        json!({ "session": sid, "attached": attached, "released": released })
    }

    fn owner_check(st: &State, conn: ConnId, sid: SessionId) -> Result<(), String> {
        if st.closing {
            return Err(CLOSING.to_string());
        }
        let live = st.sessions.get(&sid).ok_or_else(|| not_open(sid))?;
        match &live.owner {
            Some(o) if o.conn == conn && o.live => Ok(()),
            Some(o) => Err(format!(
                "this client does not drive session {}: {} does, so we did not deliver the request. Detach the other client or reconnect from it, then attach again",
                sid.0,
                if o.client_instance.is_empty() { "another client".to_string() } else { format!("client `{}`", o.client_instance) }
            )),
            None => Err(format!(
                "this client does not drive session {}, so we did not deliver the request. Attach to it first (`session.attach`)",
                sid.0
            )),
        }
    }

    fn owned(&self, conn: ConnId, sid: SessionId) -> Result<Arc<Session>, String> {
        let st = self.st();
        Daemon::owner_check(&st, conn, sid)?;
        st.sessions.get(&sid).map(|l| l.session.clone()).ok_or_else(|| not_open(sid))
    }

    async fn resume(self: &Arc<Self>, sid: SessionId) -> Result<Value, String> {
        let _files = self.files.lock().await;
        {
            let st = self.st();
            if st.closing {
                return Err(CLOSING.to_string());
            }
            if st.sessions.contains_key(&sid) {
                return Err(format!("session {} is already open in this daemon, so there is nothing to resume", sid.0));
            }
            if !st.crashed.contains(&sid) {
                return Err(format!("session {} is not marked crashed, so there is nothing to resume. Open it with `session.open`", sid.0));
            }
        }
        let path = self.log_path(sid);
        let log = self
            .index
            .open_log(sid)
            .map_err(|e| format!("the log {} could not be opened ({e}), so the session was not resumed", path.display()))?;
        let fork = crash::resume_crashed(&log)?;
        self.st().crashed.remove(&sid);
        self.boot_live(sid, Arc::new(log))
            .await
            .map_err(|e| format!("the session was recovered to the last completed turn but could not boot ({e}). Open it with `session.open` once that is fixed"))?;
        Ok(json!({ "session": sid, "fork": fork }))
    }
}
