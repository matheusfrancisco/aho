//! Runs one hook command: `sh -c <command>` in the project, the Claude Code JSON payload on
//! stdin, `CLAUDE_PROJECT_DIR` set (and `${CLAUDE_PROJECT_DIR}` substituted in the text), and
//! `AHO_HOOKS_DISABLED=1` so an `aho` the hook starts runs no hooks of its own.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aho_types::Value;

use crate::settings::HookCommand;

/// Output kept per stream; the rest is read and discarded so the hook never blocks on a full pipe.
pub const MAX_OUTPUT: usize = 1 << 20;
/// Wait for output pipes this long after the hook exited (a backgrounded child may hold them).
const DRAIN: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunResult {
    /// `None` when killed (timeout or signal) or never started.
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub ms: u64,
}

fn reader(mut r: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut kept, mut chunk) = (Vec::new(), [0u8; 8192]);
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

pub fn substitute(command: &str, project: &Path) -> String {
    command.replace("${CLAUDE_PROJECT_DIR}", &project.display().to_string())
}

#[allow(clippy::disallowed_methods, reason = "authority: hooks-claude-code-command")]
pub fn run_command(cmd: &HookCommand, project: &Path, payload: &Value) -> RunResult {
    let started = Instant::now();
    let elapsed = |s: Instant| s.elapsed().as_millis() as u64;
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(substitute(&cmd.command, project))
        .current_dir(project)
        .env("CLAUDE_PROJECT_DIR", project)
        .env("AHO_HOOKS_DISABLED", "1")
        .env_remove("AHO_READY_FD") // the daemon's readiness fd (daemon::ready::READY_FD_ENV) is never a child's to write
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunResult { spawn_error: Some(e.to_string()), ms: elapsed(started), ..RunResult::default() },
    };
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = payload.to_string().into_bytes();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let out = child.stdout.take().map(reader);
    let err = child.stderr.take().map(reader);
    let deadline = started + cmd.timeout;
    let (exit, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                return RunResult { spawn_error: Some(e.to_string()), ms: elapsed(started), ..RunResult::default() };
            }
        }
    };
    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|rx| rx.recv_timeout(DRAIN).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    };
    RunResult { exit, stdout: collect(out), stderr: collect(err), timed_out, spawn_error: None, ms: elapsed(started) }
}
