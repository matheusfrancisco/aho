//! `00`/`07` — PTY terminals as tools: `terminal_open`, `terminal_send`, `terminal_read`,
//! `terminal_resize`, `terminal_close`, `terminal_list`, over the bound world's `Terminals` seam
//! (`aho-term`; `pty-host` under `world = host`).
//!
//! Terminals are named `t1`, `t2`, … per session. Each keeps a transcript cursor, so every
//! `terminal_read` returns the screen as it is now plus exactly the output that arrived since the
//! previous read. `terminal_close` kills the terminal's whole process tree.
//!
//! Classes: opening, typing into, resizing and closing a terminal are `SpawnsProcess`; reading and
//! listing are `ReadOnly`. The blast-radius classifier (`aho_policy::shell`) runs on the command a
//! terminal opens with (the core's step 5 does that for every `command` argument) and, as this
//! capability's own check, on every line `terminal_send` types. It only understands POSIX shells,
//! and it sees one call's keystrokes: a command typed across several calls is checked in pieces.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod keys;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aho_cap::*;
use aho_log::OpKind;
use aho_policy::shell::{RiskLevel, assess, justification_ok, reflection_prompt};
use aho_term::{OpenSpec, Size, Snapshot, TermError, Terminal, TerminalsHandle};
use aho_types::*;
use serde_json::json;

/// Program a terminal runs when `command` is omitted.
pub const DEFAULT_COMMAND: &str = "/bin/sh";
/// Open terminals per session.
pub const MAX_OPEN: usize = 16;
pub const DEFAULT_READ_TIMEOUT_MS: u64 = 2_000;
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_IDLE_MS: u64 = 300;
/// How long `terminal_open` lets the program print its first screen (prompt, banner).
const OPEN_SETTLE: Duration = Duration::from_millis(250);
const OPEN_SETTLE_MAX: Duration = Duration::from_secs(2);

pub struct TerminalDeps {
    pub terminals: TerminalsHandle,
}

impl Deps for TerminalDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Terminals];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(TerminalDeps { terminals: r.handle()? })
    }
}

struct Slot {
    term: Arc<dyn Terminal>,
    command: String,
    cursor: u64,
}

#[derive(Default)]
struct Table {
    next: u64,
    open: BTreeMap<String, Slot>,
}

