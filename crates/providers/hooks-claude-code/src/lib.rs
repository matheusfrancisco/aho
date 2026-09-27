//! Claude Code `hooks.json` compatibility as one `Kind::Hook` capability.
//!
//! Reads `<project>/.claude/settings.json`, `<project>/.claude/settings.local.json`, and every
//! file in `hooks.claude_code.files` (a bootstrap snapshot: edits take effect on restart). Each
//! `type: "command"` hook runs as `sh -c <command>` in the project with the Claude Code JSON
//! payload on stdin (`session_id, cwd, hook_event_name, tool_name, tool_input, tool_use_id,
//! tool_response`), `${CLAUDE_PROJECT_DIR}` substituted in the command text and set in the
//! environment, and `AHO_HOOKS_DISABLED=1` set for the child as the recursion guard: an aho
//! started with it (config key `hooks.disabled`) runs no hooks. Hooks matching one event run in
//! parallel; identical commands run once.
//!
//! | Claude Code event | aho point | effect of a block |
//! |---|---|---|
//! | `PreToolUse` | `Registrar::pre_execute` | the call is denied with the reason |
//! | `PostToolUse` | `Registrar::post_execute` | the result is blocked; the reason is feedback |
//! | `UserPromptSubmit` | `HookEvent::PreStep` (first step of each turn) | the turn stops |
//! | `SessionStart` | `HookEvent::SessionStart` | none (stdout becomes context) |
//! | `SessionEnd` | `HookEvent::SessionEnd` | none |
//! | `Stop` | `HookEvent::TurnStopping` | the turn continues with the reason |
//! | `SubagentStop` | `HookEvent::SubagentStop` | the subagent continues with the reason |
//! | `PreCompact` | `HookEvent::PreCompact` | none |
//!
//! Output: exit 0 = pass (stdout JSON is read when it starts with `{`); exit 2 = block, stderr is
//! the reason; JSON `decision: "block"` + `reason`, `continue: false` + `stopReason`,
//! `hookSpecificOutput.permissionDecision: allow|deny|ask` + `permissionDecisionReason`, and
//! `hookSpecificOutput.additionalContext` (a deferred notice after the call, or turn context) are
//! honored. Results meet (`deny ⊓ ask ⊓ allow`): a hook can narrow, never widen, and an `allow`
//! is still subject to the bypass-immune policy steps that already ran.
//!
//! `updatedInput` is parsed and refused: the logged `Tool/Call.args` are the model's bytes and
//! are never rewritten. A hook that asks for a rewrite gets the original call routed to the
//! operator (`ask`) with a warning, and the warning is attached after the call.
//!
//! Fail-closed: a hook that cannot start, times out, exits with anything but 0/2, or prints
//! invalid JSON counts as a denial unless the hook says `"on_error": "allow"` (aho extension;
//! default from `hooks.claude_code.on_error`). An unreadable hooks file denies every tool call
//! until it is fixed (or `on_error = "allow"`, which skips the file).
//!
//! Gap list (Claude Code behavior not reproduced):
//! * `transcript_path` and `permission_mode` are not in the payload (aho has no transcript
//!   file; `permission_mode` is always `default`).
//! * `tool_input` is aho's argument object; a `path` argument is mirrored as `file_path`.
//!   Tool names are mapped for matchers and `tool_name` (`bash`→`Bash`, `read`→`Read`, …);
//!   matchers also match the aho name.
//! * `updatedInput` is refused (above); `suppressOutput` and `systemMessage` are ignored.
//! * `Notification`, `PermissionRequest` and other events without an aho point are skipped with a
//!   warning; `prompt`/`agent`/`http` hook types are skipped with a warning.
//! * `UserPromptSubmit` sees `prompt` only when the session passes it in the `PreStep` payload.
//! * `pre_execute`/`post_execute` runs are not logged as `Hook/Invoked`/`Hook/Result` (the session
//!   logs only `HookSpec` runs); they are visible in the decision reasons.
//! * User-level `~/.claude/settings.json` is not read unless listed in `hooks.claude_code.files`.
//! * Stop hooks do not receive `stop_hook_active: true` on a re-stop (always `false`).

