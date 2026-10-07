//! `bash` and `bash_persistent`. Class `SpawnsProcess`; the content refinement is
//! the blast-radius classifier (`aho_policy::shell::assess`) wired as `ToolDecl.check`.
//!
//! Output is streamed line by line to `ExecRequest.progress` and into the call's operation
//! (so `job_output` / `job_wait` and `AHO_PROGRESS` markers work while it runs); the child is
//! attached to the operation, so `job_kill` and cancellation kill its whole process group.
//!
//! Pwsh parity: `caps.tool-bash.shell = pwsh` runs both tools in PowerShell 7 (`pwsh`
//! module). The classifier reads only POSIX shells, so under pwsh every command asks.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod pwsh;

use std::sync::Arc;
use std::time::Duration;

use aho_cap::tokio_like::Sender;
use aho_cap::*;
use aho_log::OpKind;
use aho_policy::shell::{RiskLevel, assess, justification_ok, reflection_prompt};
use aho_proc::{Child, OpTable, OperationsHandle, OutputBuffer, ProcError, SpawnSpec, SubprocessHandle};
use aho_types::*;
use serde_json::json;

pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Result output kept: this much from the start plus this much from the end.
pub const KEEP_HALF: usize = 512 * 1024;
/// A line longer than this is streamed in pieces.
const MAX_LINE: usize = 64 * 1024;
/// Interval at which a running command re-checks cancellation and its deadline.
const TICK: Duration = Duration::from_millis(50);

pub use pwsh::{BashConfig, PWSH_TOOLS, SHELL_KEY, ShellKind, check_pwsh};

pub struct BashDeps {
    pub proc: SubprocessHandle,
    /// Seeded by the session; without it output still streams to `progress`.
    pub ops: Option<OperationsHandle>,
}

impl Deps for BashDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::Operations];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(BashDeps { proc: r.handle()?, ops: r.handle::<OperationsHandle>().ok() })
    }
}

#[derive(Clone)]
pub struct ToolBash {
    proc: SubprocessHandle,
    ops: Option<OpTable>,
    kind: ShellKind,
    /// The one long-lived shell of `bash_persistent` for this capability instance.
    shell: Arc<tokio::sync::Mutex<Option<Shell>>>,
}

impl Capability for ToolBash {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = BashDeps;
    type Config = BashConfig;

