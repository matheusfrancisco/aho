//! `12` "Reload = exec-in-place with a readiness fd": the parent learns that a new binary is
//! serving from exactly one byte ([`READY_BYTE`]) written on a descriptor it passed down. The
//! descriptor number travels in [`READY_FD_ENV`].
//!
//! Descriptors cross `exec` only when their close-on-exec flag is clear, and std opens every
//! descriptor with it set and has no API to clear it; that `fcntl` (in this process, or in a
//! forked child before exec) is the whole reason this module is allowed `unsafe`.
//!
//! The environment variable is never removed from the process environment (`remove_var` is
//! unsound once threads exist). Instead the descriptor is taken with close-on-exec set, so no
//! subprocess inherits it, and every subprocess spawner must `env_remove(READY_FD_ENV)` so no
//! subprocess is told about a descriptor it does not have.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::Duration;

pub const READY_FD_ENV: &str = "AHO_READY_FD";
pub const READY_BYTE: u8 = 0x06;

/// The write end handed to this process by whoever started it and waits for readiness.
#[derive(Debug)]
pub struct ReadyFd(OwnedFd);

/// Takes the readiness descriptor named by `AHO_READY_FD`, if this process was given one.
///
/// Call once, at startup, before spawning anything. Returns `None` when the variable is unset or
/// malformed, names a standard stream, names a closed descriptor, or names a descriptor that has
/// close-on-exec set — a descriptor passed across `exec` never has it, so a set flag means the
/// descriptor already belongs to someone in this process (for example an earlier call). The
/// taken descriptor gets close-on-exec so no subprocess inherits it.
#[allow(clippy::disallowed_methods, reason = "authority: daemon-ready-env")]
pub fn take_ready_fd() -> Option<ReadyFd> {
    let raw: RawFd = std::env::var(READY_FD_ENV).ok()?.trim().parse().ok()?;
    if raw <= 2 {
        return None;
    }
    // SAFETY: F_GETFD takes no pointer argument; on a descriptor that is not open it fails with
    // EBADF and reads or writes no memory.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
        return None;
    }
    // SAFETY: the descriptor is open (F_GETFD succeeded) and arrived across exec (close-on-exec
    // is clear, which std never leaves on a descriptor it owns), handed to this process through
    // AHO_READY_FD for this purpose alone, so nothing else in the process owns it. Setting
    // close-on-exec right below makes a second call return `None` instead of a second owner.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    set_cloexec(fd.as_fd(), true).ok()?;
    Some(ReadyFd(fd))
}

impl ReadyFd {
    /// Writes exactly [`READY_BYTE`], then closes the descriptor.
    pub fn signal(self) -> io::Result<()> {
        let mut out = std::fs::File::from(self.0);
        out.write_all(&[READY_BYTE])
    }
}

impl AsFd for ReadyFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Clears close-on-exec on `fd` so it survives `exec` — "passing the fd across exec". std has no
/// API for this.
pub fn inheritable(fd: BorrowedFd<'_>) -> io::Result<()> {
    set_cloexec(fd, false)
}

fn set_cloexec(fd: BorrowedFd<'_>, on: bool) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `fd` is borrowed, so the descriptor stays open for this call; F_GETFD passes no
    // pointer and only reads the descriptor flags.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
    if next != flags {
        // SAFETY: same open, borrowed descriptor; F_SETFD takes an integer and changes only its
        // close-on-exec flag.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, next) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Why waiting for readiness failed. `Display` is operator-facing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadyError {
    /// End of file without the byte: the new binary died (or dropped the descriptor) before it
    /// was ready.
    Closed,
    Timeout,
    Io(String),
}

impl fmt::Display for ReadyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadyError::Closed => write!(
                f,
                "the new process exited before it signalled that it was ready, so nothing was switched over to it. Run it by hand to see why it does not start"
            ),
            ReadyError::Timeout => write!(
                f,
                "the new process did not signal readiness in time, so we stopped waiting for it. Check whether it hangs while starting, or allow a longer timeout"
            ),
            ReadyError::Io(e) => write!(
                f,
                "waiting for the new process to become ready failed ({e}), so nothing was switched over. Check that it can be started and try again"
            ),
        }
    }
}

impl std::error::Error for ReadyError {}

/// The read end of a readiness pipe.
#[derive(Debug)]
pub struct ReadyWait {
    reader: io::PipeReader,
}

/// A fresh pipe: the waiter keeps the read end, the child gets the write end. Both ends start
/// with close-on-exec set.
pub fn ready_pipe() -> io::Result<(ReadyWait, io::PipeWriter)> {
    let (reader, writer) = io::pipe()?;
    Ok((ReadyWait { reader }, writer))
}

