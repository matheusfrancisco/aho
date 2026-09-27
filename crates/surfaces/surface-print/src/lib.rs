//! The print surface: a live transcript on a terminal, or `--json` NDJSON for wrappers.
//!
//! Text mode renders assistant text as it streams, thinking dimmed, one card per tool call and
//! result (from the tool's own `present` functions and the `Card` a result carries), approvals,
//! checkpoints, and anything that stopped a turn. `--hud` prefixes each durable entry with
//! its id, principal, and (for tool calls) the capability that owns the tool.
//!
//! JSON mode is the wrapper contract: every durable log entry is one line of JSON on stdout,
//! nothing else is written there, and the error stream stays empty unless something failed.
//! Stream chunks are not durable and never appear in JSON mode.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, Write};

use aho_cap::{BuildError, Capability, CapabilityId, Kind, Manifest, Registrar, Registry};
use aho_llm::StreamEvent;
use aho_log::{ApprovalOutcome, CapDeltaOp, CheckpointCause, Entry, EntryKind, GrantScope, OpStatus, ToolOutcome, Transition};
use aho_policy::ApprovalCard;
use aho_session::SessionEvent;
use aho_types::*;

// ───────────────────────────── capability ─────────────────────────────

pub struct SurfacePrint;

impl Capability for SurfacePrint {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(SurfacePrint)
    }
    fn register(&self, _r: &mut Registrar<'_>) {}
}

aho_cap::declare_capability!(
    SurfacePrint,
    "surface-print",
    kind = Kind::Surface,
    provides = [],
    requires = [],
    tools = &[],
    doc = "One-shot print surface: live text transcript, or `--json` log entries as NDJSON on stdout."
);

// ───────────────────────────── styling ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// ANSI colour; the caller decides (TTY and `NO_COLOR`).
    pub color: bool,
    /// Debug HUD: id, principal, and owning capability on every durable entry.
    pub hud: bool,
    /// Print operator prompts too (replaying a transcript, not typing it).
    pub echo_user: bool,
}

#[derive(Clone, Copy)]
struct Paint(bool);

impl Paint {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
}

/// At most `n` characters of `s` on one line, with `…` when cut.
pub fn clip(s: &str, n: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    let multi = s.lines().nth(1).is_some();
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n.saturating_sub(1)).collect::<String>())
    } else if multi {
        format!("{one} …")
    } else {
        one.to_string()
    }
}

fn principal(p: &Principal) -> String {
    match p {
        Principal::Operator => "operator".into(),
        Principal::Engineer => "engineer".into(),
        Principal::Agent { .. } => "agent".into(),
        Principal::System { name } => format!("system:{name}"),
    }
}

fn outcome_text(o: ApprovalOutcome, s: GrantScope) -> &'static str {
    match (o, s) {
        (ApprovalOutcome::AllowedOnce, GrantScope::Once) => "allowed once",
        (ApprovalOutcome::AllowedOnce, GrantScope::Session) => "allowed for this session",
        (ApprovalOutcome::AllowedOnce, GrantScope::UntilRevoked) => "allowed until revoked",
        (ApprovalOutcome::Rejected, _) => "rejected",
        (ApprovalOutcome::Cancelled, _) => "cancelled",
        (ApprovalOutcome::Unavailable, _) => "no one could answer (prompts are off), so it did not run",
    }
}

fn cause_text(c: &CheckpointCause) -> String {
    match c {
        CheckpointCause::ToolResult(call) => format!("after {}", call.0),
        CheckpointCause::TurnEnd => "turn end".into(),
        CheckpointCause::Manual => "manual".into(),
        CheckpointCause::PreRewind => "before rewind".into(),
    }
}

fn transition_text(t: &Transition) -> String {
    match t {
        Transition::Aborted(why) => format!("aborted: {why}"),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.get("transition").and_then(|x| x.as_str()).map(|s| s.replace('_', " ")))
            .unwrap_or_else(|| format!("{other:?}")),
    }
}

// ───────────────────────────── cards ─────────────────────────────

const RESULT_LINES: usize = 12;

/// One line naming what a call does, from the tool's own `present.call` card.
pub fn call_summary(card: &Card) -> String {
    match card {
        Card::Terminal { command, .. } => format!("$ {}", clip(command, 100)),
        Card::Read { path, range: Some((a, b)) } => format!("{path} (lines {a}–{b})"),
        Card::Read { path, range: None } => path.clone(),
        Card::Diff { path, .. } => path.clone(),
        Card::Search { query, .. } => clip(query, 100),
        Card::Web { url, .. } => url.clone(),
        Card::Generic { body, .. } => clip(body, 100),
    }
}