    fn build(deps: BashDeps, cfg: BashConfig) -> Result<Self, BuildError> {
        Ok(ToolBash { proc: deps.proc, ops: deps.ops.map(|o| o.0), kind: cfg.shell, shell: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        // Same names as the manifest's `TOOLS`; under pwsh the PowerShell descriptions and check.
        let decls = match self.kind {
            ShellKind::Posix => TOOLS,
            ShellKind::Pwsh => PWSH_TOOLS,
        };
        for decl in decls {
            let me = self.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move { me.run(name, &req).await })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolBash,
    "tool-bash",
    kind = Kind::Tool,
    provides = [],
    requires = [
        Requirement::Seam(aho_proc::SEAM),
        Requirement::AnyOf(&[Requirement::Setting { key: SHELL_KEY, pred: Pred::Eq("bash") }, Requirement::Binary("pwsh")]),
    ],
    tools = TOOLS,
    doc = "Shell commands in the bound world, with the blast-radius classifier as the tool's own check. `caps.tool-bash.shell` selects the shell: `bash` (POSIX /bin/sh, the default) or `pwsh` (PowerShell 7; Locked until `pwsh` is on PATH). The classifier understands POSIX shells only, so under pwsh every command is held at the most conservative level: Catastrophic matches are denied and every other command asks the operator (a justification does not skip the ask)."
);

/// `bash` may run as a session job (`background: true`, `jobs.detach.bash`); `bash_persistent`
/// never does: its calls share one shell behind a lock, so a backgrounded command would hold the
/// shell and every later `bash_persistent` call would wait on the job, with its `cd`/`export`
/// landing at an unpredictable point in the sequence.
pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "bash",
        "Run a shell command (`/bin/sh -c`) in the workspace. Returns combined stdout+stderr and the exit code. Each call is a fresh shell.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_bash)
    .check(check_command)
    .op(OpKind::Process, false, true)
    .detachable()
    .present(PRESENT),
    ToolDecl::new(
        "bash_persistent",
        "Run a command in one long-lived shell: `cd`, exported variables and shell state persist between calls. `exit` ends the shell; the next call starts a fresh one.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_bash)
    .check(check_command)
    .op(OpKind::Process, false, true)
    .present(PRESENT),
];

fn schema_bash(w: &BoundWorld) -> RootSchema {
    let mut props = json!({
        "command": {"type": "string", "description": "The command line."},
        "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS,
                       "description": "Kill the command (its whole process group) after this long."},
        "justification": {"type": "string",
                          "description": "Why an irreversible command is what the user asked for (≥ 25 chars, not a bare affirmation)."}
    });
    // Escalation-at-denial exists only where the bound world confines.
    if w.confines {
        props["escalate"] = json!({
            "type": "array",
            "items": {"type": "string", "enum": ["workspace_write", "host_write", "network_egress", "spawns_process"]},
            "description": "Retry a sandbox-denied call once with these strictly wider classes (needs approval and a justification)."
        });
    }
    RootSchema(json!({"type": "object", "properties": props, "required": ["command"], "additionalProperties": false}))
}

/// Decision-chain step 3 for shell: `Catastrophic` → Deny (never lifted), `Confirm` → Ask with the
/// reflection prompt unless the call carries an acceptable `justification`.
pub fn check_command(args: &Value, view: &PolicyView) -> Option<Decision> {
    let command = args.get("command").and_then(Value::as_str)?;
    let risk = assess(command, &view.workspace, &view.home);
    match risk.level {
        RiskLevel::Catastrophic => Some(Decision::Deny(reflection_prompt(&risk))),
        RiskLevel::Confirm => {
            let j = args.get("justification").and_then(Value::as_str).unwrap_or("");
            if justification_ok(j) { None } else { Some(Decision::Ask(reflection_prompt(&risk))) }
        }
        RiskLevel::Safe | RiskLevel::Low => None,
    }
}

const PRESENT: Present = Present { call: present_call, result: present_result };

fn present_call(args: &Value) -> Card {
    Card::Terminal { command: args.get("command").and_then(Value::as_str).unwrap_or("").to_string(), output: String::new(), exit: None }
}

fn present_result(args: &Value, v: &Value) -> Card {
    Card::Terminal {
        command: v.get("command").or_else(|| args.get("command")).and_then(Value::as_str).unwrap_or("").to_string(),
        output: v.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
        exit: v.get("exit").and_then(Value::as_i64).map(|c| c as i32),
    }
}

// ───────────────────────────── output handling ─────────────────────────────

/// Keeps the first and last `KEEP_HALF` bytes of an unbounded stream.
struct HeadTail {
    head: Vec<u8>,
    tail: OutputBuffer,
}

impl HeadTail {
    fn new() -> HeadTail {
        HeadTail { head: Vec::new(), tail: OutputBuffer::new(KEEP_HALF) }
    }
    fn push(&mut self, bytes: &[u8]) {
        let take = (KEEP_HALF - self.head.len().min(KEEP_HALF)).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        self.tail.push(&bytes[take..]);
    }
    fn text(&self) -> String {
        let tail = self.tail.read(0);
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if tail.dropped > 0 {
            s.push_str(&format!("\n[… {} bytes omitted …]\n", tail.dropped));
        }
        s.push_str(&String::from_utf8_lossy(&tail.bytes));
        s
    }
}

/// Splits output into lines for `progress` and the op; in persistent mode also finds the
/// sentinel line and holds back the one blank line the sentinel printf adds.
struct Lines<'a> {
    progress: Option<&'a Sender>,
    op: Option<(&'a OpTable, OpId)>,
    partial: Vec<u8>,
    out: HeadTail,
    sentinel: Option<String>,
    held_blank: bool,
    status: Option<i32>,
    /// pwsh's stdin-script mode (`-NonInteractive -Command -`) brackets every read with the
    /// DECCKM toggles `ESC[?1h`/`ESC[?1l`, attached to real output or standalone; through a
    /// pipe they are console-keypad noise, so they are stripped before line processing.
    strip_keypad_toggles: bool,
}

impl<'a> Lines<'a> {
    fn new(progress: Option<&'a Sender>, op: Option<(&'a OpTable, OpId)>, sentinel: Option<String>) -> Lines<'a> {
        Lines {
            progress,
            op,
            partial: Vec::new(),
            out: HeadTail::new(),
            sentinel,
            held_blank: false,
            status: None,
            strip_keypad_toggles: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let toggles = self.strip_keypad_toggles;
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            let cleaned;
            let chunk: &[u8] = if toggles {
                cleaned = Self::strip_toggle(chunk);
                &cleaned
            } else {
                chunk
            };
            if let Some(line) = chunk.strip_suffix(b"\n") {
                self.partial.extend_from_slice(line);
                let line = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
                self.line(&line, true);
            } else {
                self.partial.extend_from_slice(chunk);
                if self.partial.len() > MAX_LINE {
                    let piece = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
                    self.emit(&piece, false);
                }
            }
        }
    }

