//! One pty per terminal. Two threads per terminal: a reader feeding the emulator and the
//! transcript, and a waiter that reaps the session leader. Input is written off the async
//! runtime (a program that stops reading must not stall the session).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aho_log::ProcessIdentity;
use aho_term::{Closed, Delta, Exit, OpenSpec, Screen, Size, Snapshot, TermError, Terminal, Terminals};
use aho_types::{BoxFuture, Timestamp};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;

use crate::transcript::Transcript;
use crate::{TRANSCRIPT_CAP, tree};

/// How long one `write` may wait for the program to take its input.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// After the tree is killed, how long `close` waits for the leader to be reaped.
pub const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// What programs see as `$TERM`: the emulator speaks the xterm subset vt100 implements.
pub const TERM: &str = "xterm-256color";

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct HostTerminals {
    root: PathBuf,
}

impl HostTerminals {
    pub fn new(root: impl Into<PathBuf>) -> HostTerminals {
        HostTerminals { root: root.into() }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: pty-host-working-directory")]
    fn start(&self, spec: OpenSpec) -> Result<Arc<dyn Terminal>, TermError> {
        let Some(program) = spec.argv.first() else {
            return Err(TermError::BadArgs("the terminal command is empty; nothing was opened.".into()));
        };
        let cwd = match &spec.cwd {
            Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
            Some(c) => self.root.join(c),
            None => self.root.clone(),
        };
        if !cwd.is_dir() {
            return Err(TermError::NotFound(format!("working directory {}", cwd.display())));
        }
        let size = PtySize { rows: spec.size.rows, cols: spec.size.cols, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| TermError::Io(format!("the host could not allocate a pseudo-terminal ({e}); nothing was opened.")))?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(&spec.argv[1..]);
        cmd.cwd(&cwd);
        cmd.env("TERM", TERM);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            let text = e.to_string();
            if text.contains("No such file") || text.contains("not found") {
                TermError::NotFound(program.clone())
            } else {
                TermError::Io(format!("could not start `{program}` on a terminal ({text}); nothing was opened."))
            }
        })?;
        // Only the child keeps the slave open: EOF on the master then means the session hung up.
        drop(pair.slave);
        let pid = child.process_id().ok_or_else(|| TermError::Io(format!("`{program}` exited before it could be tracked.")))?;
        let reader = pair.master.try_clone_reader().map_err(|e| TermError::Io(format!("could not read the terminal ({e}).")))?;
        let writer = pair.master.take_writer().map_err(|e| TermError::Io(format!("could not write to the terminal ({e}).")))?;
        let (tx, _) = watch::channel(0u64);
        let inner = Arc::new(Inner {
            identity: ProcessIdentity { pid, start_time: Timestamp::now() },
            state: Mutex::new(State {
                parser: vt100::Parser::new(spec.size.rows, spec.size.cols, 0),
                transcript: Transcript::new(TRANSCRIPT_CAP),
                size: spec.size,
                seq: 0,
                exit: None,
                closed: None,
            }),
            tx,
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(pair.master)),
        });
        // From here on, dropping `term` (on any error below) kills the tree.
        let term = PtyTerminal { inner: inner.clone() };
        spawn_reader(inner.clone(), reader)
            .and_then(|_| spawn_waiter(inner, child))
            .map_err(|e| TermError::Io(format!("could not start the terminal's reader threads ({e}); the program was killed.")))?;
        Ok(Arc::new(term))
    }
}

impl Terminals for HostTerminals {
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, Result<Arc<dyn Terminal>, TermError>> {
        Box::pin(async move { self.start(spec) })
    }
}

struct State {
    parser: vt100::Parser,
    transcript: Transcript,
    size: Size,
    seq: u64,
    exit: Option<Exit>,
    closed: Option<Closed>,
}

struct Inner {
    identity: ProcessIdentity,
    state: Mutex<State>,
    tx: watch::Sender<u64>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl Inner {
    fn bump(&self, st: &mut State) {
        st.seq += 1;
        self.tx.send_replace(st.seq);
    }

    fn feed(&self, bytes: &[u8]) {
        let mut st = lk(&self.state);
        st.parser.process(bytes);
        st.transcript.feed(bytes);
        self.bump(&mut st);
    }

    fn exited(&self, exit: Exit) {
        let mut st = lk(&self.state);
        st.exit = Some(exit);
        self.bump(&mut st);
    }

    /// Hang up: without the master the kernel sends SIGHUP to the foreground group.
    fn hang_up(&self) {
        lk(&self.writer).take();
        lk(&self.master).take();
    }
}

fn spawn_reader(inner: Arc<Inner>, mut reader: Box<dyn Read + Send>) -> std::io::Result<()> {
    let name = format!("pty-{}-out", inner.identity.pid);
    std::thread::Builder::new().name(name).spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => inner.feed(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // EIO once the last slave fd closes: the session is gone.
                Err(_) => break,
            }
        }
        let mut st = lk(&inner.state);
        inner.bump(&mut st);
    })?;
    Ok(())
}

