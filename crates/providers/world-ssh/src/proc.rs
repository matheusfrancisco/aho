//! `proc-ssh`: processes on the remote host, each in its own process group.
//!
//! A spawn is one `ssh -T` session running [`LAUNCHER`] (perl, present wherever git is): it
//! `chdir`s, forks the command into a new process group with stdout+stderr on a private pipe,
//! and reports on the ssh stderr channel with `AHO-SSH` control lines — `P <pgid>` once the
//! command is running, `E <what> <errno> <text>` if it could not start, `X <code> <signal>` when
//! the leader exits (exact status: `128+signal` and the signal number). The command's output is
//! copied to the ssh stdout until the leader exits plus a short drain window, so a backgrounded
//! grandchild holding the pipe cannot hold the caller (07). Kills and timeouts run [`KILLER`]
//! over the same master: SIGTERM to the group, a 2 s grace, then SIGKILL.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aho_log::ProcessIdentity;
use aho_proc::{Child, Exit, OutputBuffer, ProcError, SpawnSpec, Subprocess, TransportChild};
use aho_types::{BoxFuture, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::ssh::{SSH_FAILED, Ssh, classify, quote};

/// Output kept per child (the op table keeps its own, smaller ring).
pub const CHILD_OUTPUT_CAP: usize = 8 << 20;
/// After the remote leader exited, how long the local side waits for the ssh stdout to close.
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Prefix of the launcher's control lines on the ssh stderr channel.
const CONTROL: &str = "AHO-SSH ";

/// `perl -e LAUNCHER -- <transport> <cwd> <n env pairs> <k v>… <argv>…`.
pub(crate) const LAUNCHER: &str = r#"use strict; use warnings; use POSIX (); use Time::HiRes ();
$SIG{PIPE} = "IGNORE";
my $transport = shift @ARGV; my $cwd = shift @ARGV; my $n = shift @ARGV;
my %env; for (1 .. $n) { my $k = shift @ARGV; $env{$k} = shift @ARGV; }
my @cmd = @ARGV;
sub ctl {
  my $m = "AHO-SSH " . join(" ", @_) . "\n"; my $o = 0;
  while ($o < length $m) {
    my $k = syswrite(STDERR, $m, length($m) - $o, $o);
    if (!defined $k) { next if $!{EINTR}; die "control pipe closed\n"; }
    $o += $k;
  }
}
if (!chdir $cwd) { ctl("E", "cwd", 0 + $!, "$!"); exit 0; }
my ($r, $w, $er, $ew, $sr, $sw);
if (!pipe($r, $w) || !pipe($er, $ew) || ($transport && !pipe($sr, $sw))) { ctl("E", "io", 0 + $!, "$!"); exit 0; }
my $pid = fork();
if (!defined $pid) { ctl("E", "io", 0 + $!, "$!"); exit 0; }
if ($pid == 0) {
  close $r; close $er;
  close $sr if $transport;
  POSIX::setpgid(0, 0);
  open(STDOUT, ">&", $w) or POSIX::_exit(126);
  open(STDERR, ">&", $transport ? $sw : $w) or POSIX::_exit(126);
  close $w; close $sw if $transport;
  $SIG{$_} = "DEFAULT" for qw(PIPE INT QUIT TERM HUP);
  $ENV{$_} = $env{$_} for keys %env;
  delete $ENV{AHO_READY_FD};
  { no warnings; exec { $cmd[0] } @cmd; }
  syswrite($ew, (0 + $!) . " $!");
  POSIX::_exit(127);
}
POSIX::setpgid($pid, $pid);
close $w; close $ew;
close $sw if $transport;
$SIG{HUP} = $SIG{TERM} = sub { kill("KILL", -$pid); exit 0; };
my $err = ""; sysread($er, $err, 256); close $er;
if (length $err) { waitpid($pid, 0); ctl("E", "exec", $err); exit 0; }
ctl("P", $pid);
sub out {
  my $d = shift; my $o = 0;
  while ($o < length $d) {
    my $k = syswrite(STDOUT, $d, length($d) - $o, $o);
    if (!defined $k) { next if $!{EINTR}; return 0; }
    $o += $k;
  }
  return 1;
}
my ($st, $t0, $buf, $reported) = (undef, 0, "", 0);
my $stderr_fd = $transport ? fileno($sr) : -1;
my @pipes = ($r); push @pipes, $sr if $transport;
while (1) {
  if (!defined $st && waitpid($pid, POSIX::WNOHANG()) == $pid) { $st = $?; $t0 = Time::HiRes::time(); }
  if (defined $st) {
    if ($transport && !$reported) { my $sig = $st & 127; ctl("X", $sig ? 128 + $sig : ($st >> 8), $sig); $reported = 1; }
    last if !@pipes || (!$transport && Time::HiRes::time() - $t0 > 0.25);
  }
  elsif (!@pipes) { waitpid($pid, 0); $st = $?; last; }
  my $rin = ""; vec($rin, fileno($_), 1) = 1 for @pipes;
  my $ready = select(my $rout = $rin, undef, undef, 0.02);
  next if $ready <= 0;
  my @ready = @pipes;
  for my $pipe (@ready) {
    next unless vec($rout, fileno($pipe), 1);
    my $k = sysread($pipe, $buf, 16384);
    if (!defined $k) { next if $!{EINTR}; $k = 0; }
    if ($k == 0) { @pipes = grep { fileno($_) != fileno($pipe) } @pipes; close $pipe; next; }
    if ($transport && fileno($pipe) == $stderr_fd) {
      eval { ctl("D", unpack("H*", $buf)); };
      if ($@) { kill("KILL", -$pid); exit 0; }
    } elsif (!out($buf)) { kill("TERM", -$pid); Time::HiRes::sleep(0.2); kill("KILL", -$pid); exit 0; }
  }
}
close $_ for @pipes;
my $sig = $st & 127;
ctl("X", $sig ? 128 + $sig : ($st >> 8), $sig) unless $reported;
close STDOUT;
exit 0;
"#;

/// `perl -e KILLER -- <pgid> [KILL]`: TERM, up to 2 s for the group to vanish, then KILL.
pub(crate) const KILLER: &str = r#"my ($g, $now) = @ARGV;
exit 2 unless defined $g && $g =~ /^[0-9]+$/ && $g > 1;
if (defined $now) { kill("KILL", -$g); exit 0; }
kill("TERM", -$g) or exit 0;
for (1 .. 100) { select(undef, undef, undef, 0.02); exit 0 unless kill(0, -$g) || $!{EPERM}; }
kill("KILL", -$g);
exit 0;
"#;

fn launcher_command(cwd: &str, spec: &SpawnSpec, transport: bool) -> String {
    let mut cmd = format!("exec perl -e {} -- {} {} {}", quote(LAUNCHER), u8::from(transport), quote(cwd), spec.env.len());
    for (k, v) in &spec.env {
        cmd.push(' ');
        cmd.push_str(&quote(k));
        cmd.push(' ');
        cmd.push_str(&quote(v));
    }
    for a in &spec.argv {
        cmd.push(' ');
        cmd.push_str(&quote(a));
    }
    cmd
}

fn killer_command(pgid: u32, now: bool) -> String {
    format!("exec perl -e {} -- {pgid}{}", quote(KILLER), if now { " KILL" } else { "" })
}

/// One parsed control line.
#[derive(Debug, PartialEq, Eq)]
enum Control {
    Started(u32),
    Failed { what: String, errno: i32, text: String },
    Exited(Exit),
    Stderr(Vec<u8>),
}

fn parse_control(line: &str) -> Option<Control> {
    let rest = line.trim_end().strip_prefix(CONTROL)?;
    let mut it = rest.splitn(4, ' ');
    match it.next()? {
        "P" => it.next()?.parse().ok().map(Control::Started),
        "D" => {
            let hex = it.next()?;
            if hex.len() % 2 != 0 {
                return None;
            }
            let bytes = hex
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&[high, low]| {
                    let high = (high as char).to_digit(16)?;
                    let low = (low as char).to_digit(16)?;
                    Some(((high << 4) | low) as u8)
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Control::Stderr(bytes))
        }
        "X" => {
            let code = it.next()?.parse().ok()?;
            let sig: i32 = it.next()?.parse().ok()?;
            Some(Control::Exited(Exit { code, signal: (sig != 0).then_some(sig) }))
        }
        "E" => {
            let what = it.next()?.to_string();
            let errno = it.next()?.parse().ok()?;
            Some(Control::Failed { what, errno, text: it.next().unwrap_or("").to_string() })
        }
        _ => None,
    }
}

#[derive(Clone)]
pub struct SshSubprocess {
    ssh: Arc<Ssh>,
    root: String,
}

type TransportPipes = (tokio::process::ChildStdin, tokio::process::ChildStdout, tokio::io::DuplexStream);

impl SshSubprocess {
    pub fn new(ssh: Arc<Ssh>, root: String) -> SshSubprocess {
        SshSubprocess { ssh, root }
    }

    fn host(&self) -> String {
        self.ssh.target().display()
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-transport")]
    async fn start(
        &self,
        spec: SpawnSpec,
        interactive: bool,
        transport: bool,
    ) -> Result<(Arc<dyn Child>, Option<TransportPipes>), ProcError> {
        let Some(program) = spec.argv.first().cloned() else {
            return Err(ProcError::Denied("empty argv; nothing was started.".into()));
        };
        let cwd = match &spec.cwd {
            Some(c) if c.starts_with('/') => c.clone(),
            Some(c) => format!("{}/{c}", self.root),
            None => self.root.clone(),
        };
        let mut cmd = self.ssh.command(&launcher_command(&cwd, &spec, transport));
        cmd.stdin(if interactive || spec.stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Terminal signals aimed at aho must not cut the connection under a running command.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProcError::NotFound("ssh".into()),
            _ => ProcError::Io(format!("could not start ssh to {}: {e}; nothing ran.", self.host())),
        })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ProcError::Io("ssh started without its output pipes; nothing ran.".into()));
        };
        // Written before the handshake: a command that reads stdin first must see it.
        let mut stdin = child.stdin.take();
        if interactive {
            if let (Some(bytes), Some(pipe)) = (&spec.stdin, stdin.as_mut()) {
                pipe.write_all(bytes).await.map_err(|e| ProcError::Io(format!("could not write stdin of `{program}`: {e}")))?;
            }
        } else if let (Some(bytes), Some(mut pipe)) = (spec.stdin.clone(), stdin.take()) {
            // Off-task, then closed so the command sees EOF. A command that exits without reading
            // all of it makes the write fail; its exit status already tells that story.
            tokio::spawn(async move { pipe.write_all(&bytes).await.unwrap_or(()) });
        }

        let mut lines = BufReader::new(stderr);
        let mut diag = String::new();
        let pgid = loop {
            let mut raw = Vec::new();
            let n = lines.read_until(b'\n', &mut raw).await.map_err(|e| ProcError::Io(format!("lost the ssh control channel: {e}")))?;
            if n == 0 {
                let status = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(SSH_FAILED);
                return Err(self.no_start(status, &diag));
            }
            let line = String::from_utf8_lossy(&raw);
            match parse_control(&line) {
                Some(Control::Started(pgid)) => break pgid,
                Some(Control::Failed { what, errno, text }) => {
                    return Err(failed_start(&what, errno, &text, &program, &cwd, &self.host()));
                }
                Some(Control::Exited(_) | Control::Stderr(_)) | None => diag.push_str(&line),
            }
        };
        if pgid <= 1 {
            return Err(ProcError::Io(format!("{} reported process group {pgid}; refusing to track it.", self.host())));
        }

        let (out_tx, _) = watch::channel(0u64);
        let (exit_tx, _) = watch::channel(None);
        let inner = Arc::new(ChildInner {
            identity: ProcessIdentity { pid: pgid, start_time: Timestamp::now() },
            ssh: self.ssh.clone(),
            out: Mutex::new(OutputBuffer::new(CHILD_OUTPUT_CAP)),
            out_tx,
            done: AtomicBool::new(false),
            exit_tx,
            exited: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            stdin: tokio::sync::Mutex::new(if interactive && !transport { stdin.take() } else { None }),
        });

        let (mut error_writer, error_reader) = if transport {
            let (writer, reader) = tokio::io::duplex(64 * 1024);
            (Some(writer), Some(reader))
        } else {
            (None, None)
        };
        let (reader, pipes) = if transport {
            (None, stdin.zip(error_reader).map(|(stdin, stderr)| (stdin, stdout, stderr)))
        } else {
            (Some(tokio::spawn(pump(inner.clone(), stdout))), None)
        };
        let control = inner.clone();
        let watcher = tokio::spawn(async move {
            let mut diag = diag;
            loop {
                let mut raw = Vec::new();
                match lines.read_until(b'\n', &mut raw).await {
                    Ok(0) | Err(_) => return diag,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&raw);
                match parse_control(&line) {
                    Some(Control::Exited(exit)) => control.finish(Ok(exit)),
                    Some(Control::Stderr(bytes)) => {
                        if let Some(writer) = error_writer.as_mut()
                            && writer.write_all(&bytes).await.is_err()
                        {
                            error_writer = None;
                        }
                    }
                    _ if diag.len() < 8192 => diag.push_str(&line),
                    _ => {}
                }
            }
        });

        let sup = inner.clone();
        let host = self.host();
        tokio::spawn(async move {
            let status = child.wait().await;
            let diag = watcher.await.unwrap_or_default();
            if !sup.exited.load(Ordering::SeqCst) {
                let why = match status {
                    Ok(s) => format!("ssh exited with {s}"),
                    Err(e) => format!("ssh could not be waited on: {e}"),
                };
                let detail = diag.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
                sup.finish(Err(ProcError::Io(format!(
                    "the connection to {host} ended before process {pgid} reported its exit ({why}; {detail}); its outcome is unknown. Check the host and rerun."
                ))));
            }
            if let Some(mut reader) = reader
                && tokio::time::timeout(DRAIN_GRACE, &mut reader).await.is_err()
            {
                reader.abort();
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
                    if let Err(e) = t.kill_group(false).await {
                        t.finish(Err(e));
                    }
                }
            });
        }
        Ok((Arc::new(SshChild { inner }), pipes))
    }

    fn no_start(&self, status: i32, diag: &str) -> ProcError {
        if status == SSH_FAILED {
            return ProcError::Io(classify(self.ssh.target(), self.ssh.effective(), diag).to_string());
        }
        let detail = diag.trim();
        if detail.contains("perl") && (detail.contains("not found") || detail.contains("No such file")) {
            return ProcError::Io(format!(
                "{} has no `perl`, which the ssh world uses to run commands in their own process group; nothing ran. Install perl there.",
                self.host()
            ));
        }
        ProcError::Io(format!(
            "the command launcher on {} stopped before starting anything (exit {status}: {detail}); nothing ran.",
            self.host()
        ))
    }
}

