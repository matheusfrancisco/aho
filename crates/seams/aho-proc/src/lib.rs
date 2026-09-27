//! Processes and the operations table behind every tool call.
//!
//! * `Subprocess` / `Child` are the seam a world provides (`world-host`, `world-ns`, …).
//! * `OpTable` is the operations manager the session owns: one row per started tool call, a
//!   bounded output ring with cursors, `AHO_PROGRESS` / `AHO_CHECKPOINT` markers, `wait`, and
//!   `cancel` that fires the op's token and kills an attached child's whole process group.
//! * Crash-honest child receipts: every child attached to a live op is announced
//!   through [`OpTable::on_attach`]; the session appends it as `Child/Spawned` ([`spawned`]) so
//!   `aho gc` can reap it by `(pid, start_time)` once the owner that started it is gone.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_cap::{CancelToken, Handle, HandleKind, SeamId};
use aho_log::{Entry, EntryKind, OpKind, OpStatus, ProcessIdentity};
use aho_types::*;
use tokio::sync::watch;

pub const SEAM: SeamId = SeamId("subprocess");

// ───────────────────────────── the seam ─────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    /// Relative paths resolve against the world's workspace root; `None` = the root.
    pub cwd: Option<String>,
    /// Added on top of the world's environment.
    pub env: Vec<(String, String)>,
    /// Written to stdin, which is then closed. `None` = stdin is `/dev/null`.
    pub stdin: Option<Vec<u8>>,
    /// Past this the whole process group is killed and `wait` returns `ProcError::Timeout`.
    pub timeout_ms: Option<u64>,
}