#![forbid(unsafe_code)]

mod run;
mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_types::*;
use serde_json::json;

pub use run::{MAX_OUTPUT, RunResult, run_command, substitute};
pub use settings::{CcEvent, Defaults, HookCommand, HookGroup, Matcher, Parsed, parse_settings};

/// Service holding what was loaded: `Arc<HooksStatus>`.
pub const STATUS_SERVICE: &str = "hooks.claude-code";
pub const DEFAULT_TIMEOUT_S: u64 = 60;
/// Files read from the project, in order.
pub const PROJECT_FILES: &[&str] = &[".claude/settings.json", ".claude/settings.local.json"];

/// aho tool name → Claude Code tool name (for matchers and `tool_name`).
pub const CC_NAMES: &[(&str, &str)] = &[
    ("bash", "Bash"),
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("ls", "LS"),
    ("web_fetch", "WebFetch"),
    ("web_search", "WebSearch"),
    ("notebook_edit", "NotebookEdit"),
    ("todo_write", "TodoWrite"),
    ("task", "Task"),
];

pub fn cc_tool_name(aho: &str) -> &str {
    CC_NAMES.iter().find(|(a, _)| *a == aho).map(|(_, c)| *c).unwrap_or(aho)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HooksStatus {
    pub disabled: bool,
    /// Files that existed and were read.
    pub files: Vec<String>,
    pub groups: usize,
    pub warnings: Vec<String>,
    /// First unreadable file, if any.
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HooksConfig {
    pub project: PathBuf,
    pub files: Vec<PathBuf>,
    pub defaults: Defaults,
    pub disabled: bool,
}

fn truthy(key: &str, v: Option<&Value>) -> Result<bool, ConfigError> {
    match v {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Number(n)) => Ok(n.as_f64() != Some(0.0)),
        Some(Value::String(s)) => Ok(!matches!(s.trim(), "" | "0" | "false" | "no")),
        Some(_) => Err(ConfigError::Type { key: key.into(), expected: "true, false, 1 or 0" }),
    }
}

impl Config for HooksConfig {
    const PREFIX: &'static str = "hooks";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "hooks.disabled",
                Ty::Any,
                Some(json!(false)),
                "No hooks run. Hook children get `AHO_HOOKS_DISABLED=1`, so an aho a hook starts cannot recurse.",
            ),
            ConfigKey::new(
                "hooks.claude_code.project_dir",
                Ty::String,
                None,
                "Project whose `.claude/settings*.json` hooks run; default: the directory aho was started in.",
            ),
            ConfigKey::new(
                "hooks.claude_code.files",
                Ty::Array,
                Some(json!([])),
                "Extra Claude Code hooks files (settings.json or hooks.json shape); relative to the project.",
            ),
            ConfigKey::new(
                "hooks.claude_code.on_error",
                Ty::String,
                Some(json!("deny")),
                "If a hook fails (cannot start, times out, bad exit, bad JSON): deny (fail closed) or allow. A hook's own `on_error` wins.",
            ),
            ConfigKey::new(
                "hooks.claude_code.timeout_s",
                Ty::Int,
                Some(json!(DEFAULT_TIMEOUT_S)),
                "Default per-hook timeout in seconds (a hook's `timeout` wins).",
            ),
        ]
    }

    #[allow(clippy::disallowed_methods, reason = "authority: hooks-claude-code-config")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let disabled = truthy("hooks.disabled", l.get("hooks.disabled"))?;
        let project = match l.str("hooks.claude_code.project_dir") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => std::env::current_dir().map_err(|_| ConfigError::MissingKey("hooks.claude_code.project_dir".into()))?,
        };
        let on_error = match l.str("hooks.claude_code.on_error").unwrap_or("deny") {
            "deny" => OnError::Deny,
            "allow" => OnError::Allow,
            _ => return Err(ConfigError::Type { key: "hooks.claude_code.on_error".into(), expected: "\"deny\" or \"allow\"" }),
        };
        let timeout_s = match l.get("hooks.claude_code.timeout_s") {
            None => DEFAULT_TIMEOUT_S,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::Type { key: "hooks.claude_code.timeout_s".into(), expected: "a positive integer" })?,
        };
        let mut files: Vec<PathBuf> = PROJECT_FILES.iter().map(|f| project.join(f)).collect();
        match l.get("hooks.claude_code.files") {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => {
                for v in a {
                    let Some(s) = v.as_str() else {
                        return Err(ConfigError::Type { key: "hooks.claude_code.files".into(), expected: "an array of file paths" });
                    };
                    let p = PathBuf::from(s);
                    files.push(if p.is_absolute() { p } else { project.join(p) });
                }
            }
            Some(_) => return Err(ConfigError::Type { key: "hooks.claude_code.files".into(), expected: "an array of file paths" }),
        }
        Ok(HooksConfig { project, files, defaults: Defaults { timeout: Duration::from_secs(timeout_s), on_error }, disabled })
    }
}