    /// Drops DECCKM toggles (`ESC[?1h`/`ESC[?1l`), wherever they sit in the chunk, and returns
    /// the chunk back when it holds none (the common case, no allocation).
    fn strip_toggle(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
        const H: &[u8] = b"\x1b[?1h";
        const L: &[u8] = b"\x1b[?1l";
        let at = |i: usize| -> bool { i + 5 <= bytes.len() && (&bytes[i..i + 5] == H || &bytes[i..i + 5] == L) };
        let Some(first) = (0..bytes.len().saturating_sub(4)).find(|&i| at(i)) else { return std::borrow::Cow::Borrowed(bytes) };
        let mut clean = Vec::with_capacity(bytes.len());
        clean.extend_from_slice(&bytes[..first]);
        let mut i = first;
        while i < bytes.len() {
            if at(i) {
                i += 5;
            } else {
                clean.push(bytes[i]);
                i += 1;
            }
        }
        std::borrow::Cow::Owned(clean)
    }

    fn line(&mut self, line: &str, newline: bool) {
        if self.status.is_some() {
            return;
        }
        if let Some(s) = &self.sentinel {
            if let Some(rest) = line.strip_prefix(s.as_str()) {
                self.status = Some(rest.trim().parse().unwrap_or(-1));
                self.held_blank = false;
                return;
            }
            if line.is_empty() {
                if self.held_blank {
                    self.emit("", true);
                }
                self.held_blank = true;
                return;
            }
            if self.held_blank {
                self.held_blank = false;
                self.emit("", true);
            }
        }
        self.emit(line, newline);
    }

    fn emit(&mut self, line: &str, newline: bool) {
        self.out.push(line.as_bytes());
        if newline {
            self.out.push(b"\n");
        }
        if let Some(p) = self.progress {
            p.send(line.to_string());
        }
        if let Some((t, op)) = self.op {
            t.progress(op, line.to_string());
        }
    }

    /// End of stream: flush a trailing partial line (and a held blank line outside a sentinel).
    fn finish(&mut self) {
        if !self.partial.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
            self.line(&line, false);
        }
        if self.held_blank && self.status.is_none() {
            self.held_blank = false;
            self.emit("", true);
        }
    }
}

enum Ended {
    /// Output complete (process ended, or the sentinel arrived).
    Complete,
    Cancelled,
    TimedOut,
}

/// Pump `child` output into `lines` until it is done (or the sentinel arrived), the call is
/// cancelled, or `deadline` passes. Returns the next cursor. A cancel is checked first: cancelling
/// an op (`job_kill`, an abort) kills its process group too, and the exit that kill causes must
/// still read as a cancel, not as the command's own end.
async fn pump(
    child: &dyn Child,
    mut cursor: u64,
    lines: &mut Lines<'_>,
    cancel: &CancelToken,
    deadline: tokio::time::Instant,
) -> (Ended, u64) {
    loop {
        let (bytes, next, done) = child.output(cursor);
        cursor = next;
        lines.feed(&bytes);
        if cancel.is_cancelled() {
            return (Ended::Cancelled, cursor);
        }
        if lines.status.is_some() {
            return (Ended::Complete, cursor);
        }
        if done {
            lines.finish();
            return (Ended::Complete, cursor);
        }
        if tokio::time::Instant::now() >= deadline {
            return (Ended::TimedOut, cursor);
        }
        tokio::select! {
            _ = child.readable(cursor) => {}
            _ = tokio::time::sleep(TICK) => {}
        }
    }
}

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn proc_err(e: ProcError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

struct Args<'a> {
    command: &'a str,
    timeout_ms: u64,
}