/// The body of a result card, one element per output line (colour applied when `color`).
pub fn card_lines(card: &Card, color: bool) -> Vec<String> {
    let p = Paint(color);
    let mut out = Vec::new();
    let capped = |lines: Vec<String>, out: &mut Vec<String>, max: usize| {
        let n = lines.len();
        out.extend(lines.into_iter().take(max));
        if n > max {
            out.push(p.dim(&format!("… {} more lines", n - max)));
        }
    };
    match card {
        Card::Terminal { output, exit, .. } => {
            capped(output.lines().map(str::to_string).collect(), &mut out, RESULT_LINES);
            match exit {
                Some(0) => out.push(p.dim("[exit 0]")),
                Some(c) => out.push(p.red(&format!("[exit {c}]"))),
                None => out.push(p.yellow("[still running]")),
            }
        }
        Card::Diff { path, hunks } => {
            out.push(p.bold(path));
            let lines: Vec<String> = hunks
                .iter()
                .flat_map(|h| h.lines().map(str::to_string).collect::<Vec<_>>())
                .map(|l| match l.chars().next() {
                    Some('+') => p.green(&l),
                    Some('-') => p.red(&l),
                    _ => l,
                })
                .collect();
            capped(lines, &mut out, 2 * RESULT_LINES);
        }
        Card::Read { path, range } => out.push(match range {
            Some((a, b)) => format!("read {path} (lines {a}–{b})"),
            None => format!("read {path}"),
        }),
        Card::Search { query, hits } => {
            out.push(format!("{} hit(s) for {}", hits.len(), clip(query, 60)));
            capped(hits.clone(), &mut out, 8);
        }
        Card::Web { url, title } => out.push(format!("{title} <{url}>")),
        Card::Generic { title, body } => {
            if !title.is_empty() && title != "result" {
                out.push(p.bold(title));
            }
            capped(body.lines().map(str::to_string).collect(), &mut out, 3);
        }
    }
    out
}

/// `Preset("workspace")` → `Preset`: the reason's kind, for card footers and wire payloads.
pub fn reason_label(r: &aho_policy::DecisionReason) -> String {
    let d = format!("{r:?}");
    d.split(['(', ' ', '{']).next().unwrap_or(&d).to_string()
}

/// The preset an approval's `[p]` answer switches to: the card's own `preset <X>` cut or, for
/// a preset refusal without one, the smallest preset whose cut in this world contains the class.
pub fn preset_cut(card: &ApprovalCard) -> Option<String> {
    if let Some(p) = card.suggested_cut.as_deref().and_then(|c| c.strip_prefix("preset ")) {
        return Some(p.to_string());
    }
    match card.reason {
        aho_policy::DecisionReason::Preset(_) => {
            aho_policy::Preset::smallest_containing(ConsequenceSet::EMPTY.with(card.class), &card.world).map(|p| p.name().to_string())
        }
        _ => None,
    }
}

const CARD_W: usize = 74;

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let add = if cur.is_empty() { word.chars().count() } else { cur.chars().count() + 1 + word.chars().count() };
            if add > width && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        lines.push(cur);
    }
    lines
}

fn boxed(title: &str, rows: &[String]) -> String {
    let inner = CARD_W - 4;
    let mut out = format!("┌ {title} {}┐\n", "─".repeat(CARD_W - 4 - title.chars().count()));
    for r in rows {
        let text: String = r.chars().take(inner).collect();
        let pad = inner - text.chars().count();
        out.push_str(&format!("│ {text}{} │\n", " ".repeat(pad)));
    }
    out.push_str(&format!("└{}┘\n", "─".repeat(CARD_W - 2)));
    out
}

