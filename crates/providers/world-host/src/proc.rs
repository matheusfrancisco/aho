//! `proc-host`: processes on the host, each in its own process group so a kill reaches
//! backgrounded grandchildren. The only `unsafe` in this crate is `signal_group`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aho_log::ProcessIdentity;
use aho_proc::{Child, Exit, OutputBuffer, ProcError, SpawnSpec, Subprocess, TransportChild};
use aho_types::{BoxFuture, Timestamp};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// Output kept per child (the op table keeps its own, smaller ring).
pub const CHILD_OUTPUT_CAP: usize = 8 << 20;
/// SIGTERM → SIGKILL grace for a group kill.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// After the leader exits, how long its pipes may keep draining before descendants holding
/// them are cut off ("a waited read ends when the process ends, not its last descendant").
pub const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Send `sig` to every process in group `pgid`.
///
/// Refuses `pgid <= 1`: `kill(-1, …)` would signal every process the user owns.
#[allow(unsafe_code)]
fn signal_group(pgid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let pgid = libc::pid_t::try_from(pgid).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    if pgid <= 1 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: kill(2) takes two plain integers and touches no memory we own; it has no
    // memory-safety preconditions. `-pgid` addresses the process group this crate created with
    // `process_group(0)` for one child (its leader's pid), and `pgid > 1` is checked above.
    let rc = unsafe { libc::kill(-pgid, sig) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn group_alive(pgid: u32) -> bool {
    match signal_group(pgid, 0) {
        Ok(()) => true,
        Err(e) => e.raw_os_error() == Some(libc::EPERM),
    }
}

#[derive(Clone)]
pub struct HostSubprocess {
    root: PathBuf,
}

type Pipes = (tokio::process::ChildStdin, tokio::process::ChildStdout, tokio::process::ChildStderr);

impl HostSubprocess {
    pub fn new(root: &Path) -> HostSubprocess {
        HostSubprocess { root: root.to_path_buf() }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-transport")]
    async fn start(&self, spec: SpawnSpec, interactive: bool, transport: bool) -> Result<(Arc<dyn Child>, Option<Pipes>), ProcError> {
        let Some(program) = spec.argv.first() else {
            return Err(ProcError::Denied("empty argv; nothing was started.".into()));
        };
        let cwd = match &spec.cwd {
            Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
            Some(c) => self.root.join(c),
            None => self.root.clone(),
        };
        if !cwd.is_dir() {
            return Err(ProcError::NotFound(format!("working directory {}", cwd.display())));
        }
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&spec.argv[1..])
            .current_dir(&cwd)
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            // `12` reload: the daemon's readiness fd is close-on-exec; never tell a tool about it (daemon::ready::READY_FD_ENV).
            .env_remove("AHO_READY_FD")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if interactive || spec.stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProcError::NotFound(program.clone()),
            std::io::ErrorKind::PermissionDenied => ProcError::Denied(format!("the host refused to run `{program}` ({e}); nothing ran.")),
            _ => ProcError::Io(format!("could not start `{program}`: {e}; nothing ran.")),
        })?;
        let pid = child.id().ok_or_else(|| ProcError::Io(format!("`{program}` exited before it could be tracked")))?;
        let (out_tx, _) = watch::channel(0u64);
        let (exit_tx, _) = watch::channel(None);
        let inner = Arc::new(ChildInner {
            identity: ProcessIdentity { pid, start_time: Timestamp::now() },
            out: Mutex::new(OutputBuffer::new(CHILD_OUTPUT_CAP)),
            out_tx,
            done: AtomicBool::new(false),
            exit_tx,
            exited: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            stdin: tokio::sync::Mutex::new(None),
        });

        let pipes = if transport {
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(stdin), Some(stdout), Some(stderr)) => Some((stdin, stdout, stderr)),
                _ => return Err(ProcError::Io("the protocol process started without its pipes; it was stopped.".into())),
            }
        } else {
            None
        };
        // Readers first: a child that fills its stdout before reading stdin must not deadlock us.
        let readers: Vec<tokio::task::JoinHandle<()>> = [
            child.stdout.take().map(|s| tokio::spawn(pump(inner.clone(), s))),
            child.stderr.take().map(|s| tokio::spawn(pump(inner.clone(), s))),
        ]
        .into_iter()
        .flatten()
        .collect();

        let mut stdin = child.stdin.take();
        if interactive {
            if let (Some(bytes), Some(pipe)) = (&spec.stdin, stdin.as_mut()) {
                pipe.write_all(bytes).await.map_err(|e| ProcError::Io(format!("could not write stdin of `{program}`: {e}")))?;
            }
            *inner.stdin.lock().await = stdin;
        } else if let (Some(bytes), Some(mut pipe)) = (spec.stdin.clone(), stdin.take()) {
            // Written off-task, then closed so the child sees EOF.
            tokio::spawn(async move {
                let _ = pipe.write_all(&bytes).await;
            });
        }

        let sup = inner.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            sup.exited.store(true, Ordering::SeqCst);
            let result = match status {
                _ if sup.timed_out.load(Ordering::SeqCst) => Err(ProcError::Timeout),
                Ok(st) => Ok(exit_of(st)),
                Err(e) => Err(ProcError::Io(format!("lost track of process {pid}: {e}"))),
            };
            sup.exit_tx.send_replace(Some(result));
            let mut readers = readers;
            let drained = tokio::time::timeout(DRAIN_GRACE, async {
                for r in readers.iter_mut() {
                    let _ = r.await;
                }
            })
            .await;
            if drained.is_err() {
                for r in &readers {
                    r.abort();
                }
            }
            *sup.stdin.lock().await = None;
            sup.done.store(true, Ordering::SeqCst);
            sup.out_tx.send_modify(|n| *n += 1);
        });

        if let Some(ms) = spec.timeout_ms {
            let t = inner.clone();
            let mut exit_rx = inner.exit_tx.subscribe();
            tokio::spawn(async move {
                let exited = async move {
                    while exit_rx.borrow_and_update().is_none() {
                        if exit_rx.changed().await.is_err() {
                            return;
                        }
                    }
                };
                let timed_out = tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => !t.exited.load(Ordering::SeqCst),
                    _ = exited => false,
                };
                if timed_out {
                    t.timed_out.store(true, Ordering::SeqCst);
                    t.kill_group().await;
                }
            });
        }
        Ok((Arc::new(HostChild { inner }), pipes))
    }
}