// ───────────────────────────── interpreting one run ─────────────────────────────

/// One hook run's verdict, before combining.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Interp {
    pub block: Option<String>,
    pub ask: Option<String>,
    pub allow: bool,
    pub context: Vec<String>,
    pub stop: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

fn label(cmd: &HookCommand) -> String {
    let c = cmd.command.trim();
    match c.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &c[..i]),
        None => c.to_string(),
    }
}

fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Claude Code semantics for one run's exit code and output.
pub fn interpret(event: CcEvent, cmd: &HookCommand, r: &RunResult) -> Interp {
    let mut out = Interp::default();
    let l = label(cmd);
    if let Some(e) = &r.spawn_error {
        out.error = Some(format!("hook `{l}` could not start ({e})"));
        return out;
    }
    if r.timed_out {
        out.error = Some(format!("hook `{l}` ran past its {}s timeout and was killed", cmd.timeout.as_secs_f64()));
        return out;
    }
    match r.exit {
        Some(0) => {}
        Some(2) => {
            out.block = Some(nonempty(Some(&r.stderr)).unwrap_or_else(|| format!("hook `{l}` blocked this (exit 2) without a reason")));
            return out;
        }
        Some(n) => {
            let why = nonempty(Some(&r.stderr)).map(|s| format!(": {s}")).unwrap_or_default();
            out.error = Some(format!("hook `{l}` failed with exit {n}{why}"));
            return out;
        }
        None => {
            out.error = Some(format!("hook `{l}` was killed by a signal"));
            return out;
        }
    }
    let stdout = r.stdout.trim();
    if !stdout.starts_with('{') {
        if !stdout.is_empty() && matches!(event, CcEvent::UserPromptSubmit | CcEvent::SessionStart) {
            out.context.push(stdout.to_string());
        }
        return out;
    }
    let v: Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(e) => {
            out.error = Some(format!("hook `{l}` printed output starting with `{{` that is not valid JSON ({e})"));
            return out;
        }
    };
    if v.get("continue") == Some(&Value::Bool(false)) {
        out.stop = Some(nonempty(v.get("stopReason").and_then(Value::as_str)).unwrap_or_else(|| format!("hook `{l}` stopped processing")));
    }
    let reason = nonempty(v.get("reason").and_then(Value::as_str));
    match v.get("decision").and_then(Value::as_str) {
        None => {}
        Some("block") => out.block = Some(reason.clone().unwrap_or_else(|| format!("hook `{l}` blocked this"))),
        Some("approve") => out.allow = true,
        Some(other) => {
            out.error = Some(format!("hook `{l}` returned an unknown decision `{other}`"));
            return out;
        }
    }
    let hso = v.get("hookSpecificOutput").and_then(Value::as_object);
    if let Some(h) = hso {
        if let Some(d) = h.get("permissionDecision").and_then(Value::as_str) {
            let why = nonempty(h.get("permissionDecisionReason").and_then(Value::as_str));
            match d {
                "allow" => out.allow = true,
                "deny" => out.block = Some(why.unwrap_or_else(|| format!("hook `{l}` denied this"))),
                "ask" => out.ask = Some(why.unwrap_or_else(|| format!("hook `{l}` asks for confirmation"))),
                other => {
                    out.error = Some(format!("hook `{l}` returned an unknown permissionDecision `{other}`"));
                    return out;
                }
            }
        }
        if let Some(c) = nonempty(h.get("additionalContext").and_then(Value::as_str)) {
            out.context.push(c);
        }
    }
    // Parsed and refused: the call's logged arguments are never rewritten, so the original goes
    // to the operator instead of silently running in place of what the hook wanted.
    if v.get("updatedInput").is_some() || hso.is_some_and(|h| h.contains_key("updatedInput")) {
        let refused = format!(
            "hook `{l}` returned `updatedInput`; aho never rewrites a logged call's arguments, so the rewrite was refused and the original call goes to the operator"
        );
        out.ask = Some(match out.ask.take() {
            Some(a) => format!("{a}; {refused}"),
            None => refused.clone(),
        });
        out.warnings.push(refused);
    }
    out
}