fn failed_start(what: &str, errno: i32, text: &str, program: &str, cwd: &str, host: &str) -> ProcError {
    const ENOENT: i32 = 2;
    const EACCES: i32 = 13;
    const ENOTDIR: i32 = 20;
    match (what, errno) {
        ("cwd", ENOENT | ENOTDIR) => ProcError::NotFound(format!("working directory {cwd} on {host}")),
        ("exec", ENOENT) => ProcError::NotFound(program.to_string()),
        (_, EACCES) => ProcError::Denied(format!("{host} refused to run `{program}` in {cwd} ({text}); nothing ran.")),
        _ => ProcError::Io(format!("could not start `{program}` on {host} ({what}: {text}); nothing ran.")),
    }
}

impl Subprocess for SshSubprocess {
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
                return Err(ProcError::Io("ssh did not return protocol pipes; the process was stopped.".into()));
            };
            if let Some(bytes) = initial {
                stdin.write_all(&bytes).await.map_err(|e| ProcError::Io(format!("could not write protocol stdin: {e}")))?;
            }
            Ok(TransportChild { child, stdin: Box::new(stdin), stdout: Box::new(stdout), stderr: Box::new(stderr) })
        })
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
    /// `pid` is the remote leader's pid, which is also its process group id.
    identity: ProcessIdentity,
    ssh: Arc<Ssh>,
    out: Mutex<OutputBuffer>,
    out_tx: watch::Sender<u64>,
    /// Output is final (the ssh session closed, or the drain window passed).
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

    /// First outcome wins; a timeout turns the leader's exit into `ProcError::Timeout`.
    fn finish(&self, r: Result<Exit, ProcError>) {
        if self.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        let r = if self.timed_out.load(Ordering::SeqCst) { Err(ProcError::Timeout) } else { r };
        self.exit_tx.send_replace(Some(r));
    }

    /// Group kill on the remote over the master (TERM, grace, KILL; or KILL at once).
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-kill-group")]
    async fn kill_group(&self, now: bool) -> Result<(), ProcError> {
        let cmd = killer_command(self.identity.pid, now);
        let mut c = self.ssh.command(&cmd);
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let out =
            c.output().await.map_err(|e| ProcError::Io(format!("could not run ssh to kill process group {}: {e}", self.identity.pid)))?;
        match out.status.code() {
            Some(0) => Ok(()),
            code => Err(ProcError::Io(format!(
                "killing process group {} on {} failed (exit {code:?}: {}); it may still be running. Check the host.",
                self.identity.pid,
                self.ssh.target().display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }
}

pub struct SshChild {
    inner: Arc<ChildInner>,
}

impl Drop for SshChild {
    /// A child nobody holds any more cannot be waited on or killed; do not leave it running.
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-drop-cleanup")]
    fn drop(&mut self) {
        if self.inner.exited.load(Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    // Best effort at drop; the launcher also kills the group when its ssh session breaks.
                    inner.kill_group(true).await.unwrap_or(())
                });
            }
            Err(_) => {
                let status = inner
                    .ssh
                    .std_command(&killer_command(inner.identity.pid, true))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                drop(status);
            }
        }
    }
}