impl Subprocess for HostSubprocess {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.start(spec, false, false).await.map(|(child, _)| child) })
    }
    fn spawn_interactive(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.start(spec, true, false).await.map(|(child, _)| child) })
    }
    fn spawn_transport(&self, mut spec: SpawnSpec) -> BoxFuture<'_, Result<TransportChild, ProcError>> {
        Box::pin(async move {
            let initial = spec.stdin.take();
            let (child, pipes) = self.start(spec, true, true).await?;
            let Some((mut stdin, stdout, stderr)) = pipes else {
                return Err(ProcError::Io("the host did not return protocol pipes; the process was stopped.".into()));
            };
            if let Some(bytes) = initial {
                stdin.write_all(&bytes).await.map_err(|e| ProcError::Io(format!("could not write protocol stdin: {e}")))?;
            }
            Ok(TransportChild { child, stdin: Box::new(stdin), stdout: Box::new(stdout), stderr: Box::new(stderr) })
        })
    }
}

fn exit_of(st: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(code), _) => Exit { code, signal: None },
        (None, Some(sig)) => Exit { code: 128 + sig, signal: Some(sig) },
        (None, None) => Exit { code: -1, signal: None },
    }
}

async fn pump(inner: Arc<ChildInner>, mut src: impl AsyncRead + Unpin) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                inner.lock_out().push(&buf[..n]);
                inner.out_tx.send_modify(|c| *c += 1);
            }
        }
    }
}

struct ChildInner {
    identity: ProcessIdentity,
    out: Mutex<OutputBuffer>,
    out_tx: watch::Sender<u64>,
    /// Output is final (leader exited and pipes drained or cut off).
    done: AtomicBool,
    exit_tx: watch::Sender<Option<Result<Exit, ProcError>>>,
    exited: AtomicBool,
    timed_out: AtomicBool,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
}