/// The meet of several runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Combined {
    pub deny: Vec<String>,
    pub ask: Vec<String>,
    pub context: Vec<String>,
    pub stop: Option<String>,
    pub notes: Vec<String>,
}

pub fn combine(runs: &[(&HookCommand, Interp)]) -> Combined {
    let mut c = Combined::default();
    for (cmd, i) in runs {
        if let Some(e) = &i.error {
            match cmd.on_error {
                OnError::Deny => c.deny.push(format!(
                    "{e}; aho fails closed on hook errors (set `\"on_error\": \"allow\"` on this hook in {} to ignore its failures)",
                    cmd.source
                )),
                OnError::Allow => c.notes.push(format!("{e}; ignored (`on_error: allow`)")),
            }
        }
        if let Some(b) = &i.block {
            c.deny.push(b.clone());
        }
        if let Some(a) = &i.ask {
            c.ask.push(a.clone());
        }
        if let Some(s) = &i.stop {
            c.stop.get_or_insert(s.clone());
            c.deny.push(s.clone());
        }
        c.context.extend(i.context.iter().cloned());
        c.notes.extend(i.warnings.iter().cloned());
    }
    c
}

/// Every matching command (deduplicated), run in parallel.
fn run_all(cmds: Vec<&HookCommand>, event: CcEvent, project: &Path, payload: &Value) -> Combined {
    let mut seen = BTreeSet::new();
    let cmds: Vec<&HookCommand> = cmds.into_iter().filter(|c| seen.insert(c.command.as_str())).collect();
    let runs: Vec<(&HookCommand, Interp)> = std::thread::scope(|s| {
        let handles: Vec<_> = cmds.iter().map(|c| (*c, s.spawn(move || run_command(c, project, payload)))).collect();
        handles
            .into_iter()
            .map(|(c, h)| {
                let r = h
                    .join()
                    .unwrap_or_else(|_| RunResult { spawn_error: Some("the hook runner thread panicked".into()), ..RunResult::default() });
                (c, interpret(event, c, &r))
            })
            .collect()
    });
    combine(&runs)
}

// ───────────────────────────── capability ─────────────────────────────

struct Shared {
    project: PathBuf,
    session: Mutex<String>,
    /// Warnings and contexts from `PreToolUse`, attached after the call by `post_execute`.
    notes: Mutex<BTreeMap<CallId, Vec<String>>>,
    last_prompt_turn: Mutex<Option<u64>>,
}