/// The approval card, built from the card data only (never a live tool object).
pub fn render_approval(card: &ApprovalCard, cap: Option<&str>) -> String {
    let inner = CARD_W - 4;
    let name = match cap {
        Some(c) => format!("{c}.{}", card.tool),
        None => card.tool.clone(),
    };
    let head = format!("class: {}", card.class.name());
    let gap = inner.saturating_sub(name.chars().count() + head.chars().count()).max(2);
    let mut rows = vec![format!("{name}{}{head}", " ".repeat(gap))];
    rows.push(format!("args: {}", clip(&card.args.to_string(), inner - 6)));
    for (i, l) in wrap(&card.rendered, inner - 6).into_iter().enumerate() {
        rows.push(if i == 0 { format!("why:  {l}") } else { format!("      {l}") });
    }
    rows.push(format!("reason: {}", reason_label(&card.reason)));
    rows.push(format!("world: {}  ·  enforced by: {}", card.world.name, card.world.enforced_by));
    if let Some(cut) = card.suggested_cut.as_deref().filter(|c| !c.starts_with("preset ")) {
        let text = format!("{cut} (a lasting change; this answer covers one call)");
        for (i, l) in wrap(&text, inner - 6).into_iter().enumerate() {
            rows.push(if i == 0 { format!("fix:  {l}") } else { format!("      {l}") });
        }
    }
    rows.push(String::new());
    rows.push("[y] allow once   [s] allow for session   [r] reject   [Esc] cancel".into());
    if let Some(p) = preset_cut(card) {
        rows.push(format!("[p] aho preset {p}   (writes the preset, allows this call for the session)"));
    }
    boxed("approval", &rows)
}

// ───────────────────────────── printer ─────────────────────────────

struct ToolInfo {
    cap: String,
    present: fn(&Value) -> Card,
}

/// Renders session events to `out`; errors go to `err`.
pub struct Printer<W: Write, E: Write> {
    mode: Mode,
    style: Style,
    out: W,
    err: E,
    tools: BTreeMap<String, ToolInfo>,
    calls: BTreeMap<CallId, String>,
    streamed: bool,
    thinking: bool,
    mid_line: bool,
    last: Option<EntryId>,
}

impl<W: Write, E: Write> Printer<W, E> {
    pub fn new(mode: Mode, style: Style, out: W, err: E) -> Self {
        Printer {
            mode,
            style,
            out,
            err,
            tools: BTreeMap::new(),
            calls: BTreeMap::new(),
            streamed: false,
            thinking: false,
            mid_line: false,
            last: None,
        }
    }

    /// Learns every registered tool's owning capability and `present.call` card.
    pub fn with_registry(mut self, reg: &Registry) -> Self {
        for (cap, decl, _) in &reg.tools {
            self.tools.insert(decl.name.to_string(), ToolInfo { cap: cap.name.to_string(), present: decl.present.call });
        }
        for (cap, spec, _) in &reg.dynamic_tools {
            self.tools.insert(spec.name.clone(), ToolInfo { cap: cap.name.to_string(), present: generic_call });
        }
        self
    }