impl SpawnSpec {
    pub fn new(argv: &[&str]) -> SpawnSpec {
        SpawnSpec { argv: argv.iter().map(|s| s.to_string()).collect(), ..SpawnSpec::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    /// `128 + signal` when the process was killed by a signal.
    pub code: i32,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcError {
    NotFound(String),
    Denied(String),
    Timeout,
    Io(String),
}

impl ProcError {
    pub fn code(&self) -> &'static str {
        match self {
            ProcError::NotFound(_) => "PROC_NOT_FOUND",
            ProcError::Denied(_) => "PROC_DENIED",
            ProcError::Timeout => "PROC_TIMEOUT",
            ProcError::Io(_) => "PROC_IO",
        }
    }
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::NotFound(p) => write!(f, "`{p}` was not found in this world; nothing ran. Install it or give a full path."),
            ProcError::Timeout => f.write_str(
                "the process ran past its time limit and its whole process group was killed. Raise timeout_ms, or split the work.",
            ),
            ProcError::Denied(m) | ProcError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ProcError {}

/// A running process. Output (stdout and stderr interleaved as they arrive) is captured into a
/// bounded buffer read with cursors.
pub trait Child: Send + Sync {
    fn identity(&self) -> ProcessIdentity;
    /// Ends when this process ends, not when its last descendant does (a stray `sleep 30 &`
    /// holding the pipe must not hold the caller).
    fn wait(&self) -> BoxFuture<'_, Result<Exit, ProcError>>;
    /// Kills the whole process group, including backgrounded grandchildren.
    fn kill(&self) -> BoxFuture<'_, Result<(), ProcError>>;
    /// `(bytes from cursor, next cursor, done)`; `done` once the process ended and its output drained.
    /// A cursor older than the buffer's start reads from the start (the gap is lost).
    fn output(&self, cursor: u64) -> (Vec<u8>, u64, bool);
    /// Resolves when output beyond `cursor` exists or the output is done. Default: a short sleep.
    fn readable(&self, cursor: u64) -> BoxFuture<'_, ()> {
        let _ = cursor;
        Box::pin(tokio::time::sleep(Duration::from_millis(20)))
    }
    /// Only for children from `Subprocess::spawn_interactive`.
    fn write_stdin(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), ProcError>> {
        let _ = bytes;
        Box::pin(async { Err(ProcError::Denied("this process was started without an open stdin; nothing was written.".into())) })
    }
}

/// Exclusive, lossless byte streams for a protocol process in the bound world.
///
/// Unlike `Child::output`, stdout and stderr stay separate and are never truncated to a ring.
/// The caller must drain both streams. Dropping stdin sends EOF; `child` owns the process
/// lifecycle (including whole-process-group cancellation). No stream is copied to `output`.
pub struct TransportChild {
    pub child: Arc<dyn Child>,
    pub stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    pub stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
}

pub trait Subprocess: Send + Sync {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>>;
    /// Like `spawn` but stdin stays open for `Child::write_stdin` (`spec.stdin` is written first).
    /// Worlds that cannot do this keep the default refusal; `bash_persistent` then reports it.
    fn spawn_interactive(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        let _ = spec;
        Box::pin(async {
            Err(ProcError::Denied(
                "this world cannot keep a process's stdin open, so no long-lived shell was started. Use `bash` instead.".into(),
            ))
        })
    }
    /// Start a protocol process with open stdin and separate, untruncated output streams.
    /// Worlds without protocol transport support refuse rather than starting on the host.
    fn spawn_transport(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<TransportChild, ProcError>> {
        let _ = spec;
        Box::pin(async { Err(ProcError::Denied("this world does not support separate protocol streams; no process was started.".into())) })
    }
}

#[derive(Clone)]
pub struct SubprocessHandle(pub Arc<dyn Subprocess>);
impl Handle for SubprocessHandle {
    const KIND: HandleKind = HandleKind::Subprocess;
}

// ───────────────────────────── bounded output ─────────────────────────────

/// Default capacity of an op's output ring.
pub const OUTPUT_CAP: usize = 1 << 20;

/// A byte ring addressed by absolute offsets: offset 0 is the first byte ever pushed; when the
/// ring is full the oldest bytes are dropped and `start()` advances.
#[derive(Clone, Debug)]
pub struct OutputBuffer {
    cap: usize,
    start: u64,
    data: VecDeque<u8>,
}

/// One cursor read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub next: u64,
    /// Bytes between the caller's cursor and the oldest byte still held (lost to the ring).
    pub dropped: u64,
}

impl OutputBuffer {
    pub fn new(cap: usize) -> OutputBuffer {
        OutputBuffer { cap: cap.max(1), start: 0, data: VecDeque::new() }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        let bytes = if bytes.len() > self.cap {
            let skip = bytes.len() - self.cap;
            self.start += (self.data.len() + skip) as u64;
            self.data.clear();
            &bytes[skip..]
        } else {
            bytes
        };
        self.data.extend(bytes);
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.start += excess as u64;
        }
    }
    pub fn start(&self) -> u64 {
        self.start
    }
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
    pub fn read(&self, cursor: u64) -> Chunk {
        let from = cursor.clamp(self.start, self.end());
        let skip = (from - self.start) as usize;
        Chunk { bytes: self.data.iter().skip(skip).copied().collect(), next: self.end(), dropped: self.start.saturating_sub(cursor) }
    }
}

// ───────────────────────────── child receipts ─────────────────────────────

/// Namespace and name of a child receipt: `Custom{ns: "Child", name: "Spawned"}`.
pub const CHILD_NS: &str = "Child";
pub const SPAWNED: &str = "Spawned";
/// The only principal whose receipts count, `Principal::system(RECEIPT_BY)`: a tool can append any
/// `Custom` entry through `value.events`, so a receipt from anyone else names no child of ours.
pub const RECEIPT_BY: &str = "ops";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChildKind {
    Job,
    Viewer,
    Sandbox,
}

impl ChildKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChildKind::Job => "job",
            ChildKind::Viewer => "viewer",
            ChildKind::Sandbox => "sandbox",
        }
    }
}