impl Shared {
    fn base(&self, event: CcEvent) -> Value {
        json!({
            "session_id": lock(&self.session).clone(),
            "cwd": self.project.display().to_string(),
            "hook_event_name": event.name(),
        })
    }
}

fn tool_input(args: &Value) -> Value {
    let mut v = args.clone();
    if let Value::Object(m) = &mut v
        && let (Some(p), false) = (m.get("path").cloned(), m.contains_key("file_path"))
    {
        m.insert("file_path".into(), p);
    }
    v
}

fn tool_response(o: &ExecOutcome) -> Value {
    match o {
        ExecOutcome::Done { value, .. } => value.clone(),
        ExecOutcome::Err { code, reason } => json!({"error": reason, "code": code}),
        ExecOutcome::Cancelled => json!({"cancelled": true}),
    }
}

fn matching<'a>(groups: &'a [HookGroup], event: CcEvent, candidates: &[&str]) -> Vec<&'a HookCommand> {
    groups.iter().filter(|g| g.event == event && g.matcher.matches(candidates)).flat_map(|g| g.commands.iter()).collect()
}

pub struct ClaudeCodeHooks {
    shared: Arc<Shared>,
    groups: Arc<Vec<HookGroup>>,
    load_error: Option<String>,
    on_error: OnError,
    status: Arc<HooksStatus>,
}

const ON_SESSION_START: &[HookEvent] = &[HookEvent::SessionStart];
const ON_SESSION_END: &[HookEvent] = &[HookEvent::SessionEnd];
const ON_PRE_STEP: &[HookEvent] = &[HookEvent::PreStep];
const ON_TURN_STOPPING: &[HookEvent] = &[HookEvent::TurnStopping];
const ON_SUBAGENT_STOP: &[HookEvent] = &[HookEvent::SubagentStop];
const ON_PRE_COMPACT: &[HookEvent] = &[HookEvent::PreCompact];

impl ClaudeCodeHooks {
    fn pre(shared: &Shared, groups: &[HookGroup], view: &ExecView) -> PreDecision {
        let cc = cc_tool_name(&view.tool);
        let cmds = matching(groups, CcEvent::PreToolUse, &[cc, view.tool.as_str()]);
        if cmds.is_empty() {
            return PreDecision::Allow;
        }
        let mut payload = shared.base(CcEvent::PreToolUse);
        payload["tool_name"] = json!(cc);
        payload["tool_input"] = tool_input(&view.args);
        payload["tool_use_id"] = json!(view.call.0);
        payload["permission_mode"] = json!("default");
        let c = run_all(cmds, CcEvent::PreToolUse, &shared.project, &payload);
        let notes: Vec<String> = c.context.iter().chain(c.notes.iter()).cloned().collect();
        if !notes.is_empty() {
            lock(&shared.notes).entry(view.call.clone()).or_default().extend(notes);
        }
        if !c.deny.is_empty() {
            PreDecision::Deny { reason: c.deny.join("; ") }
        } else if !c.ask.is_empty() {
            PreDecision::Ask { reason: Some(c.ask.join("; ")) }
        } else {
            PreDecision::Allow
        }
    }

    fn post(shared: &Shared, groups: &[HookGroup], view: &ExecView, outcome: &ExecOutcome) -> PostDecision {
        let mut deferred = lock(&shared.notes).remove(&view.call).unwrap_or_default();
        let cc = cc_tool_name(&view.tool);
        let cmds = matching(groups, CcEvent::PostToolUse, &[cc, view.tool.as_str()]);
        if cmds.is_empty() {
            return PostDecision::Accept { content: None, deferred_context: deferred };
        }
        let mut payload = shared.base(CcEvent::PostToolUse);
        payload["tool_name"] = json!(cc);
        payload["tool_input"] = tool_input(&view.args);
        payload["tool_use_id"] = json!(view.call.0);
        payload["tool_response"] = tool_response(outcome);
        let c = run_all(cmds, CcEvent::PostToolUse, &shared.project, &payload);
        deferred.extend(c.context.iter().chain(c.notes.iter()).cloned());
        if c.deny.is_empty() {
            PostDecision::Accept { content: None, deferred_context: deferred }
        } else {
            PostDecision::Block { feedback: c.deny.join("; "), deferred_context: deferred }
        }
    }