#[derive(Clone)]
pub struct ToolTerminal {
    terminals: TerminalsHandle,
    table: Arc<Mutex<Table>>,
}

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Capability for ToolTerminal {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = TerminalDeps;
    type Config = ();

    fn build(deps: TerminalDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolTerminal { terminals: deps.terminals, table: Arc::default() })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
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
    ToolTerminal,
    "tool-terminal",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_term::SEAM)],
    tools = TOOLS,
    doc = "PTY terminals in the bound world: open a program on a terminal, type keys (control sequences included), read the rendered screen plus the output since the last read (optionally waiting for a regex or for output to go idle), resize, list, and close (kills the whole process tree). Open/send/resize/close are SpawnsProcess; read/list are ReadOnly. Typed lines go through the POSIX shell blast-radius classifier as the tool's own check; it does not parse other shells, and it sees one call's keystrokes at a time."
);

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "terminal_open",
        "Open a program on a new pseudo-terminal (default `/bin/sh`) in the workspace and return its id (`t1`, …) and first screen. Use terminals for interactive programs (REPLs, TUIs, prompts); use `bash` for one-shot commands.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_open)
    .op(OpKind::Process, false, true)
    .present(PRESENT),
    ToolDecl::new(
        "terminal_send",
        "Type into a terminal: `text` is sent verbatim (JSON escapes like \\n or \\u0003 are control bytes), then each of `keys` (Enter, Tab, Esc, Backspace, Up/Down/Left/Right, Home, End, PageUp, PageDown, Delete, F1–F12, C-c, M-x, …). Returns immediately; read the result with `terminal_read`.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_send)
    .check(check_send)
    .op(OpKind::Process, false, true),
    ToolDecl::new(
        "terminal_read",
        "Read a terminal: the screen as it is now plus the output that arrived since your last read. Waits until `wait_for` (a regex) matches the new output or the screen, or else until output has been idle for `idle_ms`, at most `timeout_ms`.",
        Consequence::ReadOnly,
    )
    .schema(schema_read)
    .cancel_on_interrupt()
    .present(PRESENT),
    ToolDecl::new(
        "terminal_resize",
        "Resize a terminal (columns × rows); the program receives SIGWINCH.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_resize)
    .op(OpKind::Process, false, true),
    ToolDecl::new(
        "terminal_close",
        "Close a terminal and kill its whole process tree (background jobs included). Returns the pids that were signalled and the exit status.",
        Consequence::SpawnsProcess,
    )
    .schema(schema_id)
    .op(OpKind::Process, false, true),
    ToolDecl::new("terminal_list", "List this session's open terminals: id, command, pid, size, and whether the program still runs.", Consequence::ReadOnly)
        .safe(),
];

// ───────────────────────────── schemas ─────────────────────────────

fn size_props() -> Value {
    json!({
        "cols": {"type": "integer", "minimum": Size::MIN_COLS, "maximum": Size::MAX_COLS, "default": Size::DEFAULT.cols},
        "rows": {"type": "integer", "minimum": Size::MIN_ROWS, "maximum": Size::MAX_ROWS, "default": Size::DEFAULT.rows}
    })
}

fn id_prop() -> Value {
    json!({"type": "string", "description": "Terminal id from terminal_open (`t1`, …)."})
}

fn schema_open(_: &BoundWorld) -> RootSchema {
    let mut props = size_props();
    props["command"] = json!({"type": "string", "description": "Command line run by `/bin/sh -c` on the terminal; default `/bin/sh` (an interactive shell)."});
    props["cwd"] = json!({"type": "string", "description": "Working directory, relative to the workspace root."});
    RootSchema(json!({"type": "object", "properties": props, "additionalProperties": false}))
}

fn schema_send(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": id_prop(),
            "text": {"type": "string", "description": "Sent verbatim before `keys`."},
            "keys": {"type": "array", "items": {"type": "string"}, "description": "Named keys sent after `text`, in order (e.g. [\"C-c\"], [\"Up\", \"Enter\"])."},
            "justification": {"type": "string", "description": "Why an irreversible command typed here is what the user asked for (≥ 25 chars)."}
        },
        "required": ["id"],
        "additionalProperties": false
    }))
}

fn schema_read(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": id_prop(),
            "wait_for": {"type": "string", "description": "Regex (Rust syntax, multi-line mode) to wait for in the new output or on the screen."},
            "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_TIMEOUT_MS,
                           "description": "Longest wait; default 10000 with wait_for, else 2000."},
            "idle_ms": {"type": "integer", "minimum": 0, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_IDLE_MS,
                        "description": "Without wait_for: return once no output arrived for this long (0 = return at once)."}
        },
        "required": ["id"],
        "additionalProperties": false
    }))
}

fn schema_resize(_: &BoundWorld) -> RootSchema {
    let mut props = size_props();
    props["id"] = id_prop();
    RootSchema(json!({"type": "object", "properties": props, "required": ["id", "cols", "rows"], "additionalProperties": false}))
}

fn schema_id(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {"id": id_prop()}, "required": ["id"], "additionalProperties": false}))
}

// ───────────────────────────── check ─────────────────────────────

/// `04` step 3 for typed input: every line `terminal_send` would type goes through the shell
/// classifier. `Catastrophic` → Deny; `Confirm` → Ask unless a real `justification` is given.
pub fn check_send(args: &Value, view: &PolicyView) -> Option<Decision> {
    let typed = typed_text(args);
    let mut ask = None;
    for line in typed.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()) {
        let risk = assess(line, &view.workspace, &view.home);
        match risk.level {
            RiskLevel::Catastrophic => return Some(Decision::Deny(reflection_prompt(&risk))),
            RiskLevel::Confirm if ask.is_none() => ask = Some(reflection_prompt(&risk)),
            _ => {}
        }
    }
    let justified = args.get("justification").and_then(Value::as_str).is_some_and(justification_ok);
    ask.filter(|_| !justified).map(Decision::Ask)
}