/// The receipt an owner appends, as `Principal::system(RECEIPT_BY)`, when it starts a child that
/// may outlive it: `{pid, start_time, kind, label}`.
pub fn spawned(child: ProcessIdentity, kind: ChildKind, label: &str) -> EntryKind {
    EntryKind::Custom {
        ns: CHILD_NS.into(),
        name: SPAWNED.into(),
        body: serde_json::json!({"pid": child.pid, "start_time": child.start_time.0, "kind": kind.as_str(), "label": label}),
    }
}

/// A `Child/Spawned` receipt read back from the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildReceipt {
    pub child: ProcessIdentity,
    pub kind: String,
    pub label: String,
}

/// The receipt `e` is, if it is one appended by `system:ops` with a readable body.
pub fn child_receipt(e: &Entry) -> Option<ChildReceipt> {
    let EntryKind::Custom { ns, name, body } = &e.kind else { return None };
    if ns != CHILD_NS || name != SPAWNED || !matches!(&e.by, Principal::System { name } if name == RECEIPT_BY) {
        return None;
    }
    let pid = u32::try_from(body.get("pid")?.as_u64()?).ok()?;
    let start_time = Timestamp(body.get("start_time")?.as_u64()?);
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or(ChildKind::Job.as_str()).to_string();
    let label = body.get("label").and_then(Value::as_str).unwrap_or("").to_string();
    Some(ChildReceipt { child: ProcessIdentity { pid, start_time }, kind, label })
}

// ───────────────────────────── operations ─────────────────────────────

/// Marker lines a job may print on stdout; `job_wait` returns on them.
pub const PROGRESS_MARKER: &str = "AHO_PROGRESS ";
pub const CHECKPOINT_MARKER: &str = "AHO_CHECKPOINT";

#[derive(Clone, Debug, PartialEq)]
pub struct OpInfo {
    pub op: OpId,
    pub call: CallId,
    pub tool: String,
    pub kind: OpKind,
    /// `None` while running.
    pub status: Option<OpStatus>,
    pub started: Timestamp,
    pub identity: Option<ProcessIdentity>,
}

/// The event `OpTable::wait` resolved on.
#[derive(Clone, Debug, PartialEq)]
pub enum OpWait {
    /// Payload after `AHO_PROGRESS ` (usually JSON).
    Progress(String),
    /// Text after `AHO_CHECKPOINT`, trimmed (may be empty).
    Checkpoint(String),
    Done(OpStatus),
}

/// One cursor read of an op's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpOutput {
    pub text: String,
    pub cursor: u64,
    pub done: bool,
    /// Bytes lost to the ring between the caller's cursor and what is still held.
    pub dropped: u64,
}

struct OpEntry {
    info: OpInfo,
    rerunnable: bool,
    external_effect: bool,
    cancel: CancelToken,
    started_at: Instant,
    child: Option<Arc<dyn Child>>,
    out: OutputBuffer,
    events: Vec<OpWait>,
    tx: watch::Sender<u64>,
}

/// Called with the op's row once a child is attached to a live op (its `identity` is set), after
/// the table's lock is released, so the hook may read the table.
pub type AttachHook = Arc<dyn Fn(&OpInfo) + Send + Sync>;

#[derive(Default)]
struct Ops {
    next: u64,
    ops: BTreeMap<OpId, OpEntry>,
    by_call: BTreeMap<CallId, OpId>,
    on_attach: Option<AttachHook>,
}

/// The session's operations manager. Cheap to clone; all clones share one table.
#[derive(Clone, Default)]
pub struct OpTable(Arc<Mutex<Ops>>);

/// Handle kind `Operations`; seeded by the session so `tool-bash` / `tool-jobs` can hold it.
#[derive(Clone)]
pub struct OperationsHandle(pub OpTable);
impl Handle for OperationsHandle {
    const KIND: HandleKind = HandleKind::Operations;
}

impl OpTable {
    pub fn new() -> OpTable {
        OpTable::default()
    }