    /// One `HookSpec` per non-tool Claude Code event.
    fn event_spec(&self, event: CcEvent) -> Option<HookSpec> {
        let on = match event {
            CcEvent::SessionStart => ON_SESSION_START,
            CcEvent::SessionEnd => ON_SESSION_END,
            CcEvent::UserPromptSubmit => ON_PRE_STEP,
            CcEvent::Stop => ON_TURN_STOPPING,
            CcEvent::SubagentStop => ON_SUBAGENT_STOP,
            CcEvent::PreCompact => ON_PRE_COMPACT,
            CcEvent::PreToolUse | CcEvent::PostToolUse => return None,
        };
        let longest = self.groups.iter().filter(|g| g.event == event).flat_map(|g| &g.commands).map(|c| c.timeout).max();
        if longest.is_none() && event != CcEvent::SessionStart {
            return None;
        }
        let (shared, groups) = (self.shared.clone(), self.groups.clone());
        let run = Arc::new(move |_: HookEvent, payload: &Value| -> BoxFuture<'static, HookOutput> {
            let (shared, groups, payload) = (shared.clone(), groups.clone(), payload.clone());
            let (tx, rx) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(run_event(&shared, &groups, event, &payload));
            });
            Box::pin(async move {
                rx.await.unwrap_or(HookOutput {
                    decision: Some(Decision::Deny("the hook runner thread failed".into())),
                    ..HookOutput::default()
                })
            })
        });
        Some(HookSpec {
            on,
            if_class: None,
            if_tool: None,
            timeout_ms: longest.unwrap_or(Duration::ZERO).as_millis() as u64 + 2_000,
            once: false,
            on_error: self.on_error,
            run,
        })
    }
}

/// Runs one non-tool event's hooks against the session's payload.
fn run_event(shared: &Shared, groups: &[HookGroup], event: CcEvent, payload: &Value) -> HookOutput {
    let str_of = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
    let mut p = shared.base(event);
    let candidates: Vec<String> = match event {
        CcEvent::SessionStart => {
            if let Some(id) = str_of("session").or_else(|| str_of("session_id")) {
                *lock(&shared.session) = id.clone();
                p["session_id"] = json!(id);
            }
            let source = str_of("source").unwrap_or_else(|| "startup".into());
            p["source"] = json!(source);
            vec![source]
        }
        CcEvent::UserPromptSubmit => {
            if let Some(turn) = payload.get("turn").and_then(Value::as_u64) {
                let mut last = lock(&shared.last_prompt_turn);
                if *last == Some(turn) {
                    return HookOutput::default();
                }
                *last = Some(turn);
            }
            p["prompt"] = json!(str_of("prompt").unwrap_or_default());
            vec![]
        }
        CcEvent::Stop | CcEvent::SubagentStop => {
            p["stop_hook_active"] = json!(false);
            vec![]
        }
        CcEvent::PreCompact => {
            let trigger = str_of("trigger").unwrap_or_else(|| "auto".into());
            p["trigger"] = json!(trigger);
            p["custom_instructions"] = json!(str_of("custom_instructions").unwrap_or_default());
            vec![trigger]
        }
        CcEvent::SessionEnd => {
            p["reason"] = json!(str_of("reason").unwrap_or_else(|| "other".into()));
            vec![]
        }
        CcEvent::PreToolUse | CcEvent::PostToolUse => vec![],
    };
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let cmds = matching(groups, event, &refs);
    if cmds.is_empty() {
        return HookOutput::default();
    }
    let c = run_all(cmds, event, &shared.project, &p);
    let mut out = HookOutput { decision: None, context: c.context, stop: c.stop };
    out.context.extend(c.notes);
    if !c.deny.is_empty() {
        let reason = c.deny.join("; ");
        match event {
            CcEvent::UserPromptSubmit => {
                out.stop.get_or_insert(reason.clone());
            }
            CcEvent::Stop | CcEvent::SubagentStop => out.context.push(reason.clone()),
            _ => {}
        }
        out.decision = Some(Decision::Deny(reason));
    } else if !c.ask.is_empty() {
        out.decision = Some(Decision::Ask(c.ask.join("; ")));
    }
    out
}