    /// Learns the linked tools' owning capability and `present.call` from the manifests alone,
    /// for rendering a log offline (`aho session show`) without starting anything.
    pub fn with_manifests(mut self, manifests: &[&'static aho_cap::Manifest]) -> Self {
        for m in manifests {
            for decl in m.tools {
                self.tools.entry(decl.name.to_string()).or_insert(ToolInfo { cap: m.id.name.to_string(), present: decl.present.call });
            }
        }
        self
    }

    pub fn into_parts(self) -> (W, E) {
        (self.out, self.err)
    }

    fn paint(&self) -> Paint {
        Paint(self.style.color)
    }

    fn end_line(&mut self) -> io::Result<()> {
        if self.mid_line {
            writeln!(self.out)?;
            self.mid_line = false;
        }
        Ok(())
    }

    fn line(&mut self, s: &str) -> io::Result<()> {
        self.end_line()?;
        writeln!(self.out, "{s}")
    }

    pub fn event(&mut self, ev: &SessionEvent) -> io::Result<()> {
        match ev {
            SessionEvent::Entry(e) => self.entry(e),
            SessionEvent::Error(msg) => {
                if self.mode == Mode::Text {
                    self.end_line()?;
                    self.out.flush()?;
                }
                let p = self.paint();
                writeln!(self.err, "{}", p.red(&format!("error: {msg}")))?;
                self.err.flush()
            }
            _ if self.mode == Mode::Json => Ok(()),
            SessionEvent::Stream(s) => self.stream(s),
            SessionEvent::Idle { turn, transition } => {
                self.end_line()?;
                if self.style.hud {
                    let p = self.paint();
                    self.line(&p.dim(&format!("— turn {} · {}", turn.0, transition_text(transition))))?;
                }
                self.out.flush()
            }
        }
    }

    fn stream(&mut self, s: &StreamEvent) -> io::Result<()> {
        let p = self.paint();
        let text = match s {
            StreamEvent::Text(t) => {
                if self.thinking {
                    self.end_line()?;
                    self.thinking = false;
                }
                self.streamed = true;
                t.clone()
            }
            StreamEvent::Thinking(t) => {
                if !self.thinking {
                    self.end_line()?;
                    self.thinking = true;
                }
                p.dim(t)
            }
            _ => return Ok(()),
        };
        if text.is_empty() {
            return Ok(());
        }
        write!(self.out, "{text}")?;
        self.mid_line = !text.ends_with('\n');
        self.out.flush()
    }

    /// Renders one durable entry. An entry already printed (replay then live) is skipped.
    pub fn entry(&mut self, e: &Entry) -> io::Result<()> {
        if self.last.is_some_and(|l| e.id <= l) {
            return Ok(());
        }
        self.last = Some(e.id);
        if self.mode == Mode::Json {
            let line = serde_json::to_string(e).map_err(io::Error::other)?;
            writeln!(self.out, "{line}")?;
            return self.out.flush();
        }
        if self.style.hud {
            self.hud(e)?;
        }
        self.text(e)?;
        self.out.flush()
    }

    fn hud(&mut self, e: &Entry) -> io::Result<()> {
        let p = self.paint();
        let what = match &e.kind {
            EntryKind::ConfigResolved { key, sourced } => {
                format!("{key} = {} ← {}", clip(&sourced.value.to_string(), 60), sourced.winner.render())
            }
            EntryKind::ToolCall { tool, .. } => match self.tools.get(tool) {
                Some(t) => format!("cap {}", t.cap),
                None => "cap (not linked)".into(),
            },
            EntryKind::RequestHeader { model, tools_visible, policy_epoch, open_calls, plan_hash, .. } => format!(
                "model {model} · epoch {} · {} tools · {} open · plan {}",
                policy_epoch.0,
                tools_visible.len(),
                open_calls.len(),
                plan_hash.short()
            ),
            EntryKind::PlanSealed { plan_hash, fingerprint, schema_hash } => {
                format!("plan {} · fingerprint {} · schema {}", plan_hash.short(), fingerprint.short(), schema_hash.short())
            }
            EntryKind::CapActivated { id, deps, .. } => format!("{id} active · holds {}", deps.join(", ")),
            EntryKind::HookInvoked { point, cap, matcher } => format!("hook {cap} at {point} ({matcher})"),
            EntryKind::HookResult { decision, ms, .. } => {
                format!("hook → {decision} in {ms} ms{}", if *ms > 2000 { " ⚠ slow" } else { "" })
            }
            EntryKind::SystemMessage { sections, .. } => {
                format!("system prompt: {}", sections.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "))
            }
            _ => String::new(),
        };
        let head = format!("[#{} {} by {}]", e.id.0, e.kind.tag(), principal(&e.by));
        let s = if what.is_empty() { head } else { format!("{head} {what}") };
        self.line(&p.dim(&s))
    }

    fn text(&mut self, e: &Entry) -> io::Result<()> {
        let p = self.paint();
        match &e.kind {
            EntryKind::UserMessage { form, text, .. } => match form {
                Form::Prompt if self.style.echo_user => self.line(&format!("{} {text}", p.bold("›"))),
                Form::Notice => self.line(&p.dim(&format!("note: {text}"))),
                _ => Ok(()),
            },
            EntryKind::AssistantMessage { content, .. } => {
                if !self.streamed {
                    for b in content {
                        match b {
                            ContentBlock::Text { text } if !text.is_empty() => self.line(text)?,
                            ContentBlock::Thinking { text } if !text.is_empty() => self.line(&p.dim(text))?,
                            _ => {}
                        }
                    }
                }
                self.streamed = false;
                self.thinking = false;
                self.end_line()
            }
            EntryKind::ToolCall { call, tool, args, class } => {
                self.calls.insert(call.clone(), tool.clone());
                let summary = match self.tools.get(tool) {
                    Some(t) => call_summary(&(t.present)(args)),
                    None => clip(&args.to_string(), 100),
                };
                self.line(&format!("{} {} {}  {}", p.cyan("●"), p.bold(tool), summary, p.dim(class.name())))
            }
            EntryKind::ToolResult { call, outcome, content, meta, late, .. } => {
                let tool = self.calls.get(call).cloned().unwrap_or_else(|| call.0.clone());
                let late = if *late { p.dim("  (late result)") } else { String::new() };
                match outcome {
                    ToolOutcome::Ok => {
                        let lines = match meta {
                            Some(card) => card_lines(card, self.style.color),
                            None => {
                                let text: String = content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n");
                                card_lines(&Card::Generic { title: String::new(), body: text }, self.style.color)
                            }
                        };
                        for l in lines {
                            self.line(&format!("  {l}"))?;
                        }
                        if !late.is_empty() {
                            self.line(&late)?;
                        }
                        Ok(())
                    }
                    ToolOutcome::Err { code, reason } => self.line(&format!("{}{late}", p.red(&format!("  ✕ {tool}: {reason} ({code})")))),
                    ToolOutcome::Cancelled { reason } => {
                        self.line(&format!("{}{late}", p.yellow(&format!("  ⊘ {tool} cancelled ({reason:?})"))))
                    }
                }
            }
            EntryKind::ApprovalRequested { for_cap, reason, .. } => {
                self.line(&p.yellow(&format!("? approval needed ({for_cap}): {reason}")))
            }
            EntryKind::ApprovalDecided { outcome, scope, .. } => self.line(&format!("  → {}", outcome_text(*outcome, *scope))),
            EntryKind::PolicyEpoch { epoch, allowed, preset, .. } if epoch.0 > 1 => {
                self.line(&p.dim(&format!("policy epoch {}: {preset} (allowed: {allowed})", epoch.0)))
            }
            EntryKind::WorkspaceCheckpoint { id, cause, .. } => {
                self.line(&p.dim(&format!("◆ checkpoint c{} ({})", id.0, cause_text(cause))))
            }
            EntryKind::WorkspaceRestored { to, mode, changed, deleted, .. } => {
                self.line(&format!("↺ workspace restored to c{} ({mode:?}): {} changed, {} deleted", to.0, changed.len(), deleted.len()))
            }
            EntryKind::StepEnd { transition, .. } => match transition {
                Transition::Aborted(why) => self.line(&p.red(&format!("✕ the turn stopped: {why}"))),
                Transition::MaxTurns => {
                    self.line(&p.yellow("the turn stopped at `session.max_turns`; raise it with `aho set session.max_turns <n>`"))
                }
                Transition::PromptTooLong => self.line(&p.red("the conversation no longer fits the model's context; compact or fork it")),
                _ => Ok(()),
            },
            EntryKind::AssistantAttempt { error } => self.line(&p.yellow(&format!("model call failed, retrying: {error}"))),
            EntryKind::OpDone { op, status: OpStatus::Uncertain { .. } } => {
                self.line(&p.yellow(&format!("⚠ operation {} may or may not have happened (the process died mid-dispatch)", op.0)))
            }
            EntryKind::CapDelta { id, op: CapDeltaOp::Broken { err }, .. } => self.line(&p.red(&format!("✕ {id} is broken: {err}"))),
            EntryKind::LoadoutDiagnostic { lint, severity, key, detail } if severity != "info" => {
                self.line(&p.yellow(&format!("⚠ {lint} {key}: {detail}")))
            }
            EntryKind::CompactionStart { .. } => self.line(&p.dim("compacting the conversation…")),
            EntryKind::CompactionEnd { error: Some(err), .. } => self.line(&p.yellow(&format!("compaction failed: {err}"))),
            EntryKind::CompactionEnd { error: None, .. } => self.line(&p.dim("compacted")),
            EntryKind::Note { text } => self.line(&p.dim(&format!("note: {text}"))),
            EntryKind::SubagentStarted { child, provider, .. } => {
                self.line(&p.dim(&format!("subagent {:x} started ({provider})", child.0)))
            }
            EntryKind::SubagentEnded { child, .. } => self.line(&p.dim(&format!("subagent {:x} ended", child.0))),
            EntryKind::InvariantChecked { id, verdict, .. } => self.line(&format!("invariant {id}: {verdict:?}")),
            _ => Ok(()),
        }
    }

    /// Ends a half-written line and flushes both streams.
    pub fn finish(&mut self) -> io::Result<()> {
        self.end_line()?;
        self.out.flush()?;
        self.err.flush()
    }
}

fn generic_call(v: &Value) -> Card {
    Card::Generic { title: "call".into(), body: v.to_string() }
}

#[cfg(test)]
mod tests;