impl ChildInner {
    fn lock_out(&self) -> MutexGuard<'_, OutputBuffer> {
        self.out.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// SIGTERM to the group, up to `KILL_GRACE` for it to disappear, then SIGKILL.
    async fn kill_group(&self) {
        let pgid = self.identity.pid;
        if signal_group(pgid, libc::SIGTERM).is_err() {
            return;
        }
        let deadline = tokio::time::Instant::now() + KILL_GRACE;
        while tokio::time::Instant::now() < deadline {
            if self.exited.load(Ordering::SeqCst) && !group_alive(pgid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = signal_group(pgid, libc::SIGKILL);
    }
}

pub struct HostChild {
    inner: Arc<ChildInner>,
}

impl Drop for HostChild {
    /// A child nobody holds any more cannot be waited on or killed; do not leave it running.
    fn drop(&mut self) {
        if !self.inner.exited.load(Ordering::SeqCst) {
            let _ = signal_group(self.inner.identity.pid, libc::SIGKILL);
        }
    }
}

impl Child for HostChild {
    fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    fn wait(&self) -> BoxFuture<'_, Result<Exit, ProcError>> {
        let mut rx = self.inner.exit_tx.subscribe();
        Box::pin(async move {
            loop {
                if let Some(r) = rx.borrow_and_update().clone() {
                    return r;
                }
                if rx.changed().await.is_err() {
                    return Err(ProcError::Io("the process supervisor stopped; its exit status is unknown.".into()));
                }
            }
        })
    }

    fn kill(&self) -> BoxFuture<'_, Result<(), ProcError>> {
        Box::pin(async move {
            self.inner.kill_group().await;
            Ok(())
        })
    }

    fn output(&self, cursor: u64) -> (Vec<u8>, u64, bool) {
        // `done` first: if it is set, the buffer read after it is final.
        let done = self.inner.done.load(Ordering::SeqCst);
        let c = self.inner.lock_out().read(cursor);
        (c.bytes, c.next, done)
    }