fn spawn_waiter(inner: Arc<Inner>, child: Box<dyn portable_pty::Child + Send + Sync>) -> std::io::Result<()> {
    let name = format!("pty-{}-wait", inner.identity.pid);
    let child: Box<dyn portable_pty::Child> = child;
    match child.downcast::<std::process::Child>() {
        // The unix pty system hands out a `std::process::Child`: its status keeps the signal number.
        Ok(mut c) => std::thread::Builder::new().name(name).spawn(move || {
            let exit = match c.wait() {
                Ok(st) => exit_of(st),
                Err(_) => Exit { code: -1, signal: None },
            };
            inner.exited(exit);
        }),
        Err(mut c) => std::thread::Builder::new().name(name).spawn(move || {
            let exit = match c.wait() {
                Ok(st) => Exit { code: st.exit_code() as i32, signal: None },
                Err(_) => Exit { code: -1, signal: None },
            };
            inner.exited(exit);
        }),
    }?;
    Ok(())
}

fn exit_of(st: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(code), _) => Exit { code, signal: None },
        (None, Some(sig)) => Exit { code: 128 + sig, signal: Some(sig) },
        (None, None) => Exit { code: -1, signal: None },
    }
}

fn screen_of(st: &State) -> Screen {
    let s = st.parser.screen();
    let text = s.contents().lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim_end().to_string();
    let (row, col) = s.cursor_position();
    Screen {
        text,
        cursor_row: row,
        cursor_col: col,
        size: st.size,
        alternate: s.alternate_screen(),
        application_cursor: s.application_cursor(),
        title: s.title().to_string(),
    }
}

/// A terminal on the host. Dropping it without `close` still kills its tree.
pub struct PtyTerminal {
    inner: Arc<Inner>,
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        let closed = lk(&self.inner.state).closed.is_some();
        if !closed {
            tree::kill_tree(self.inner.identity.pid);
            self.inner.hang_up();
        }
    }
}

impl Terminal for PtyTerminal {
    fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    fn snapshot(&self, cursor: u64) -> Snapshot {
        let st = lk(&self.inner.state);
        let delta: Delta = st.transcript.read(cursor);
        Snapshot { screen: screen_of(&st), delta, seq: st.seq, exit: st.exit }
    }

    fn changed(&self, seq: u64) -> BoxFuture<'_, ()> {
        let mut rx = self.inner.tx.subscribe();
        Box::pin(async move {
            loop {
                if *rx.borrow_and_update() > seq {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    }

    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TermError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            {
                let st = lk(&inner.state);
                if st.closed.is_some() {
                    return Err(TermError::Closed("this terminal was closed; nothing was sent. Open a new one.".into()));
                }
            }
            let job = tokio::task::spawn_blocking(move || {
                let exited = lk(&inner.state).exit.is_some();
                let mut w = lk(&inner.writer);
                let Some(w) = w.as_mut() else {
                    return Err(TermError::Closed("this terminal was closed; nothing was sent. Open a new one.".into()));
                };
                w.write_all(&bytes).and_then(|_| w.flush()).map_err(|e| {
                    if exited {
                        TermError::Closed(
                            "the terminal's program has exited; the input was not delivered. Close it and open a new one.".into(),
                        )
                    } else {
                        TermError::Io(format!("could not write to the terminal ({e}); some input may not have been delivered."))
                    }
                })
            });
            match tokio::time::timeout(WRITE_TIMEOUT, job).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(TermError::Io(format!("the terminal writer stopped ({e}); some input may not have been delivered."))),
                Err(_) => Err(TermError::Io(format!(
                    "the terminal did not take the input within {} s (its program is not reading); the rest is still queued. Read the screen, or close the terminal.",
                    WRITE_TIMEOUT.as_secs()
                ))),
            }
        })
    }

    fn resize(&self, size: Size) -> Result<(), TermError> {
        {
            let master = lk(&self.inner.master);
            let Some(m) = master.as_ref() else {
                return Err(TermError::Closed("this terminal was closed; nothing was resized.".into()));
            };
            m.resize(PtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|e| TermError::Io(format!("the host refused to resize the terminal ({e}); its size is unchanged.")))?;
        }
        let mut st = lk(&self.inner.state);
        st.parser.set_size(size.rows, size.cols);
        st.size = size;
        self.inner.bump(&mut st);
        Ok(())
    }

    fn close(&self) -> BoxFuture<'_, Result<Closed, TermError>> {
        Box::pin(async move {
            if let Some(c) = lk(&self.inner.state).closed.clone() {
                return Ok(Closed { killed: vec![], exit: c.exit, note: Some("the terminal was already closed.".into()) });
            }
            let leader = self.inner.identity.pid;
            let report = tokio::task::spawn_blocking(move || tree::kill_tree(leader))
                .await
                .map_err(|e| TermError::Io(format!("the process-tree kill did not finish ({e}); the terminal may still be running.")))?;
            self.inner.hang_up();
            let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
            loop {
                let seq = {
                    let st = lk(&self.inner.state);
                    if st.exit.is_some() {
                        break;
                    }
                    st.seq
                };
                if tokio::time::timeout_at(deadline, self.changed(seq)).await.is_err() {
                    break;
                }
            }
            let mut st = lk(&self.inner.state);
            let exit = st.exit;
            let note = match (report.note, exit) {
                (n, Some(_)) => n,
                (Some(n), None) => Some(format!("{n}; the leader had not been reaped yet")),
                (None, None) => Some(format!("process {leader} had not been reaped {} s after the kill", CLOSE_GRACE.as_secs())),
            };
            let closed = Closed { killed: report.killed, exit, note };
            st.closed = Some(closed.clone());
            self.inner.bump(&mut st);
            Ok(closed)
        })
    }
}
