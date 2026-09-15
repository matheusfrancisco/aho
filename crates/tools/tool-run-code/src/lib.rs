//! `00`/`07`/`10` — `run_code`: programmatic tool calling (PTC). The model submits a Python or
//! Node program; it runs through the bound world's `Subprocess` handle (never `std::process`) in
//! the workspace, with a time limit and an output cap, and may call aho tools with the injected
//! helper (`aho.call("read", {"path": "x"})`, see `prelude`).
//!
//! Nested calls never bypass the session: each goes through the `aho.tool_call` service — the
//! session's own lookup, decision chain (rules, tool checks, guards, safety checks, preset),
//! approvals and pre/post hooks — and is recorded as `Ptc/Call` + `Ptc/Result` after the
//! program's result (10: "PTC sub-dispatch bounding is log-copy only": the log keeps at most
//! `LOG_COPY_BYTES` of each value; the program received all of it). If the session does not
//! provide the service, the program still runs and each `aho.call` fails with `PTC_UNAVAILABLE`.
//!
//! Classes (07 line 103): `run_code` is a `Subprocess` consumer, so its class is `SpawnsProcess`
//! (the ceiling of its handles). Under a world that does not confine (`host`, policy only) the
//! program runs as your user and can write anywhere you can: it is *also* `HostWrite`. That class
//! is not reachable through a tool's handles, so it is made visible instead of hidden: the tool's
//! own check treats the call as needing `HostWrite` whenever the world does not confine (Ask when
//! the preset lacks it and an approver exists, else Deny), and every result lists the effective
//! classes.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

pub mod frames;
pub mod prelude;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::OpKind;
use aho_proc::{Child, OpTable, OperationsHandle, OutputBuffer, ProcError, SpawnSpec, SubprocessHandle};
use aho_types::*;
use serde_json::json;

use crate::frames::{Event, Frames};

/// Session service: nested tool dispatch for PTC. The session seeds an empty slot before the
/// composition starts and fills it once it exists (see the crate report, "Core changes needed").
pub const TOOL_CALL_SERVICE: &str = "aho.tool_call";
/// `(parent call's view, cancel token, tool, args) → outcome`, through the session's decision chain.
pub type NestedToolCall = Arc<dyn Fn(ExecView, CancelToken, String, Value) -> BoxFuture<'static, ExecOutcome> + Send + Sync>;
pub type ToolCallSlot = Arc<OnceLock<NestedToolCall>>;

pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_MAX_OUTPUT: u64 = 64 * 1024;
pub const MIN_MAX_OUTPUT: u64 = 256;
pub const MAX_MAX_OUTPUT: u64 = 1 << 20;
pub const MAX_CODE_BYTES: usize = 1 << 20;
/// Nested tool calls per program.
pub const MAX_CALLS: usize = 256;
/// Bytes of a nested call's args/value kept in its `Ptc/*` log copy.
pub const LOG_COPY_BYTES: usize = 4096;
const TICK: Duration = Duration::from_millis(50);

pub struct RunCodeDeps {
    pub proc: SubprocessHandle,
    /// Seeded by the session (16): `job_kill` and cancellation reach the program's process group.
    pub ops: Option<OperationsHandle>,
    pub nested: Option<ToolCallSlot>,
}

impl Deps for RunCodeDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::Operations];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(RunCodeDeps { proc: r.handle()?, ops: r.handle::<OperationsHandle>().ok(), nested: r.service(TOOL_CALL_SERVICE) })
    }
}

#[derive(Clone, Debug)]
pub struct RunCodeConfig {
    pub python: String,
    pub node: String,
}

impl Default for RunCodeConfig {
    fn default() -> Self {
        RunCodeConfig { python: "python3".into(), node: "node".into() }
    }
}