fn parse(args: &Value) -> Result<Args<'_>, ExecOutcome> {
    let command =
        args.get("command").and_then(Value::as_str).ok_or_else(|| bad_args("`command` is required and must be a string; nothing ran."))?;
    if command.trim().is_empty() {
        return Err(bad_args("`command` is empty; nothing ran."));
    }
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| bad_args("`timeout_ms` must be a positive integer; nothing ran."))?
            .min(MAX_TIMEOUT_MS),
    };
    Ok(Args { command, timeout_ms })
}

/// The model-facing result for a finished command.
fn result(
    command: &str,
    output: String,
    exit: Option<i32>,
    signal: Option<i32>,
    timeout_ms: Option<u64>,
    note: Option<String>,
) -> ExecOutcome {
    let mut text = if output.is_empty() { "(no output)".to_string() } else { output.clone() };
    let status = match (exit, signal, timeout_ms) {
        (_, _, Some(ms)) => Some(format!("[killed after {ms} ms: the time limit was reached; raise timeout_ms to allow longer]")),
        (_, Some(sig), None) => Some(format!("[killed by signal {sig}]")),
        (Some(0), None, None) => None,
        (Some(c), None, None) => Some(format!("[exit {c}]")),
        (None, None, None) => None,
    };
    for extra in status.into_iter().chain(note.clone()) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&extra);
    }
    let value = json!({
        "command": command, "output": output, "exit": exit, "signal": signal,
        "timed_out": timeout_ms.is_some(), "note": note,
    });
    let meta = Some(Card::Terminal { command: command.to_string(), output, exit });
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta }
}

// ───────────────────────────── the persistent shell ─────────────────────────────

struct Shell {
    child: Arc<dyn Child>,
    cursor: u64,
    nonce: String,
    seq: u64,
}

impl Shell {
    /// Script for one call. The command travels as one single-quoted word (no expansion; `'`
    /// written as `'\''`), is syntax-checked in a throwaway `sh -n` (a syntax error must not end
    /// the shell), then `eval`ed in this shell so `cd`/`export` persist, with stdin from
    /// /dev/null so it cannot eat the protocol. The sentinel line carries the exit status.
    fn script(command: &str, sentinel: &str) -> String {
        let quoted = command.replace('\'', "'\\''");
        format!(
            "__aho_cmd='{quoted}'\n\
             if /bin/sh -n -c \"$__aho_cmd\" 2>/dev/null; then eval \"$__aho_cmd\" </dev/null; __aho_rc=$?; \
             else /bin/sh -n -c \"$__aho_cmd\"; __aho_rc=2; fi\n\
             printf '\\n%s %d\\n' '{sentinel}' \"$__aho_rc\"\n"
        )
    }
}

// ───────────────────────────── execution ─────────────────────────────

impl ToolBash {
    pub async fn run(&self, tool: &str, req: &ExecRequest) -> ExecOutcome {
        match tool {
            "bash" => self.bash(req).await,
            "bash_persistent" => self.persistent(req).await,
            other => bad_args(format!("tool-bash has no tool `{other}`.")),
        }
    }

    fn op_of(&self, call: &CallId) -> Option<(&OpTable, OpId)> {
        let t = self.ops.as_ref()?;
        t.op_for_call(call).map(|op| (t, op))
    }