/// The printable reading of a send: `text`, with `Enter` keys as line breaks.
fn typed_text(args: &Value) -> String {
    let mut s = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    for k in args.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if keys::is_submit(k) {
            s.push('\n');
        }
    }
    s
}

// ───────────────────────────── presentation ─────────────────────────────

const PRESENT: Present = Present { call: present_call, result: present_result };

fn present_call(args: &Value) -> Card {
    let command = args.get("command").or_else(|| args.get("id")).and_then(Value::as_str).unwrap_or("").to_string();
    Card::Terminal { command, output: String::new(), exit: None }
}

fn present_result(args: &Value, v: &Value) -> Card {
    Card::Terminal {
        command: v.get("command").or_else(|| args.get("id")).and_then(Value::as_str).unwrap_or("").to_string(),
        output: v.get("screen").and_then(Value::as_str).unwrap_or("").to_string(),
        exit: v.get("exit").and_then(Value::as_i64).map(|c| c as i32),
    }
}

// ───────────────────────────── execution ─────────────────────────────

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn term_err(e: TermError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

fn u64_arg(args: &Value, key: &str, default: u64, max: u64) -> Result<u64, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            v.as_u64().map(|n| n.min(max)).ok_or_else(|| bad_args(format!("`{key}` must be a non-negative integer; nothing was done.")))
        }
    }
}

fn size_arg(args: &Value, default: Size) -> Result<Size, ExecOutcome> {
    let dim = |key: &str, d: u16| -> Result<u16, ExecOutcome> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(d),
            Some(v) => v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| bad_args(format!("`{key}` must be a positive integer; nothing was done."))),
        }
    };
    Size::new(dim("cols", default.cols)?, dim("rows", default.rows)?).map_err(term_err)
}

/// Why a read returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Matched,
    Idle,
    Exited,
    TimedOut,
}

impl ToolTerminal {
    pub async fn run(&self, tool: &str, req: &ExecRequest) -> ExecOutcome {
        let args = &req.view.args;
        match tool {
            "terminal_open" => self.open(args).await,
            "terminal_send" => self.send(args).await,
            "terminal_read" => self.read(args, &req.cancel).await,
            "terminal_resize" => self.resize(args),
            "terminal_close" => self.close(args).await,
            "terminal_list" => self.list(),
            other => bad_args(format!("tool-terminal has no tool `{other}`.")),
        }
    }

    fn slot(&self, args: &Value) -> Result<(String, Arc<dyn Terminal>, u64), ExecOutcome> {
        let id =
            args.get("id").and_then(Value::as_str).ok_or_else(|| bad_args("`id` is required (from terminal_open); nothing was done."))?;
        let t = lk(&self.table);
        match t.open.get(id) {
            Some(s) => Ok((id.to_string(), s.term.clone(), s.cursor)),
            None => {
                let known: Vec<&str> = t.open.keys().map(String::as_str).collect();
                let listing = if known.is_empty() { "none are open".to_string() } else { format!("open: {}", known.join(", ")) };
                Err(ExecOutcome::err(
                    "TERM_UNKNOWN",
                    format!("there is no open terminal `{id}` ({listing}); nothing was done. Open one with terminal_open."),
                ))
            }
        }
    }