impl Config for RunCodeConfig {
    const PREFIX: &'static str = "caps.tool-run-code";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "caps.tool-run-code.python",
                Ty::String,
                Some(json!("python3")),
                "Python interpreter `run_code` starts (name on PATH or full path).",
            ),
            ConfigKey::new(
                "caps.tool-run-code.node",
                Ty::String,
                Some(json!("node")),
                "Node.js interpreter `run_code` starts (name on PATH or full path).",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let d = RunCodeConfig::default();
        Ok(RunCodeConfig {
            python: cfg_str(l, "caps.tool-run-code.python", &d.python),
            node: cfg_str(l, "caps.tool-run-code.node", &d.node),
        })
    }
}

#[derive(Clone)]
pub struct ToolRunCode {
    proc: SubprocessHandle,
    ops: Option<OpTable>,
    nested: Option<ToolCallSlot>,
    cfg: RunCodeConfig,
}

impl Capability for ToolRunCode {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = RunCodeDeps;
    type Config = RunCodeConfig;

    fn build(deps: RunCodeDeps, cfg: RunCodeConfig) -> Result<Self, BuildError> {
        Ok(ToolRunCode { proc: deps.proc, ops: deps.ops.map(|o| o.0), nested: deps.nested, cfg })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let me = self.clone();
        let exec: ToolExec = Arc::new(move |req: ExecRequest| {
            let me = me.clone();
            Box::pin(async move { me.run(&req).await })
        });
        r.tool(&TOOLS[0], exec);
        r.entry_kind("Ptc", fold_ptc);
    }
}

declare_capability!(
    ToolRunCode,
    "tool-run-code",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_proc::SEAM), Requirement::AnyOf(&[Requirement::Binary("python3"), Requirement::Binary("node")])],
    tools = TOOLS,
    unlock = Fix::cli("./install-system-deps.sh"),
    entry_kinds = ["Ptc/Call", "Ptc/Result"],
    doc = "run_code (PTC): Python or Node programs run through the bound world's Subprocess in the workspace, with a time limit and an output cap; `aho.call(tool, args)` inside the program calls aho tools through the session's own decision chain and approvals (service `aho.tool_call`), logged as Ptc/Call + Ptc/Result. Class SpawnsProcess; under a world that does not confine (host) the program can write anywhere you can, so it is also HostWrite: the tool's check requires HostWrite there (ask or deny when the preset lacks it) and every result lists the effective classes."
);

pub static TOOLS: &[ToolDecl] = &[ToolDecl::new(
    "run_code",
    "Run a Python or Node program in the workspace and return its output (stdout+stderr) and exit code. Inside it, `aho.call(tool, args)` calls any tool you have (e.g. `aho.call(\"read\", {\"path\": \"a.txt\"})`) and returns the tool's value; errors raise `aho.ToolError` (code, reason). Every nested call goes through the same policy and approvals as your own calls. Loop over files, filter results, and print only what you need. Do not read stdin: it carries the tool channel.",
    Consequence::SpawnsProcess,
)
.schema(schema)
.check(check_run_code)
.op(OpKind::Process, false, true)
.present(PRESENT)];

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "language": {"type": "string", "enum": ["python", "node"]},
            "code": {"type": "string", "description": "The program. Python: `import aho` is optional (`aho` is predefined). Node: top-level `await` works."},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS,
                           "description": "Kill the program (its whole process group) after this long, nested tool calls included."},
            "max_output_bytes": {"type": "integer", "minimum": MIN_MAX_OUTPUT, "maximum": MAX_MAX_OUTPUT, "default": DEFAULT_MAX_OUTPUT,
                                 "description": "Output kept: the first and last halves of this many bytes; the middle is dropped and counted."}
        },
        "required": ["language", "code"],
        "additionalProperties": false
    }))
}

/// Effective classes of a run under `world`: `HostWrite` joins `SpawnsProcess` when the world does not confine.
pub fn effective_classes(world: &BoundWorld) -> ConsequenceSet {
    let base = ConsequenceSet::EMPTY.with(Consequence::SpawnsProcess);
    if world.confines { base } else { base.with(Consequence::HostWrite) }
}