impl Capability for ClaudeCodeHooks {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = HooksConfig;

    #[allow(clippy::disallowed_methods, reason = "authority: hooks-claude-code-settings")]
    fn build(_: (), cfg: HooksConfig) -> Result<Self, BuildError> {
        let mut status = HooksStatus { disabled: cfg.disabled, ..HooksStatus::default() };
        let mut groups = Vec::new();
        let mut load_error = None;
        if !cfg.disabled {
            for f in &cfg.files {
                let path = f.display().to_string();
                let text = match std::fs::read_to_string(f) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        load_error.get_or_insert(format!("{path} could not be read ({e})"));
                        continue;
                    }
                };
                status.files.push(path.clone());
                match parse_settings(&text, &path, &cfg.defaults) {
                    Ok(p) => {
                        groups.extend(p.groups);
                        status.warnings.extend(p.warnings);
                    }
                    Err(e) => {
                        load_error.get_or_insert(e);
                    }
                }
            }
        }
        status.groups = groups.len();
        status.error = load_error.clone();
        Ok(ClaudeCodeHooks {
            shared: Arc::new(Shared {
                project: cfg.project,
                session: Mutex::new(String::new()),
                notes: Mutex::new(BTreeMap::new()),
                last_prompt_turn: Mutex::new(None),
            }),
            groups: Arc::new(groups),
            load_error,
            on_error: cfg.defaults.on_error,
            status: Arc::new(status),
        })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.service(STATUS_SERVICE, self.status.clone());
        if self.status.disabled {
            return;
        }
        if let (Some(err), OnError::Deny) = (&self.load_error, self.on_error) {
            let reason = format!(
                "the Claude Code hooks could not be loaded: {err}. Every tool call is refused until the file is fixed (restart aho after), or set `hooks.claude_code.on_error = \"allow\"` to run without it."
            );
            r.pre_execute(Arc::new(move |_: &ExecView| PreDecision::Deny { reason: reason.clone() }));
            return;
        }
        if self.groups.is_empty() {
            return;
        }
        let has = |e: CcEvent| self.groups.iter().any(|g| g.event == e);
        if has(CcEvent::PreToolUse) {
            let (shared, groups) = (self.shared.clone(), self.groups.clone());
            r.pre_execute(Arc::new(move |view: &ExecView| ClaudeCodeHooks::pre(&shared, &groups, view)));
        }
        if has(CcEvent::PreToolUse) || has(CcEvent::PostToolUse) {
            let (shared, groups) = (self.shared.clone(), self.groups.clone());
            r.post_execute(Arc::new(move |view: &ExecView, o: &ExecOutcome| ClaudeCodeHooks::post(&shared, &groups, view, o)));
        }
        for event in CcEvent::ALL {
            if let Some(spec) = self.event_spec(event) {
                r.hook(spec);
            }
        }
    }
}

declare_capability!(
    ClaudeCodeHooks,
    "hooks-claude-code",
    kind = Kind::Hook,
    provides = [SeamId("hooks")],
    requires = [Requirement::Binary("sh")],
    tools = &[],
    doc = "Claude Code hooks (`.claude/settings.json`, hooks.json): exit 2 blocks, JSON decisions honored, `updatedInput` refused, fail-closed."
);