    fn lock(&self) -> MutexGuard<'_, Ops> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start tracking the operation behind `call`. Returns its id and cancellation token.
    pub fn register(&self, call: CallId, tool: &str, kind: OpKind, rerunnable: bool, external_effect: bool) -> (OpId, CancelToken) {
        let mut g = self.lock();
        g.next += 1;
        let op = OpId(g.next);
        let cancel = CancelToken::new();
        let (tx, _) = watch::channel(0u64);
        let info = OpInfo { op, call: call.clone(), tool: tool.to_string(), kind, status: None, started: Timestamp::now(), identity: None };
        g.ops.insert(
            op,
            OpEntry {
                info,
                rerunnable,
                external_effect,
                cancel: cancel.clone(),
                started_at: Instant::now(),
                child: None,
                out: OutputBuffer::new(OUTPUT_CAP),
                events: Vec::new(),
                tx,
            },
        );
        g.by_call.insert(call, op);
        (op, cancel)
    }

    pub fn op_for_call(&self, call: &CallId) -> Option<OpId> {
        self.lock().by_call.get(call).copied()
    }

    /// `(rerunnable, external_effect)` as registered: what resume folds on.
    pub fn resume_bits(&self, op: OpId) -> Option<(bool, bool)> {
        self.lock().ops.get(&op).map(|e| (e.rerunnable, e.external_effect))
    }

    /// Every later [`attach_child`](OpTable::attach_child) to a live op calls `hook` (replacing
    /// any earlier one). The session installs the one that writes `Child/Spawned` receipts.
    pub fn on_attach(&self, hook: AttachHook) {
        self.lock().on_attach = Some(hook);
    }

    /// Tie a process to the op: its identity is listed, `cancel` kills its process group, and the
    /// [`on_attach`](OpTable::on_attach) hook announces it. Attaching to an op that already
    /// finished kills the child at once.
    pub fn attach_child(&self, op: OpId, child: Arc<dyn Child>) {
        let (orphan, announce) = {
            let mut g = self.lock();
            let hook = g.on_attach.clone();
            match g.ops.get_mut(&op) {
                Some(e) if e.info.status.is_none() && !e.cancel.is_cancelled() => {
                    e.info.identity = Some(child.identity());
                    e.child = Some(child);
                    (None, hook.map(|h| (h, e.info.clone())))
                }
                Some(e) => {
                    e.info.identity = Some(child.identity());
                    (Some(child), None)
                }
                None => (None, None),
            }
        };
        if let Some((hook, info)) = announce {
            hook(&info);
        }
        if let Some(c) = orphan {
            drive_kill(c);
        }
    }

    /// One output line (without its newline). Appended to the op's ring; a line starting with
    /// `PROGRESS_MARKER` / `CHECKPOINT_MARKER` also wakes `wait`.
    pub fn progress(&self, op: OpId, line: String) {
        let mut g = self.lock();
        let Some(e) = g.ops.get_mut(&op) else { return };
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        e.out.push(line.as_bytes());
        e.out.push(b"\n");
        let event = if let Some(rest) = line.strip_prefix(PROGRESS_MARKER) {
            Some(OpWait::Progress(rest.trim().to_string()))
        } else {
            line.strip_prefix(CHECKPOINT_MARKER)
                .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                .map(|rest| OpWait::Checkpoint(rest.trim().to_string()))
        };
        if let Some(ev) = event {
            e.events.push(ev);
            e.tx.send_modify(|n| *n += 1);
        }
    }

    /// Terminal status; the first one wins. Wakes every waiter.
    pub fn finish(&self, op: OpId, status: OpStatus) {
        let mut g = self.lock();
        let Some(e) = g.ops.get_mut(&op) else { return };
        if e.info.status.is_some() {
            return;
        }
        e.info.status = Some(status);
        e.child = None;
        e.tx.send_modify(|n| *n += 1);
    }