/// `04` step 3: under a non-confining world the program is also `HostWrite` (07); a preset
/// without it asks (when an approver exists) or denies, as the chain treats any class above the preset.
pub fn check_run_code(_: &Value, view: &PolicyView) -> Option<Decision> {
    if view.world.confines || view.allowed.contains(Consequence::HostWrite) {
        return None;
    }
    let why = format!(
        "`run_code` runs your program as your user in world `{}` (enforced by: {}), so it can write anywhere you can: besides SpawnsProcess it is HostWrite (07), and the current policy does not allow HostWrite.",
        view.world.name, view.world.enforced_by
    );
    if view.allowed.contains(Consequence::TalksToHuman) {
        Some(Decision::Ask(format!(
            "{why} Approve this call, or switch to a preset that allows HostWrite on this world (`aho preset dev`)."
        )))
    } else {
        Some(Decision::Deny(format!("{why} Nothing ran. Switch to a preset that allows HostWrite on this world (`aho preset dev`).")))
    }
}

const PRESENT: Present = Present { call: present_call, result: present_result };

fn present_call(args: &Value) -> Card {
    let lang = args.get("language").and_then(Value::as_str).unwrap_or("?");
    Card::Terminal {
        command: format!("run_code ({lang})"),
        output: args.get("code").and_then(Value::as_str).unwrap_or("").to_string(),
        exit: None,
    }
}

fn present_result(args: &Value, v: &Value) -> Card {
    let lang = args.get("language").and_then(Value::as_str).unwrap_or("?");
    Card::Terminal {
        command: format!("run_code ({lang})"),
        output: v.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
        exit: v.get("exit").and_then(Value::as_i64).map(|c| c as i32),
    }
}

/// Fold for `Ptc/*`: every result closes exactly one earlier call of the same parent.
pub fn fold_ptc(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut s = prev.cloned().unwrap_or_else(|| json!({"calls": 0, "errors": 0, "open": []}));
    let key =
        format!("{}#{}", body.get("parent").and_then(Value::as_str).unwrap_or(""), body.get("n").and_then(Value::as_u64).unwrap_or(0));
    let mut open: Vec<String> =
        s["open"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    match name {
        "Call" => {
            if open.contains(&key) {
                return Err(format!("nested call {key} was logged twice"));
            }
            open.push(key);
            s["calls"] = json!(s["calls"].as_u64().unwrap_or(0) + 1);
        }
        "Result" => {
            let Some(i) = open.iter().position(|k| *k == key) else { return Err(format!("result for nested call {key} has no call")) };
            open.remove(i);
            if body.get("outcome").and_then(Value::as_str) != Some("ok") {
                s["errors"] = json!(s["errors"].as_u64().unwrap_or(0) + 1);
            }
        }
        other => return Err(format!("unknown entry Ptc/{other}")),
    }
    s["open"] = json!(open);
    Ok(s)
}

/// The log copy of a value: whole if small, else its size, digest and head.
fn log_copy(v: &Value) -> Value {
    let text = v.to_string();
    if text.len() <= LOG_COPY_BYTES {
        return v.clone();
    }
    let head: String = text.chars().take(LOG_COPY_BYTES / 4).collect();
    json!({"truncated": true, "bytes": text.len(), "blake3": Hash::of(text.as_bytes()).to_hex(), "head": head})
}

// ───────────────────────────── output cap ─────────────────────────────

/// Keeps the first `cap/2` and the last `cap/2` bytes of the program's own output.
struct Capped {
    half: usize,
    head: Vec<u8>,
    tail: OutputBuffer,
    total: u64,
}

impl Capped {
    fn new(cap: usize) -> Capped {
        let half = (cap / 2).max(1);
        Capped { half, head: Vec::new(), tail: OutputBuffer::new(half), total: 0 }
    }
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let take = (self.half - self.head.len().min(self.half)).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        self.tail.push(&bytes[take..]);
    }
    fn omitted(&self) -> u64 {
        self.tail.read(0).dropped
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

// ───────────────────────────── execution ─────────────────────────────

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Python,
    Node,
}

struct Args {
    language: Language,
    code: String,
    timeout_ms: u64,
    max_output: u64,
}

fn parse(args: &Value) -> Result<Args, ExecOutcome> {
    let language = match args.get("language").and_then(Value::as_str) {
        Some("python") => Language::Python,
        Some("node") => Language::Node,
        _ => return Err(bad_args("`language` must be \"python\" or \"node\"; nothing ran.")),
    };
    let code = args.get("code").and_then(Value::as_str).ok_or_else(|| bad_args("`code` is required and must be a string; nothing ran."))?;
    if code.trim().is_empty() {
        return Err(bad_args("`code` is empty; nothing ran."));
    }
    if code.len() > MAX_CODE_BYTES {
        return Err(bad_args(format!(
            "`code` is {} bytes, over the {MAX_CODE_BYTES}-byte limit; nothing ran. Put the bulk in a file and read it.",
            code.len()
        )));
    }
    let int = |key: &str, default: u64, min: u64, max: u64| -> Result<u64, ExecOutcome> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .filter(|n| *n >= min)
                .map(|n| n.min(max))
                .ok_or_else(|| bad_args(format!("`{key}` must be an integer ≥ {min}; nothing ran."))),
        }
    };
    Ok(Args {
        language,
        code: code.to_string(),
        timeout_ms: int("timeout_ms", DEFAULT_TIMEOUT_MS, 1, MAX_TIMEOUT_MS)?,
        max_output: int("max_output_bytes", DEFAULT_MAX_OUTPUT, MIN_MAX_OUTPUT, MAX_MAX_OUTPUT)?,
    })
}