    async fn open(&self, args: &Value) -> ExecOutcome {
        let size = match size_arg(args, Size::DEFAULT) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let command = match args.get("command") {
            None | Some(Value::Null) => DEFAULT_COMMAND.to_string(),
            Some(Value::String(c)) if !c.trim().is_empty() => c.clone(),
            Some(_) => return bad_args("`command` must be a non-empty string; no terminal was opened."),
        };
        let cwd = match args.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(c)) => Some(c.clone()),
            Some(_) => return bad_args("`cwd` must be a string; no terminal was opened."),
        };
        if lk(&self.table).open.len() >= MAX_OPEN {
            return ExecOutcome::err(
                "TERM_LIMIT",
                format!(
                    "{MAX_OPEN} terminals are already open in this session; none was opened. Close one with terminal_close (see terminal_list)."
                ),
            );
        }
        let argv = if command == DEFAULT_COMMAND { vec![command.clone()] } else { vec!["/bin/sh".into(), "-c".into(), command.clone()] };
        let term = match self.terminals.0.open(OpenSpec { argv, cwd, env: vec![], size }).await {
            Ok(t) => t,
            Err(e) => return term_err(e),
        };
        let id = {
            let mut t = lk(&self.table);
            t.next += 1;
            let id = format!("t{}", t.next);
            t.open.insert(id.clone(), Slot { term: term.clone(), command: command.clone(), cursor: 0 });
            id
        };
        let deadline = tokio::time::Instant::now() + OPEN_SETTLE_MAX;
        let (snap, _) = settle(term.as_ref(), 0, None, OPEN_SETTLE, deadline, &CancelToken::new()).await;
        self.advance(&id, snap.delta.next);
        let pid = term.identity().pid;
        let mut out = render(&id, &command, &snap, None);
        if let ExecOutcome::Done { value, content, .. } = &mut out {
            value["pid"] = json!(pid);
            if let Some(ContentBlock::Text { text }) = content.first_mut() {
                *text = format!("[opened {id}: `{command}` (pid {pid}) at {}x{}]\n{text}", size.cols, size.rows);
            }
        }
        out
    }

    fn advance(&self, id: &str, next: u64) {
        if let Some(s) = lk(&self.table).open.get_mut(id) {
            s.cursor = s.cursor.max(next);
        }
    }

    async fn send(&self, args: &Value) -> ExecOutcome {
        let (id, term, cursor) = match self.slot(args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut bytes = match args.get("text") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => s.as_bytes().to_vec(),
            Some(_) => return bad_args("`text` must be a string; nothing was sent."),
        };
        let names: Vec<String> = match args.get("keys") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => match a.iter().map(|k| k.as_str().map(str::to_string)).collect::<Option<Vec<_>>>() {
                Some(v) => v,
                None => return bad_args("`keys` must be an array of key names; nothing was sent."),
            },
            Some(_) => return bad_args("`keys` must be an array of key names; nothing was sent."),
        };
        let app_cursor = term.snapshot(cursor).screen.application_cursor;
        match keys::encode(&names, app_cursor) {
            Ok(k) => bytes.extend(k),
            Err(e) => return bad_args(format!("{e}; nothing was sent.")),
        }
        if bytes.is_empty() {
            return bad_args("give `text` and/or `keys`; nothing was sent.");
        }
        let n = bytes.len();
        match term.write(bytes).await {
            Ok(()) => ExecOutcome::Done {
                value: json!({"id": id, "sent_bytes": n}),
                content: vec![ContentBlock::text(format!("[sent {n} bytes to {id}; read the result with terminal_read]"))],
                meta: None,
            },
            Err(e) => term_err(e),
        }
    }

    async fn read(&self, args: &Value, cancel: &CancelToken) -> ExecOutcome {
        let (id, term, cursor) = match self.slot(args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let pattern = match args.get("wait_for") {
            None | Some(Value::Null) => None,
            Some(Value::String(p)) => match regex::RegexBuilder::new(p).multi_line(true).size_limit(1 << 20).build() {
                Ok(r) => Some(r),
                Err(e) => return bad_args(format!("`wait_for` is not a valid regex ({e}); nothing was read.")),
            },
            Some(_) => return bad_args("`wait_for` must be a string regex; nothing was read."),
        };
        let default_timeout = if pattern.is_some() { DEFAULT_WAIT_TIMEOUT_MS } else { DEFAULT_READ_TIMEOUT_MS };
        let (timeout_ms, idle_ms) =
            match (u64_arg(args, "timeout_ms", default_timeout, MAX_TIMEOUT_MS), u64_arg(args, "idle_ms", DEFAULT_IDLE_MS, MAX_TIMEOUT_MS))
            {
                (Ok(t), Ok(i)) => (t, i),
                (Err(e), _) | (_, Err(e)) => return e,
            };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let (snap, ended) = settle(term.as_ref(), cursor, pattern.as_ref(), Duration::from_millis(idle_ms), deadline, cancel).await;
        if cancel.is_cancelled() {
            return ExecOutcome::Cancelled;
        }
        self.advance(&id, snap.delta.next);
        let command = lk(&self.table).open.get(&id).map(|s| s.command.clone()).unwrap_or_default();
        let wait = pattern.as_ref().map(|p| (p.as_str().to_string(), ended, timeout_ms));
        render(&id, &command, &snap, wait)
    }

    fn resize(&self, args: &Value) -> ExecOutcome {
        let (id, term, _) = match self.slot(args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if args.get("cols").is_none() || args.get("rows").is_none() {
            return bad_args("`cols` and `rows` are both required; the size is unchanged.");
        }
        let size = match size_arg(args, Size::DEFAULT) {
            Ok(s) => s,
            Err(e) => return e,
        };
        match term.resize(size) {
            Ok(()) => ExecOutcome::Done {
                value: json!({"id": id, "cols": size.cols, "rows": size.rows}),
                content: vec![ContentBlock::text(format!("[{id} resized to {}x{}]", size.cols, size.rows))],
                meta: None,
            },
            Err(e) => term_err(e),
        }
    }

    async fn close(&self, args: &Value) -> ExecOutcome {
        let (id, term, _) = match self.slot(args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let closed = term.close().await;
        lk(&self.table).open.remove(&id);
        match closed {
            Ok(c) => {
                let mut text = format!(
                    "[closed {id}; killed {} process{}: {}]",
                    c.killed.len(),
                    if c.killed.len() == 1 { "" } else { "es" },
                    c.killed.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")
                );
                if let Some(e) = c.exit {
                    text.push_str(&format!("\n[exit {}{}]", e.code, e.signal.map(|s| format!(", signal {s}")).unwrap_or_default()));
                }
                if let Some(n) = &c.note {
                    text.push_str(&format!("\n[note: {n}]"));
                }
                ExecOutcome::Done {
                    value: json!({
                        "id": id, "killed": c.killed, "exit": c.exit.map(|e| e.code),
                        "signal": c.exit.and_then(|e| e.signal), "note": c.note,
                    }),
                    content: vec![ContentBlock::text(text)],
                    meta: None,
                }
            }
            Err(e) => term_err(e),
        }
    }

    fn list(&self) -> ExecOutcome {
        let rows: Vec<Value> = lk(&self.table)
            .open
            .iter()
            .map(|(id, s)| {
                let snap = s.term.snapshot(s.cursor);
                json!({
                    "id": id, "command": s.command, "pid": s.term.identity().pid,
                    "cols": snap.screen.size.cols, "rows": snap.screen.size.rows,
                    "running": snap.exit.is_none(), "exit": snap.exit.map(|e| e.code),
                    "unread_bytes": snap.delta.text.len(),
                })
            })
            .collect();
        let text = if rows.is_empty() {
            "(no terminals open)".to_string()
        } else {
            rows.iter()
                .map(|r| {
                    let state = if r["running"] == json!(true) { "running".to_string() } else { format!("exited {}", r["exit"]) };
                    format!(
                        "{}  pid {}  {}x{}  {}  {}",
                        r["id"].as_str().unwrap_or(""),
                        r["pid"],
                        r["cols"],
                        r["rows"],
                        state,
                        r["command"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        ExecOutcome::Done { value: json!({"terminals": rows}), content: vec![ContentBlock::text(text)], meta: None }
    }
}

/// Waits for `pattern` (in the new output or on the screen), else for `idle` without output, until
/// `deadline`; returns the last snapshot read from `cursor`. A finished program ends the wait.
async fn settle(
    term: &dyn Terminal,
    cursor: u64,
    pattern: Option<&regex::Regex>,
    idle: Duration,
    deadline: tokio::time::Instant,
    cancel: &CancelToken,
) -> (Snapshot, Ended) {
    let tick = Duration::from_millis(50);
    let mut last_change = tokio::time::Instant::now();
    let mut seen = None;
    loop {
        let snap = term.snapshot(cursor);
        if seen != Some(snap.seq) {
            seen = Some(snap.seq);
            last_change = tokio::time::Instant::now();
        }
        let now = tokio::time::Instant::now();
        let ended = match pattern {
            Some(p) if p.is_match(&snap.delta.text) || p.is_match(&snap.screen.text) => Some(Ended::Matched),
            _ if snap.exit.is_some() => Some(Ended::Exited),
            None if now.duration_since(last_change) >= idle => Some(Ended::Idle),
            _ if now >= deadline => Some(Ended::TimedOut),
            _ => None,
        };
        if let Some(e) = ended {
            return (snap, e);
        }
        if cancel.is_cancelled() {
            return (snap, Ended::TimedOut);
        }
        let wake = (now + tick).min(deadline);
        tokio::select! {
            _ = term.changed(snap.seq) => {}
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
}

/// The model-facing result of a read: output since the cursor, then the screen.
fn render(id: &str, command: &str, snap: &Snapshot, wait: Option<(String, Ended, u64)>) -> ExecOutcome {
    let s = &snap.screen;
    let mut text = String::new();
    if snap.delta.dropped > 0 {
        text.push_str(&format!("[… {} bytes of earlier output were dropped from the transcript …]\n", snap.delta.dropped));
    }
    let output = snap.delta.text.trim_end();
    text.push_str(&format!("── output since last read ──\n{}\n", if output.is_empty() { "(none)" } else { output }));
    text.push_str(&format!(
        "── screen {}x{}, cursor {},{}{} ──\n{}",
        s.size.cols,
        s.size.rows,
        s.cursor_row,
        s.cursor_col,
        if s.alternate { ", alternate screen" } else { "" },
        if s.text.is_empty() { "(blank)" } else { &s.text }
    ));
    let (matched, timed_out) = match &wait {
        Some((p, Ended::TimedOut, ms)) => {
            text.push_str(&format!("\n[wait_for /{p}/ did not match within {ms} ms]"));
            (Some(false), true)
        }
        Some((p, Ended::Exited, _)) => {
            text.push_str(&format!("\n[wait_for /{p}/ did not match before the program exited]"));
            (Some(false), false)
        }
        Some((_, e, _)) => (Some(*e == Ended::Matched), false),
        None => (None, false),
    };
    if let Some(e) = snap.exit {
        text.push_str(&format!(
            "\n[program exited {}{}; terminal_close {id} to release it]",
            e.code,
            e.signal.map(|s| format!(", signal {s}")).unwrap_or_default()
        ));
    }
    let value = json!({
        "id": id, "command": command, "output": snap.delta.text, "dropped": snap.delta.dropped,
        "screen": s.text, "cols": s.size.cols, "rows": s.size.rows, "cursor_row": s.cursor_row, "cursor_col": s.cursor_col,
        "alternate_screen": s.alternate, "title": s.title,
        "matched": matched, "timed_out": timed_out,
        "exit": snap.exit.map(|e| e.code), "signal": snap.exit.and_then(|e| e.signal),
    });
    let meta = Some(Card::Terminal { command: command.to_string(), output: s.text.clone(), exit: snap.exit.map(|e| e.code) });
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta }
}

#[cfg(test)]
mod tests;