    /// `None` = no such op; `Some(None)` = running.
    pub fn status(&self, op: OpId) -> Option<Option<OpStatus>> {
        self.lock().ops.get(&op).map(|e| e.info.status.clone())
    }

    pub fn list(&self) -> Vec<OpInfo> {
        self.lock().ops.values().map(|e| e.info.clone()).collect()
    }

    pub fn info(&self, op: OpId) -> Option<OpInfo> {
        self.lock().ops.get(&op).map(|e| e.info.clone())
    }

    pub fn read(&self, op: OpId, cursor: u64) -> Option<OpOutput> {
        let g = self.lock();
        let e = g.ops.get(&op)?;
        let c = e.out.read(cursor);
        Some(OpOutput {
            text: String::from_utf8_lossy(&c.bytes).into_owned(),
            cursor: c.next,
            done: e.info.status.is_some(),
            dropped: c.dropped,
        })
    }

    /// `(text from cursor, next cursor, done)`; a gap lost to the ring is announced in the text.
    pub fn output(&self, op: OpId, cursor: u64) -> Option<(String, u64, bool)> {
        let o = self.read(op, cursor)?;
        let text = if o.dropped > 0 { format!("[… {} earlier bytes dropped …]\n{}", o.dropped, o.text) } else { o.text };
        Some((text, o.cursor, o.done))
    }

    /// Fires the op's token and kills an attached child's process group. `false` if the op is
    /// unknown or already finished.
    pub fn cancel(&self, op: OpId) -> bool {
        let child = {
            let mut g = self.lock();
            let Some(e) = g.ops.get_mut(&op) else { return false };
            if e.info.status.is_some() {
                return false;
            }
            e.cancel.cancel();
            e.child.take()
        };
        if let Some(c) = child {
            drive_kill(c);
        }
        true
    }

    /// Resolves on the next progress or checkpoint marker, on the terminal status (at once if it
    /// is already terminal), or `None` after `timeout_ms` (also `None` for an unknown op).
    pub fn wait(&self, op: OpId, timeout_ms: u64) -> BoxFuture<'static, Option<OpWait>> {
        let (mut rx, seen) = {
            let g = self.lock();
            let Some(e) = g.ops.get(&op) else { return Box::pin(async { None }) };
            if let Some(s) = &e.info.status {
                let s = s.clone();
                return Box::pin(async move { Some(OpWait::Done(s)) });
            }
            (e.tx.subscribe(), e.events.len())
        };
        let table = self.clone();
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                {
                    let g = table.lock();
                    let e = g.ops.get(&op)?;
                    if let Some(ev) = e.events.get(seen) {
                        return Some(ev.clone());
                    }
                    if let Some(s) = &e.info.status {
                        return Some(OpWait::Done(s.clone()));
                    }
                }
                match tokio::time::timeout_at(deadline, rx.changed()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) | Err(_) => return None,
                }
            }
        })
    }

    /// Ops without a terminal status, with their age in ms (the heartbeat lists these).
    pub fn running(&self) -> Vec<(OpId, CallId, u64)> {
        self.lock()
            .ops
            .values()
            .filter(|e| e.info.status.is_none())
            .map(|e| (e.info.op, e.info.call.clone(), e.started_at.elapsed().as_millis() as u64))
            .collect()
    }
}