enum Ended {
    Complete,
    Cancelled,
    TimedOut,
}

/// One nested call as the program made it, for the `Ptc/*` records.
struct Nested {
    n: u64,
    tool: String,
    args: Value,
    outcome: ExecOutcome,
}

async fn cancelled(c: &CancelToken) {
    while !c.is_cancelled() {
        tokio::time::sleep(TICK).await;
    }
}

impl ToolRunCode {
    pub async fn run(&self, req: &ExecRequest) -> ExecOutcome {
        let a = match parse(&req.view.args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let nonce = Hash::of(format!("{}:{}:{}", req.view.call.0, Timestamp::now().0, a.code.len()).as_bytes()).to_hex()[..16].to_string();
        let (interp, flags, prelude) = match a.language {
            Language::Python => (self.cfg.python.clone(), vec!["-u", "-c"], prelude::python(&nonce)),
            Language::Node => (self.cfg.node.clone(), vec!["-e"], prelude::node(&nonce)),
        };
        // stderr joins stdout at the fd level so frame pieces and output share one ordered pipe; a
        // missing interpreter is reported by a marker line instead of a shell's own wording.
        let missing = format!("__AHO_NO_INTERPRETER_{nonce}__");
        let script = format!("command -v \"$1\" >/dev/null 2>&1 || {{ echo {missing}; exit 127; }}; exec \"$@\" 2>&1");
        let mut argv = vec!["/bin/sh".to_string(), "-c".into(), script, "sh".into(), interp.clone()];
        argv.extend(flags.iter().map(|f| f.to_string()));
        argv.push(prelude);
        let mut stdin = serde_json::to_string(&a.code).unwrap_or_default().into_bytes();
        stdin.push(b'\n');
        let spec = SpawnSpec { argv, stdin: Some(stdin), timeout_ms: Some(a.timeout_ms), ..SpawnSpec::default() };
        let child = match self.proc.0.spawn_interactive(spec).await {
            Ok(c) => c,
            Err(e) => return ExecOutcome::err(e.code(), e.to_string()),
        };
        if let Some(t) = &self.ops
            && let Some(op) = t.op_for_call(&req.view.call)
        {
            t.attach_child(op, child.clone());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(a.timeout_ms);
        let mut out = Capped::new(a.max_output as usize);
        let mut nested: Vec<Nested> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let ended = self.pump(child.as_ref(), &nonce, req, deadline, &mut out, &mut nested, &mut notes).await;
        match ended {
            Ended::Cancelled => {
                if let Err(e) = child.kill().await {
                    notes.push(format!("the kill failed: {e}"));
                }
                return ExecOutcome::Cancelled;
            }
            Ended::TimedOut => {
                if let Err(e) = child.kill().await {
                    notes.push(format!("the kill failed: {e}"));
                }
            }
            Ended::Complete => {}
        }
        let exit = child.wait().await;
        let timed_out = matches!(ended, Ended::TimedOut) || matches!(exit, Err(ProcError::Timeout));
        let (code, signal) = match &exit {
            Ok(e) => (Some(e.code), e.signal),
            Err(ProcError::Timeout) => (None, None),
            Err(ProcError::NotFound(p)) => return ExecOutcome::err("PROC_NOT_FOUND", ProcError::NotFound(p.clone()).to_string()),
            Err(e) => return ExecOutcome::err(e.code(), e.to_string()),
        };
        if code == Some(127) && out.text().trim_end() == missing {
            return ExecOutcome::err(
                "PROC_NOT_FOUND",
                format!(
                    "`{interp}` was not found in this world; nothing ran. Install it, or point `caps.tool-run-code.{}` at it.",
                    if a.language == Language::Python { "python" } else { "node" }
                ),
            );
        }
        self.result(req, &a, out, code, signal, timed_out, nested, notes)
    }

    #[allow(clippy::too_many_arguments)] // one call site; the pieces are the run's whole state
    async fn pump(
        &self,
        child: &dyn Child,
        nonce: &str,
        req: &ExecRequest,
        deadline: tokio::time::Instant,
        out: &mut Capped,
        nested: &mut Vec<Nested>,
        notes: &mut Vec<String>,
    ) -> Ended {
        let mut frames = Frames::new(nonce);
        let mut cursor = 0u64;
        loop {
            let (bytes, next, done) = child.output(cursor);
            cursor = next;
            let mut events = frames.feed(&bytes);
            if done {
                events.extend(frames.finish());
            }
            for ev in events {
                match ev {
                    Event::Text(t) => out.push(&t),
                    Event::Bad(why) => {
                        notes.push(why.clone());
                        let answer = json!({"ok": false, "code": "PTC_BAD_FRAME", "reason": format!("{why}; the call did not run.")});
                        self.answer(child, &answer, notes).await;
                    }
                    Event::Request { id, tool, args } => {
                        let outcome = tokio::select! {
                            o = self.dispatch(req, nested.len(), &tool, args.clone()) => o,
                            _ = tokio::time::sleep_until(deadline) => return Ended::TimedOut,
                            _ = cancelled(&req.cancel) => return Ended::Cancelled,
                        };
                        let answer = match &outcome {
                            ExecOutcome::Done { value, .. } => json!({"id": id, "ok": true, "value": value}),
                            ExecOutcome::Err { code, reason } => json!({"id": id, "ok": false, "code": code, "reason": reason}),
                            ExecOutcome::Cancelled => {
                                json!({"id": id, "ok": false, "code": "CANCELLED", "reason": "the tool call was cancelled"})
                            }
                        };
                        nested.push(Nested { n: nested.len() as u64 + 1, tool, args, outcome });
                        self.answer(child, &answer, notes).await;
                    }
                }
            }
            if done {
                return Ended::Complete;
            }
            if req.cancel.is_cancelled() {
                return Ended::Cancelled;
            }
            if tokio::time::Instant::now() >= deadline {
                return Ended::TimedOut;
            }
            tokio::select! {
                _ = child.readable(cursor) => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }

    async fn answer(&self, child: &dyn Child, answer: &Value, notes: &mut Vec<String>) {
        let mut line = answer.to_string().into_bytes();
        line.push(b'\n');
        if let Err(e) = child.write_stdin(line).await {
            notes.push(format!("a tool-call answer could not be delivered ({e})"));
        }
    }

    /// One nested call, through the session's service; refused here only for what the service
    /// cannot know (recursion, the per-program budget, a session without the service).
    async fn dispatch(&self, req: &ExecRequest, made: usize, tool: &str, args: Value) -> ExecOutcome {
        if tool == TOOLS[0].name {
            return ExecOutcome::err(
                "PTC_RECURSION",
                "`run_code` cannot start another program from inside a program; the call did not run.",
            );
        }
        if made >= MAX_CALLS {
            return ExecOutcome::err(
                "PTC_LIMIT",
                format!(
                    "this program already made {MAX_CALLS} tool calls, the per-program limit; the call did not run. Split the work over several run_code calls."
                ),
            );
        }
        let Some(call) = self.nested.as_ref().and_then(|s| s.get()).cloned() else {
            return ExecOutcome::err(
                "PTC_UNAVAILABLE",
                format!(
                    "this session does not provide nested tool dispatch (service `{TOOL_CALL_SERVICE}`), so `{tool}` did not run; the program keeps running. Call `{tool}` directly instead."
                ),
            );
        };
        call(req.view.clone(), req.cancel.child(), tool.to_string(), args).await
    }

    #[allow(clippy::too_many_arguments)] // one call site
    fn result(
        &self,
        req: &ExecRequest,
        a: &Args,
        out: Capped,
        code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        nested: Vec<Nested>,
        notes: Vec<String>,
    ) -> ExecOutcome {
        let output = out.text();
        let mut text = if output.is_empty() { "(no output)".to_string() } else { output.clone() };
        let mut lines = Vec::new();
        if timed_out {
            lines.push(format!("[killed after {} ms: the time limit was reached; raise timeout_ms to allow longer]", a.timeout_ms));
        } else if let Some(sig) = signal {
            lines.push(format!("[killed by signal {sig}]"));
        } else if let Some(c) = code.filter(|c| *c != 0) {
            lines.push(format!("[exit {c}]"));
        }
        if out.omitted() > 0 {
            lines.push(format!(
                "[output capped at {} bytes: {} of {} bytes omitted from the middle; raise max_output_bytes to keep more]",
                a.max_output,
                out.omitted(),
                out.total
            ));
        }
        if !nested.is_empty() {
            let failed = nested.iter().filter(|n| !matches!(n.outcome, ExecOutcome::Done { .. })).count();
            lines.push(format!(
                "[{} nested tool call{}{}]",
                nested.len(),
                if nested.len() == 1 { "" } else { "s" },
                if failed > 0 { format!(", {failed} failed") } else { String::new() }
            ));
        }
        lines.extend(notes.iter().map(|n| format!("[note: {n}]")));
        for l in lines {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&l);
        }
        let parent = req.view.call.0.clone();
        let mut events = Vec::new();
        for n in &nested {
            events.push(
                json!({"ns": "Ptc", "name": "Call", "body": {"parent": parent, "n": n.n, "tool": n.tool, "args": log_copy(&n.args)}}),
            );
            let (outcome, code, value) = match &n.outcome {
                ExecOutcome::Done { value, .. } => ("ok", Value::Null, log_copy(value)),
                ExecOutcome::Err { code, reason } => ("err", json!(code), json!(reason)),
                ExecOutcome::Cancelled => ("cancelled", Value::Null, Value::Null),
            };
            events.push(json!({"ns": "Ptc", "name": "Result", "body": {"parent": parent, "n": n.n, "outcome": outcome, "code": code, "value": value}}));
        }
        let language = match a.language {
            Language::Python => "python",
            Language::Node => "node",
        };
        let value = json!({
            "language": language, "exit": code, "signal": signal, "timed_out": timed_out,
            "output": output, "output_bytes": out.total, "omitted_bytes": out.omitted(),
            "tool_calls": nested.len(), "classes": effective_classes(&req.view.policy.world),
            "notes": notes, "events": events,
        });
        let meta = Some(Card::Terminal { command: format!("run_code ({language})"), output, exit: code });
        ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta }
    }
}

#[cfg(test)]
mod tests;