impl Child for SshChild {
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
            // Also after the leader exited: backgrounded grandchildren may still hold the group.
            self.inner.kill_group(false).await
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

    #[test]
    fn control_lines_parse_and_everything_else_is_diagnostics() {
        assert_eq!(parse_control("AHO-SSH P 4242\n"), Some(Control::Started(4242)));
        assert_eq!(parse_control("AHO-SSH X 143 15\n"), Some(Control::Exited(Exit { code: 143, signal: Some(15) })));
        assert_eq!(parse_control("AHO-SSH X 3 0"), Some(Control::Exited(Exit { code: 3, signal: None })));
        assert_eq!(
            parse_control("AHO-SSH E exec 2 No such file or directory\n"),
            Some(Control::Failed { what: "exec".into(), errno: 2, text: "No such file or directory".into() })
        );
        assert_eq!(parse_control("mux_client_request_session: session request failed"), None);
        assert_eq!(parse_control("AHO-SSH P notapid"), None);
    }

    #[test]
    fn start_failures_map_to_the_seam_errors() {
        assert_eq!(failed_start("exec", 2, "No such file", "nope", "/w", "h"), ProcError::NotFound("nope".into()));
        assert!(
            matches!(failed_start("cwd", 2, "No such file", "ls", "/w/missing", "h"), ProcError::NotFound(m) if m.contains("/w/missing"))
        );
        assert!(matches!(failed_start("exec", 13, "Permission denied", "./x", "/w", "h"), ProcError::Denied(_)));
        assert!(matches!(failed_start("io", 24, "Too many open files", "ls", "/w", "h"), ProcError::Io(_)));
    }
}