/// Run `child.kill()` to completion without blocking the caller.
fn drive_kill(child: Arc<dyn Child>) {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(async move {
                let _ = child.kill().await;
            });
        }
        Err(_) => {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    let _ = rt.block_on(child.kill());
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_bytes_and_reports_what_a_stale_cursor_lost() {
        let mut b = OutputBuffer::new(8);
        b.push(b"hello ");
        assert_eq!(b.read(0), Chunk { bytes: b"hello ".to_vec(), next: 6, dropped: 0 });
        b.push(b"world");
        assert_eq!((b.start(), b.end()), (3, 11));
        let c = b.read(0);
        assert_eq!(c.bytes, b"lo world".to_vec());
        assert_eq!(c.dropped, 3);
        assert_eq!(b.read(6), Chunk { bytes: b"world".to_vec(), next: 11, dropped: 0 });
        assert_eq!(b.read(11).bytes, Vec::<u8>::new());
        assert_eq!(b.read(99).next, 11, "a cursor past the end reads nothing and snaps back");
        b.push(b"0123456789abc");
        assert_eq!(b.read(11).bytes, b"56789abc".to_vec());
        assert_eq!(b.read(11).dropped, 5);
    }

    #[test]
    fn markers_are_parsed_only_at_line_start() {
        let t = OpTable::new();
        let (op, _) = t.register(CallId("c1".into()), "bash", OpKind::Process, false, true);
        t.progress(op, "building…".into());
        t.progress(op, "note: AHO_PROGRESS not at start".into());
        t.progress(op, "AHO_CHECKPOINTED is not a checkpoint".into());
        t.progress(op, "AHO_PROGRESS {\"pct\":50}\r\n".into());
        t.progress(op, "AHO_CHECKPOINT  tests compiled ".into());
        let g = t.lock();
        let e = &g.ops[&op];
        assert_eq!(e.events, vec![OpWait::Progress("{\"pct\":50}".into()), OpWait::Checkpoint("tests compiled".into())]);
        drop(g);
        let (text, cursor, done) = t.output(op, 0).unwrap();
        assert!(text.starts_with("building…\n"));
        assert!(text.contains("AHO_PROGRESS {\"pct\":50}\n"), "{text:?}");
        assert!(!done);
        assert_eq!(t.output(op, cursor).unwrap().0, "");
    }

    #[tokio::test]
    async fn wait_resolves_on_progress_checkpoint_done_or_timeout() {
        let t = OpTable::new();
        let (op, _) = t.register(CallId("c1".into()), "bash", OpKind::Process, false, true);
        assert_eq!(t.wait(op, 30).await, None, "nothing happened: times out");

        let w = t.wait(op, 5_000);
        let t2 = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            t2.progress(op, "plain output does not wake".into());
            tokio::time::sleep(Duration::from_millis(20)).await;
            t2.progress(op, "AHO_PROGRESS 1/3".into());
        });
        assert_eq!(w.await, Some(OpWait::Progress("1/3".into())));

        let w = t.wait(op, 5_000);
        t.progress(op, "AHO_CHECKPOINT".into());
        assert_eq!(w.await, Some(OpWait::Checkpoint(String::new())));

        let w = t.wait(op, 5_000);
        t.finish(op, OpStatus::Completed { code: 0 });
        t.finish(op, OpStatus::Failed { err: "late".into() });
        assert_eq!(w.await, Some(OpWait::Done(OpStatus::Completed { code: 0 })));
        assert_eq!(t.wait(op, 0).await, Some(OpWait::Done(OpStatus::Completed { code: 0 })), "terminal status: immediate");
        assert_eq!(t.status(op), Some(Some(OpStatus::Completed { code: 0 })), "the first status wins");
        assert_eq!(t.wait(OpId(999), 10).await, None);
        assert_eq!(t.status(OpId(999)), None);
    }

    #[test]
    fn cancel_fires_the_token_once_and_not_after_finish() {
        let t = OpTable::new();
        let (a, tok_a) = t.register(CallId("a".into()), "bash", OpKind::Process, false, true);
        let (b, tok_b) = t.register(CallId("b".into()), "read", OpKind::Io, true, false);
        assert_eq!(t.op_for_call(&CallId("b".into())), Some(b));
        assert_eq!(t.resume_bits(a), Some((false, true)));
        assert!(t.cancel(a));
        assert!(tok_a.is_cancelled());
        t.finish(b, OpStatus::Completed { code: 0 });
        assert!(!t.cancel(b), "a finished op cannot be cancelled");
        assert!(!tok_b.is_cancelled());
        assert!(!t.cancel(OpId(42)));
        let running: Vec<OpId> = t.running().into_iter().map(|(op, _, _)| op).collect();
        assert_eq!(running, vec![a], "cancel does not settle an op; its tool reports Cancelled");
    }

    /// A process stand-in with a fixed identity that records whether it was killed.
    struct Held {
        id: ProcessIdentity,
        killed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Child for Held {
        fn identity(&self) -> ProcessIdentity {
            self.id
        }
        fn wait(&self) -> BoxFuture<'_, Result<Exit, ProcError>> {
            Box::pin(std::future::pending())
        }
        fn kill(&self) -> BoxFuture<'_, Result<(), ProcError>> {
            self.killed.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn output(&self, cursor: u64) -> (Vec<u8>, u64, bool) {
            (vec![], cursor, false)
        }
    }

    fn held(pid: u32) -> (Arc<dyn Child>, Arc<std::sync::atomic::AtomicBool>) {
        let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        (Arc::new(Held { id: ProcessIdentity { pid, start_time: Timestamp(pid as u64 * 1_000) }, killed: killed.clone() }), killed)
    }

    /// `(op, identity, rows the hook saw in the table)` per announcement.
    type Announced = Arc<Mutex<Vec<(OpId, Option<ProcessIdentity>, usize)>>>;

    #[test]
    fn only_children_of_live_ops_are_announced_and_the_hook_may_read_the_table() {
        let t = OpTable::new();
        let seen: Announced = Arc::default();
        let (t2, seen2) = (t.clone(), seen.clone());
        // reading the table from the hook would deadlock if it ran under the lock
        t.on_attach(Arc::new(move |info: &OpInfo| {
            let rows = t2.list().len();
            seen2.lock().unwrap_or_else(|p| p.into_inner()).push((info.op, info.identity, rows));
        }));
        let (live, _) = t.register(CallId("a".into()), "bash", OpKind::Process, false, true);
        let (c1, killed1) = held(101);
        t.attach_child(live, c1.clone());

        let (done, _) = t.register(CallId("b".into()), "bash", OpKind::Process, false, true);
        t.finish(done, OpStatus::Completed { code: 0 });
        let (c2, killed2) = held(102);
        t.attach_child(done, c2);
        let (cancelled, _) = t.register(CallId("c".into()), "bash", OpKind::Process, false, true);
        t.cancel(cancelled);
        let (c3, killed3) = held(103);
        t.attach_child(cancelled, c3);
        t.attach_child(OpId(99), held(104).0);

        let announced = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(announced, vec![(live, Some(c1.identity()), 1)], "one receipt, for the child that keeps running");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(killed2.load(std::sync::atomic::Ordering::SeqCst) && killed3.load(std::sync::atomic::Ordering::SeqCst)) {
            assert!(Instant::now() < deadline, "a child attached to a settled op is killed at once");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!killed1.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn receipts_round_trip_and_only_the_ops_table_writes_them() {
        let log = aho_log::MemoryLog::new();
        let id = ProcessIdentity { pid: 42, start_time: Timestamp(7_000) };
        let append = |by: Principal, k: EntryKind| aho_log::LogStore::append(&log, None, by, k).unwrap();
        let ops = append(Principal::system(RECEIPT_BY), spawned(id, ChildKind::Viewer, "vnc"));
        assert_eq!(child_receipt(&ops), Some(ChildReceipt { child: id, kind: "viewer".into(), label: "vnc".into() }));
        let forged = append(Principal::system("bash"), spawned(id, ChildKind::Job, "forged"));
        assert_eq!(child_receipt(&forged), None, "a tool's value.events cannot name a child for gc to kill");
        let unreadable = append(
            Principal::system(RECEIPT_BY),
            EntryKind::Custom { ns: CHILD_NS.into(), name: SPAWNED.into(), body: serde_json::json!({"pid": "x"}) },
        );
        assert_eq!(child_receipt(&unreadable), None);
    }
}