    async fn bash(&self, req: &ExecRequest) -> ExecOutcome {
        let a = match parse(&req.view.args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let argv = match self.kind {
            ShellKind::Posix => vec!["/bin/sh".into(), "-c".into(), a.command.into()],
            ShellKind::Pwsh => pwsh::oneshot(a.command),
        };
        let spec = SpawnSpec { argv, timeout_ms: Some(a.timeout_ms), ..SpawnSpec::default() };
        let child = match self.proc.0.spawn(spec).await {
            Ok(c) => c,
            Err(e) => return proc_err(e),
        };
        let op = self.op_of(&req.view.call);
        if let Some((t, op)) = op {
            t.attach_child(op, child.clone());
        }
        let mut lines = Lines::new(req.progress.as_ref(), op, None);
        lines.strip_keypad_toggles = self.kind == ShellKind::Pwsh;
        // The world enforces the timeout; the local deadline is a backstop with a little slack.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(a.timeout_ms) + Duration::from_secs(5);
        let (ended, _) = pump(child.as_ref(), 0, &mut lines, &req.cancel, deadline).await;
        match ended {
            Ended::Cancelled => {
                let _ = child.kill().await;
                return ExecOutcome::Cancelled;
            }
            Ended::TimedOut => {
                let _ = child.kill().await;
            }
            Ended::Complete => {}
        }
        match child.wait().await {
            Ok(exit) if matches!(ended, Ended::TimedOut) => {
                result(a.command, lines.out.text(), Some(exit.code), exit.signal, Some(a.timeout_ms), None)
            }
            Ok(exit) => result(a.command, lines.out.text(), Some(exit.code), exit.signal, None, None),
            Err(ProcError::Timeout) => result(a.command, lines.out.text(), None, None, Some(a.timeout_ms), None),
            Err(e) => proc_err(e),
        }
    }

    async fn persistent(&self, req: &ExecRequest) -> ExecOutcome {
        let a = match parse(&req.view.args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let mut guard = self.shell.lock().await;
        if guard.as_ref().is_none_or(|s| s.child.output(s.cursor).2) {
            let argv = match self.kind {
                ShellKind::Posix => vec!["/bin/sh".to_string()],
                ShellKind::Pwsh => pwsh::persistent(),
            };
            let child = match self.proc.0.spawn_interactive(SpawnSpec { argv, ..SpawnSpec::default() }).await {
                Ok(c) => c,
                Err(e) => return proc_err(e),
            };
            let id = child.identity();
            let nonce = Hash::of(format!("{}:{}:{}", id.pid, id.start_time.0, a.command.len()).as_bytes()).to_hex()[..12].to_string();
            *guard = Some(Shell { child, cursor: 0, nonce, seq: 0 });
        }
        let Some(shell) = guard.as_mut() else { return ExecOutcome::err("PROC_IO", "the persistent shell could not be started.") };
        shell.seq += 1;
        let sentinel = format!("__AHO_DONE_{}_{}", shell.nonce, shell.seq);
        let script = match self.kind {
            ShellKind::Posix => Shell::script(a.command, &sentinel),
            ShellKind::Pwsh => pwsh::script(a.command, &sentinel),
        };
        let op = self.op_of(&req.view.call);
        if let Some((t, op)) = op {
            t.attach_child(op, shell.child.clone());
        }
        if let Err(e) = shell.child.write_stdin(script.into_bytes()).await {
            let _ = shell.child.kill().await;
            *guard = None;
            return proc_err(e);
        }
        let mut lines = Lines::new(req.progress.as_ref(), op, Some(sentinel));
        lines.strip_keypad_toggles = self.kind == ShellKind::Pwsh;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(a.timeout_ms);
        let (ended, cursor) = pump(shell.child.as_ref(), shell.cursor, &mut lines, &req.cancel, deadline).await;
        shell.cursor = cursor;
        match ended {
            Ended::Cancelled => {
                let _ = shell.child.kill().await;
                *guard = None;
                ExecOutcome::Cancelled
            }
            Ended::TimedOut => {
                let _ = shell.child.kill().await;
                *guard = None;
                let note = "[the shell was killed; the next call starts a fresh one (cwd and environment reset)]".to_string();
                result(a.command, lines.out.text(), None, None, Some(a.timeout_ms), Some(note))
            }
            Ended::Complete => match lines.status {
                Some(code) => result(a.command, lines.out.text(), Some(code), None, None, None),
                None => {
                    // The shell itself ended (`exit`, or killed): report its status and start over next time.
                    let exit = shell.child.wait().await.ok();
                    *guard = None;
                    let note = "[the shell exited; the next call starts a fresh one (cwd and environment reset)]".to_string();
                    result(a.command, lines.out.text(), exit.map(|e| e.code), exit.and_then(|e| e.signal), None, Some(note))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
