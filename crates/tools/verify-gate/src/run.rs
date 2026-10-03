//! One check run in the bound world: spawned through the world's `Subprocess`, bounded by the
//! configured timeout (enforced by the world, with a local backstop), output reduced to its tail.

use std::sync::Arc;
use std::time::Duration;

use aho_proc::{Child, ProcError, SpawnSpec, Subprocess};

use crate::detect::Check;

/// Bytes of output kept for the reason (the end of the output, where failures are reported).
pub const TAIL_BYTES: usize = 4096;
/// How long output may keep draining after the process ended (a stray background child can hold
/// the pipe open).
const DRAIN: Duration = Duration::from_secs(2);
/// Past the world's own deadline, the local one fires and kills the process group.
const BACKSTOP: Duration = Duration::from_secs(5);
/// Upper bound on everything around the command itself; the hook's timeout is the command's plus this.
pub const OVERHEAD: Duration = Duration::from_secs(30);

const REASON: &str = include_str!("../prompts/failed.md");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `128 + signal` when killed by a signal.
    Exited(i32),
    TimedOut(Duration),
    /// The command did not run to an exit status (could not start, or its status was lost).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    pub outcome: Outcome,
    /// The last `TAIL_BYTES` of output (stdout and stderr interleaved), lossily decoded.
    pub tail: String,
    /// Bytes of output before the tail that are not in it.
    pub cut: u64,
}

impl Ran {
    pub fn passed(&self, check: &Check) -> bool {
        matches!(self.outcome, Outcome::Exited(c) if check.passes(c))
    }

    /// `verify: <command> <outcome>\n<tail>` (template in `prompts/failed.md`).
    pub fn reason(&self, check: &Check) -> String {
        let outcome = match &self.outcome {
            Outcome::Exited(c) => format!("exited {c}"),
            Outcome::TimedOut(d) => format!("timed out after {}s", d.as_secs()),
            Outcome::Failed(e) => format!("did not run: {e}"),
        };
        let tail = if self.cut > 0 { format!("[{} earlier bytes cut]\n{}", self.cut, self.tail) } else { self.tail.clone() };
        render(REASON.trim_end(), &[("{command}", &check.command), ("{outcome}", &outcome), ("{tail}", &tail)])
    }
}

/// Single pass, so a placeholder inside a substituted value is never expanded.
fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + vars.iter().map(|(_, v)| v.len()).sum::<usize>());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match vars.iter().find(|(k, _)| rest.starts_with(k)) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &rest[k.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

pub async fn run(proc: &dyn Subprocess, check: &Check, timeout: Duration) -> Ran {
    let spec = SpawnSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), check.command.clone()],
        timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        ..SpawnSpec::default()
    };
    let child = match proc.spawn(spec).await {
        Ok(c) => c,
        Err(e) => return Ran { outcome: Outcome::Failed(e.to_string()), tail: String::new(), cut: 0 },
    };
    let outcome = match tokio::time::timeout(timeout + BACKSTOP, child.wait()).await {
        Ok(Ok(exit)) => Outcome::Exited(exit.code),
        Ok(Err(ProcError::Timeout)) => Outcome::TimedOut(timeout),
        Ok(Err(e)) => Outcome::Failed(e.to_string()),
        Err(_) => match child.kill().await {
            Ok(()) => Outcome::TimedOut(timeout),
            Err(e) => Outcome::Failed(format!("timed out after {}s and could not be killed: {e}", timeout.as_secs())),
        },
    };
    let (tail, cut) = drain(&child).await;
    Ran { outcome, tail, cut }
}

/// Reads the child's output to its end (or `DRAIN`), keeping only the last `TAIL_BYTES`.
async fn drain(child: &Arc<dyn Child>) -> (String, u64) {
    let deadline = tokio::time::Instant::now() + DRAIN;
    let mut kept: Vec<u8> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let (bytes, next, done) = child.output(cursor);
        kept.extend_from_slice(&bytes);
        if kept.len() > TAIL_BYTES {
            kept.drain(..kept.len() - TAIL_BYTES);
        }
        cursor = next;
        if done || tokio::time::timeout_at(deadline, child.readable(cursor)).await.is_err() {
            break;
        }
    }
    let cut = cursor.saturating_sub(kept.len() as u64);
    (String::from_utf8_lossy(&kept).trim_end().to_string(), cut)
}