impl ReadyWait {
    /// Waits until [`READY_BYTE`] arrives (`Ok`), every write end is closed first
    /// ([`ReadyError::Closed`]), or `timeout` passes ([`ReadyError::Timeout`]). Other bytes are
    /// skipped. Every write end must be closed in this process before waiting, or a dead child
    /// reads as a timeout.
    ///
    /// A helper thread does the blocking reads. After a timeout it keeps blocking until the last
    /// write end closes (the child exits or is killed), then ends by itself.
    pub fn wait(self, timeout: Duration) -> Result<(), ReadyError> {
        let (done, outcome) = mpsc::sync_channel(1);
        let mut reader = self.reader;
        std::thread::Builder::new()
            .name("aho-ready-wait".to_string())
            .spawn(move || {
                let mut byte = [0u8; 1];
                let result = loop {
                    match reader.read(&mut byte) {
                        Ok(0) => break Err(ReadyError::Closed),
                        Ok(_) if byte[0] == READY_BYTE => break Ok(()),
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => break Err(ReadyError::Io(e.to_string())),
                    }
                };
                let _ = done.send(result);
            })
            .map_err(|e| ReadyError::Io(format!("could not start the thread that waits for readiness: {e}")))?;
        match outcome.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ReadyError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(ReadyError::Io("the thread waiting for readiness stopped without an answer".to_string()))
            }
        }
    }
}

/// Starts `cmd` with a readiness pipe and waits up to `timeout` for its byte.
///
/// The write end is made inheritable only inside the forked child (a `pre_exec` hook), never in
/// this process, so a subprocess spawned concurrently by another thread cannot inherit it and
/// turn a dead child into a timeout. After this call `cmd` carries `env_remove(READY_FD_ENV)`;
/// do not spawn it again for readiness. On failure the child is killed and reaped.
#[allow(clippy::disallowed_methods, reason = "authority: daemon-ready-spawn")]
pub fn spawn_with_ready(cmd: &mut Command, timeout: Duration) -> Result<Child, ReadyError> {
    let (wait, writer) = ready_pipe().map_err(|e| ReadyError::Io(format!("could not create the readiness pipe: {e}")))?;
    let raw = writer.as_raw_fd();
    cmd.env(READY_FD_ENV, raw.to_string());
    // SAFETY: the hook runs in the forked child before exec, where only async-signal-safe calls
    // are allowed; `clear_cloexec_raw` makes two fcntl calls on a descriptor number the child
    // inherited from `writer` (still open in this process until `spawn` returns), and allocates
    // nothing and takes no locks.
    unsafe {
        cmd.pre_exec(move || clear_cloexec_raw(raw));
    }
    let spawned = cmd.spawn();
    cmd.env_remove(READY_FD_ENV);
    drop(writer);
    let mut child = spawned.map_err(|e| ReadyError::Io(format!("could not start the process: {e}")))?;
    match wait.wait(timeout) {
        Ok(()) => Ok(child),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// Async-signal-safe: runs between fork and exec.
fn clear_cloexec_raw(raw: RawFd) -> io::Result<()> {
    // SAFETY: fcntl with F_GETFD/F_SETFD takes integers only and is async-signal-safe; a closed
    // descriptor fails with EBADF, which is reported as the spawn error.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    // SAFETY: as above; only the close-on-exec flag of `raw` changes.
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Replaces this process image with `cmd`, handing it `fd` as its readiness descriptor (same
/// pid: exec in place). Returns only when exec failed; `fd` then gets close-on-exec back so
/// later subprocesses do not inherit it, and the standard streams are put back: `exec` applies
/// `cmd`'s stdio redirections to this very process before `execve`, so without that a failed
/// reload would leave the daemon writing its log to wherever the new image's streams pointed.
/// Between marking `fd` inheritable and exec, a subprocess spawned by another thread would
/// inherit it: stop spawning first.
#[allow(clippy::disallowed_methods, reason = "authority: daemon-ready-exec")]
pub fn exec_with_ready(cmd: &mut Command, fd: BorrowedFd<'_>) -> io::Error {
    if let Err(e) = inheritable(fd) {
        return e;
    }
    let saved = SavedStdio::save();
    cmd.env(READY_FD_ENV, fd.as_raw_fd().to_string());
    let err = cmd.exec();
    let restored = saved.restore();
    let _ = set_cloexec(fd, true);
    cmd.env_remove(READY_FD_ENV);
    match restored {
        Ok(()) => err,
        Err(e) => io::Error::new(err.kind(), format!("{err}; the standard streams could not be put back either ({e})")),
    }
}

/// Close-on-exec copies of this process's standard streams, taken before an exec that may fail.
struct SavedStdio([Option<OwnedFd>; 3]);

impl SavedStdio {
    fn save() -> SavedStdio {
        let copy = |fd: BorrowedFd<'_>| fd.try_clone_to_owned().ok();
        SavedStdio([copy(io::stdin().as_fd()), copy(io::stdout().as_fd()), copy(io::stderr().as_fd())])
    }

    /// Points 0, 1 and 2 back at the saved copies (a stream that was closed stays as it is).
    fn restore(self) -> io::Result<()> {
        let mut first_error = None;
        for (target, saved) in (0..).zip(self.0) {
            let Some(saved) = saved else { continue };
            // SAFETY: dup2 takes two integers and touches no memory: `saved` is a descriptor this
            // function owns (open until it drops below), and `target` is 0, 1 or 2, which dup2
            // atomically re-points at the same open file description.
            if unsafe { libc::dup2(saved.as_raw_fd(), target) } < 0 && first_error.is_none() {
                first_error = Some(io::Error::last_os_error());
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