    fn readable(&self, cursor: u64) -> BoxFuture<'_, ()> {
        let mut rx = self.inner.out_tx.subscribe();
        Box::pin(async move {
            loop {
                if self.inner.done.load(Ordering::SeqCst) || self.inner.lock_out().end() > cursor {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    }

    fn write_stdin(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), ProcError>> {
        Box::pin(async move {
            let mut g = self.inner.stdin.lock().await;
            let Some(pipe) = g.as_mut() else {
                return Err(ProcError::Denied(
                    "the process's stdin is closed (it exited or was not started interactive); nothing was written.".into(),
                ));
            };
            pipe.write_all(&bytes).await.map_err(|e| ProcError::Io(format!("could not write to the process's stdin: {e}")))?;
            pipe.flush().await.map_err(|e| ProcError::Io(format!("could not flush the process's stdin: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_log::{OpKind, OpStatus};
    use aho_proc::OpTable;
    use aho_types::CallId;

    async fn drain(c: &dyn Child) -> String {
        let (mut cur, mut out) = (0, Vec::new());
        loop {
            let (b, next, done) = c.output(cur);
            out.extend(b);
            cur = next;
            if done {
                return String::from_utf8_lossy(&out).into_owned();
            }
            c.readable(cur).await;
        }
    }

    /// Polls until `pid` no longer exists (`kill -0` fails); reaping by launchd/init may lag a kill.
    async fn gone(pid: u32) -> bool {
        for _ in 0..200 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(true);
            if !alive {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn group_kill_reaches_a_backgrounded_grandchild() {
        let p = HostSubprocess::new(Path::new("/"));
        let c = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!; sleep 30"])).await.unwrap();
        let mut cur = 0;
        let mut text = String::new();
        while !text.contains('\n') {
            c.readable(cur).await;
            let (b, next, _) = c.output(cur);
            text.push_str(&String::from_utf8_lossy(&b));
            cur = next;
        }
        let grandchild: u32 = text.trim().parse().unwrap();
        let t0 = std::time::Instant::now();
        c.kill().await.unwrap();
        let exit = c.wait().await.unwrap();
        assert_eq!(exit, Exit { code: 128 + libc::SIGTERM, signal: Some(libc::SIGTERM) });
        assert!(t0.elapsed() < KILL_GRACE + Duration::from_secs(1));
        assert!(gone(grandchild).await, "grandchild {grandchild} survived");
    }

    #[tokio::test]
    async fn exit_codes_pass_through_including_128_plus_signal() {
        let p = HostSubprocess::new(Path::new("/"));
        let c = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "echo out; echo err >&2; exit 7"])).await.unwrap();
        assert_eq!(c.wait().await.unwrap(), Exit { code: 7, signal: None });
        let text = drain(c.as_ref()).await;
        assert!(text.contains("out\n") && text.contains("err\n"), "stdout and stderr both captured: {text:?}");
        let c = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "kill -KILL $$"])).await.unwrap();
        assert_eq!(c.wait().await.unwrap(), Exit { code: 137, signal: Some(9) });
        assert!(matches!(p.spawn(SpawnSpec::new(&["/definitely/not/a/binary"])).await, Err(ProcError::NotFound(_))));
    }

    #[tokio::test]
    async fn a_waited_read_returns_when_the_process_ends_not_when_its_descendant_does() {
        let p = HostSubprocess::new(Path::new("/"));
        let t0 = std::time::Instant::now();
        let c = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!"])).await.unwrap();
        assert_eq!(c.wait().await.unwrap().code, 0);
        let text = drain(c.as_ref()).await;
        assert!(t0.elapsed() < Duration::from_secs(5), "blocked on the descendant for {:?}", t0.elapsed());
        let grandchild: u32 = text.trim().parse().unwrap();
        let _ = signal_group(c.identity().pid, libc::SIGKILL);
        assert!(gone(grandchild).await);
    }

    #[tokio::test]
    async fn a_timeout_kills_the_group_and_reports_timeout() {
        let p = HostSubprocess::new(Path::new("/"));
        let spec = SpawnSpec { timeout_ms: Some(200), ..SpawnSpec::new(&["/bin/sh", "-c", "sleep 30"]) };
        let t0 = std::time::Instant::now();
        let c = p.spawn(spec).await.unwrap();
        assert_eq!(c.wait().await, Err(ProcError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn stdin_cwd_and_env_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let p = HostSubprocess::new(dir.path());
        let spec = SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "pwd; echo $AHO_T; cat".into()],
            cwd: Some("sub".into()),
            env: vec![("AHO_T".into(), "v1".into())],
            stdin: Some(b"from-stdin".to_vec()),
            timeout_ms: None,
        };
        let c = p.spawn(spec).await.unwrap();
        c.wait().await.unwrap();
        let text = drain(c.as_ref()).await;
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with("/sub"), "{text}");
        assert_eq!(&lines[1..], &["v1", "from-stdin"]);
        assert!(matches!(
            p.spawn(SpawnSpec { cwd: Some("missing".into()), ..SpawnSpec::new(&["true"]) }).await,
            Err(ProcError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn interactive_children_keep_stdin_open() {
        let p = HostSubprocess::new(Path::new("/"));
        let c = p.spawn_interactive(SpawnSpec::new(&["/bin/sh"])).await.unwrap();
        c.write_stdin(b"echo one\n".to_vec()).await.unwrap();
        c.write_stdin(b"echo two; exit 0\n".to_vec()).await.unwrap();
        assert_eq!(c.wait().await.unwrap().code, 0);
        assert_eq!(drain(c.as_ref()).await, "one\ntwo\n");
        assert!(matches!(c.write_stdin(b"x".to_vec()).await, Err(ProcError::Denied(_))));
        let plain = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "sleep 5"])).await.unwrap();
        assert!(matches!(plain.write_stdin(b"x".to_vec()).await, Err(ProcError::Denied(_))));
        plain.kill().await.unwrap();
    }

    #[tokio::test]
    async fn op_table_cancel_kills_an_attached_childs_process_group() {
        let p = HostSubprocess::new(Path::new("/"));
        let ops = OpTable::new();
        let (op, token) = ops.register(CallId("c1".into()), "bash", OpKind::Process, false, true);
        let c = p.spawn(SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & echo $!; sleep 30"])).await.unwrap();
        ops.attach_child(op, c.clone());
        assert_eq!(ops.info(op).unwrap().identity, Some(c.identity()));
        let mut cur = 0;
        let mut text = String::new();
        while !text.contains('\n') {
            c.readable(cur).await;
            let (b, next, _) = c.output(cur);
            text.push_str(&String::from_utf8_lossy(&b));
            cur = next;
        }
        let grandchild: u32 = text.trim().parse().unwrap();
        assert!(ops.cancel(op));
        assert!(token.is_cancelled());
        let exit = tokio::time::timeout(Duration::from_secs(5), c.wait()).await.unwrap().unwrap();
        assert!(exit.signal.is_some());
        assert!(gone(grandchild).await);
        ops.finish(op, OpStatus::Cancelled { reason: aho_log::CancelReason::Aborted });
        assert!(!ops.cancel(op));
    }

    #[test]
    fn signal_group_refuses_init_and_everyone() {
        assert!(signal_group(0, 0).is_err());
        assert!(signal_group(1, 0).is_err());
    }
}
