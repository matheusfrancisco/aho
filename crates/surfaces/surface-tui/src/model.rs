//! Everything the TUI draws. [`Transcript`] and [`UiState::state`] are folds of the session log
//! (plus the transient stream of the in-flight model call); [`Local`] is the only state the
//! surface owns (draft, cursor, scroll, focus, zoom).

use std::collections::BTreeMap;

use aho_cap::{Plan, Registry};
use aho_config::Loadout;
use aho_inspect::{Card as InvCard, InvariantRow, TreeNode};
use aho_llm::StreamEvent;
use aho_llm::models::Price;
use aho_log::{
    ApprovalOutcome, CapDeltaOp, CapState, CheckpointCause, CustomFolds, Entry, EntryKind, GrantScope, RewindMode, State, ToolOutcome,
    Transition,
};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_proc::OpInfo;
use aho_types::*;

use crate::keys::Keymap;
use crate::slash::CommandInfo;
use crate::theme::Theme;

// ───────────────────────────── transcript ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultStatus {
    Ok,
    Err,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultView {
    pub status: ResultStatus,
    pub summary: String,
    pub body: Vec<String>,
    pub late: bool,
    pub entry: EntryId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCard {
    pub call: CallId,
    pub tool: String,
    /// The capability that registered the tool (HUD origin).
    pub cap: Option<String>,
    pub class: Option<Consequence>,
    pub args: Value,
    pub op: Option<OpId>,
    /// `None` = placeholder: the call is still running.
    pub result: Option<ToolResultView>,
    /// Approval asked/answered for this call, as the log recorded it.
    pub decision: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    User(String),
    Notice(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolCard),
    Checkpoint { id: CheckpointId, cause: String, turn: u64 },
    Restored { from: CheckpointId, to: CheckpointId, mode: RewindMode, changed: usize, deleted: usize },
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub entry: EntryId,
    pub by: Principal,
    pub body: Body,
}

/// The current branch of the log as transcript blocks, plus the live stream of the running call.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    calls: BTreeMap<CallId, usize>,
    approvals: BTreeMap<EntryId, CallId>,
    /// Last entry folded on this branch; a new entry whose parent is not the tip means the head
    /// moved (fork/rewind) and the transcript must be refolded from that entry's branch.
    pub tip: Option<EntryId>,
    /// Highest entry id ever folded (ids are monotonic per log).
    pub seen: Option<EntryId>,
    pub turn: u64,
    pub turn_open: bool,
    pub live_text: String,
    pub live_thinking: String,
    pub live_tools: Vec<String>,
}

impl Transcript {
    pub fn from_entries(entries: &[Entry], tool_caps: &BTreeMap<String, String>) -> Transcript {
        let mut t = Transcript::default();
        for e in entries {
            t.apply(e, tool_caps);
        }
        t
    }

    /// Whether `e` continues the folded branch (else: refold from `e`'s branch).
    pub fn continues(&self, e: &Entry) -> bool {
        self.tip.is_none() || e.parent == self.tip
    }

    pub fn already_seen(&self, e: &Entry) -> bool {
        self.seen.is_some_and(|s| e.id <= s)
    }

    fn push(&mut self, e: &Entry, body: Body) {
        self.blocks.push(Block { entry: e.id, by: e.by.clone(), body });
    }

    fn card(&mut self, e: &Entry, call: &CallId, tool: &str, args: &Value, tool_caps: &BTreeMap<String, String>) -> usize {
        if let Some(&i) = self.calls.get(call) {
            return i;
        }
        let card = ToolCard {
            call: call.clone(),
            tool: tool.to_string(),
            cap: tool_caps.get(tool).cloned(),
            class: None,
            args: args.clone(),
            op: None,
            result: None,
            decision: None,
        };
        self.push(e, Body::Tool(card));
        let i = self.blocks.len() - 1;
        self.calls.insert(call.clone(), i);
        i
    }

    fn tool_mut(&mut self, i: usize) -> Option<&mut ToolCard> {
        match self.blocks.get_mut(i).map(|b| &mut b.body) {
            Some(Body::Tool(c)) => Some(c),
            _ => None,
        }
    }

    pub fn apply(&mut self, e: &Entry, tool_caps: &BTreeMap<String, String>) {
        self.tip = Some(e.id);
        self.seen = Some(self.seen.map_or(e.id, |s| s.max(e.id)));
        let bootstrap = matches!(&e.by, Principal::System { name } if name == "bootstrap");
        match &e.kind {
            EntryKind::TurnStart { turn } => {
                self.turn = turn.0;
                self.turn_open = true;
            }
            EntryKind::TurnEnd { .. } => {
                self.turn_open = false;
                self.clear_live();
            }
            EntryKind::UserMessage { form, text, .. } => match form {
                Form::Prompt => self.push(e, Body::User(text.clone())),
                Form::Notice => self.push(e, Body::Notice(text.clone())),
                Form::Snapshot | Form::Catalog | Form::Instructions => {}
            },
            EntryKind::AssistantMessage { content, .. } => {
                self.clear_live();
                for b in content {
                    match b {
                        ContentBlock::Text { text } if !text.trim().is_empty() => self.push(e, Body::Assistant(text.clone())),
                        ContentBlock::Thinking { text } if !text.trim().is_empty() => self.push(e, Body::Thinking(text.clone())),
                        ContentBlock::ToolUse { id, name, args } => {
                            self.card(e, id, name, args, tool_caps);
                        }
                        _ => {}
                    }
                }
            }
            EntryKind::AssistantAttempt { error } => self.push(e, Body::Error(format!("model request failed: {error}"))),
            EntryKind::ToolCall { call, tool, args, class } => {
                let i = self.card(e, call, tool, args, tool_caps);
                if let Some(c) = self.tool_mut(i) {
                    c.tool = tool.clone();
                    c.cap = tool_caps.get(tool).cloned();
                    c.args = args.clone();
                    c.class = Some(*class);
                }
            }
            EntryKind::OpStarted { op, call, .. } => {
                if let Some(&i) = self.calls.get(call)
                    && let Some(c) = self.tool_mut(i)
                {
                    c.op = Some(*op);
                }
            }
            EntryKind::ToolResult { call, outcome, content, meta, late, .. } => {
                let i = self.card(e, call, "tool", &Value::Null, tool_caps);
                let view = result_view(e.id, outcome, content, meta.as_ref(), *late);
                if let Some(c) = self.tool_mut(i) {
                    c.result = Some(view);
                }
            }
            EntryKind::ApprovalRequested { call: Some(call), reason, .. } => {
                self.approvals.insert(e.id, call.clone());
                if let Some(&i) = self.calls.get(call)
                    && let Some(c) = self.tool_mut(i)
                {
                    c.decision = Some(format!("approval asked: {reason}"));
                }
            }
            EntryKind::ApprovalDecided { req, outcome, scope } => {
                if let Some(call) = self.approvals.get(req).cloned()
                    && let Some(&i) = self.calls.get(&call)
                    && let Some(c) = self.tool_mut(i)
                {
                    c.decision = Some(decision_text(*outcome, *scope).to_string());
                }
            }
            EntryKind::WorkspaceCheckpoint { id, cause, .. } => {
                let cause = match cause {
                    CheckpointCause::ToolResult(call) => {
                        let tool = self.calls.get(call).and_then(|&i| self.blocks.get(i)).and_then(|b| match &b.body {
                            Body::Tool(c) => Some(c.tool.clone()),
                            _ => None,
                        });
                        format!("after {}", tool.unwrap_or_else(|| format!("call {}", call.0)))
                    }
                    CheckpointCause::TurnEnd => "turn end".into(),
                    CheckpointCause::Manual => "manual".into(),
                    CheckpointCause::PreRewind => "before rewind".into(),
                };
                self.push(e, Body::Checkpoint { id: *id, cause, turn: self.turn });
            }
            EntryKind::WorkspaceRestored { from, to, mode, changed, deleted } => {
                self.push(e, Body::Restored { from: *from, to: *to, mode: *mode, changed: changed.len(), deleted: deleted.len() })
            }
            EntryKind::StepEnd { transition: Transition::Aborted(reason), .. } => {
                self.push(e, Body::Error(format!("turn stopped: {reason}")))
            }
            EntryKind::PolicyEpoch { epoch, preset, .. } if !bootstrap => {
                self.push(e, Body::Notice(format!("policy epoch {} (preset {preset})", epoch.0)))
            }
            EntryKind::CapDelta { id, op, reason } if !bootstrap => {
                let what = match op {
                    CapDeltaOp::Activated => "activated".to_string(),
                    CapDeltaOp::Deactivated => "deactivated".to_string(),
                    CapDeltaOp::Broken { err } => format!("broken: {err}"),
                    CapDeltaOp::Replaced { by } => format!("replaced by {by}"),
                    CapDeltaOp::Rejected { .. } => "change rejected".to_string(),
                };
                self.push(e, Body::Notice(format!("{id} {what} ({reason})")))
            }
            _ => {}
        }
    }

    pub fn stream(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::Text(t) => self.live_text.push_str(t),
            StreamEvent::Thinking(t) => self.live_thinking.push_str(t),
            StreamEvent::ToolUseStart { name, .. } => self.live_tools.push(name.clone()),
            StreamEvent::ToolUseEnd { .. } | StreamEvent::Usage(_) => {}
        }
    }

    pub fn clear_live(&mut self) {
        self.live_text.clear();
        self.live_thinking.clear();
        self.live_tools.clear();
    }

    /// Blocks before the first tool card still waiting for its result: they will not change
    /// again, so an inline surface may commit them to the terminal's scrollback.
    pub fn settled(&self) -> usize {
        self.blocks.iter().position(|b| matches!(&b.body, Body::Tool(c) if c.result.is_none())).unwrap_or(self.blocks.len())
    }

    pub fn card_for(&self, call: &CallId) -> Option<&ToolCard> {
        self.calls.get(call).and_then(|&i| match &self.blocks.get(i)?.body {
            Body::Tool(c) => Some(c),
            _ => None,
        })
    }
}

fn decision_text(o: ApprovalOutcome, s: GrantScope) -> &'static str {
    match (o, s) {
        (ApprovalOutcome::AllowedOnce, GrantScope::Once) => "approved once",
        (ApprovalOutcome::AllowedOnce, GrantScope::Session) => "approved for the session",
        (ApprovalOutcome::AllowedOnce, GrantScope::UntilRevoked) => "approved until revoked",
        (ApprovalOutcome::Rejected, _) => "rejected by the operator",
        (ApprovalOutcome::Cancelled, _) => "approval cancelled",
        (ApprovalOutcome::Unavailable, _) => "no one could answer (prompts off)",
    }
}

const BODY_LINES: usize = 6;

fn result_view(entry: EntryId, outcome: &ToolOutcome, content: &[ContentBlock], meta: Option<&Card>, late: bool) -> ToolResultView {
    let text: String = content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n");
    let tail = |s: &str| -> Vec<String> {
        let lines: Vec<String> = s.lines().map(str::to_string).collect();
        let skip = lines.len().saturating_sub(BODY_LINES);
        lines.into_iter().skip(skip).collect()
    };
    let head = |s: &str| -> Vec<String> { s.lines().take(BODY_LINES).map(str::to_string).collect() };
    let (status, summary, body) = match outcome {
        ToolOutcome::Err { code, reason } => (ResultStatus::Err, format!("{code}: {reason}"), vec![]),
        ToolOutcome::Cancelled { reason } => (ResultStatus::Cancelled, format!("cancelled ({reason:?})").to_lowercase(), vec![]),
        ToolOutcome::Ok => match meta {
            Some(Card::Terminal { command, output, exit }) => {
                (ResultStatus::Ok, format!("$ {command}  → exit {}", exit.map_or_else(|| "?".to_string(), |c| c.to_string())), tail(output))
            }
            Some(Card::Diff { path, hunks }) => {
                (ResultStatus::Ok, format!("edited {path}"), hunks.iter().flat_map(|h| head(h)).take(BODY_LINES).collect())
            }
            Some(Card::Read { path, range }) => (
                ResultStatus::Ok,
                match range {
                    Some((a, b)) => format!("read {path}:{a}-{b}"),
                    None => format!("read {path}"),
                },
                vec![],
            ),
            Some(Card::Search { query, hits }) => {
                (ResultStatus::Ok, format!("{} hits for {query}", hits.len()), hits.iter().take(BODY_LINES).cloned().collect())
            }
            Some(Card::Web { url, title }) => (ResultStatus::Ok, format!("{title} <{url}>"), vec![]),
            Some(Card::Generic { title, body }) => (ResultStatus::Ok, title.clone(), head(body)),
            None => {
                let mut lines = head(&text);
                let first = if lines.is_empty() { "ok".to_string() } else { lines.remove(0) };
                (ResultStatus::Ok, first, lines)
            }
        },
    };
    ToolResultView { status, summary, body, late, entry }
}

// ───────────────────────────── approval card ─────────────────────────────

/// The approval card, built from the card payload and the fold only (never a live tool object).
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalView {
    pub tool: String,
    pub cap: Option<String>,
    pub class: Consequence,
    pub args: String,
    pub why: String,
    pub reason: &'static str,
    pub world: String,
    pub enforced_by: String,
    pub lease: String,
    /// The epoch a session-scoped allow opens.
    pub epoch_next: u32,
    pub cut: Option<String>,
}

impl ApprovalView {
    pub fn new(card: &ApprovalCard, cap: Option<String>, state: &State) -> ApprovalView {
        let now = Timestamp::now();
        let live: Vec<&String> = state.leases.values().filter(|(_, exp)| *exp > now).map(|(s, _)| s).collect();
        let lease = match live.len() {
            0 => "none yet".to_string(),
            1 => format!("1 active ({})", live[0]),
            n => format!("{n} active"),
        };
        ApprovalView {
            tool: card.tool.clone(),
            cap,
            class: card.class,
            args: compact(&card.args, 160),
            why: card.rendered.clone(),
            reason: reason_label(&card.reason),
            world: card.world.name.clone(),
            enforced_by: card.world.enforced_by.clone(),
            lease,
            epoch_next: state.policy.epoch.0.max(1) + 1,
            cut: card.suggested_cut.clone(),
        }
    }

    /// `tool-vm.vm_start`
    pub fn title(&self) -> String {
        match &self.cap {
            Some(c) => format!("{c}.{}", self.tool),
            None => self.tool.clone(),
        }
    }

    /// The preset a `[p]` press would switch to, when the smallest cut is a preset change.
    pub fn preset_cut(&self) -> Option<&str> {
        self.cut.as_deref().and_then(|c| c.strip_prefix("preset ")).map(str::trim)
    }
}

pub fn reason_label(r: &DecisionReason) -> &'static str {
    match r {
        DecisionReason::Rule { .. } => "Rule",
        DecisionReason::Preset(_) => "Preset",
        DecisionReason::Hook(_) => "Hook",
        DecisionReason::Guard(_) => "Guard",
        DecisionReason::SafetyCheck(_) => "SafetyCheck",
        DecisionReason::Classifier => "Classifier",
        DecisionReason::WorkingDir => "WorkingDir",
        DecisionReason::AsyncAgent => "AsyncAgent",
        DecisionReason::Delegation => "Delegation",
        DecisionReason::Escalation => "Escalation",
        DecisionReason::Unavailable => "Unavailable",
        DecisionReason::Passthrough => "Passthrough",
        DecisionReason::ToolCheck(_) => "ToolCheck",
    }
}

/// `{ name: "ubuntu-24" }`: keys bare, values JSON, cut at `max` chars.
pub fn compact(v: &Value, max: usize) -> String {
    fn go(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) if m.is_empty() => out.push_str("{}"),
            Value::Object(m) => {
                out.push_str("{ ");
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(k);
                    out.push_str(": ");
                    go(x, out);
                }
                out.push_str(" }");
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    go(v, &mut s);
    let s = s.replace('\n', "⏎");
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        s
    }
}

// ───────────────────────────── rewind picker ─────────────────────────────

pub const MODES: [RewindMode; 3] = [RewindMode::Both, RewindMode::ChatOnly, RewindMode::CodeOnly];

pub fn mode_name(m: RewindMode) -> &'static str {
    match m {
        RewindMode::Both => "both",
        RewindMode::ChatOnly => "chat",
        RewindMode::CodeOnly => "code",
    }
}

/// What each rewind mode does, one line per mode.
pub fn mode_doc(m: RewindMode) -> &'static str {
    match m {
        RewindMode::Both => "fork the log here and restore the files",
        RewindMode::ChatOnly => "fork the log here; files untouched",
        RewindMode::CodeOnly => "restore the files; same conversation, the model is told",
    }
}

/// The effect of a rewind, computed before anything is applied (plan-then-apply).
#[derive(Clone, Debug, PartialEq)]
pub struct RewindPlan {
    pub checkpoint: CheckpointId,
    pub mode: RewindMode,
    pub fork_at: Option<EntryId>,
    /// `(changed, deleted)` when files are restored.
    pub restore: Option<(Vec<String>, Vec<String>)>,
    /// Running operations that are killed first.
    pub kill: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RewindPicker {
    pub checkpoint: CheckpointId,
    pub entry: EntryId,
    pub selected: RewindMode,
    pub plan: Option<Result<RewindPlan, String>>,
}

// ───────────────────────────── ui state ─────────────────────────────

/// A live view a panel draws from the fold on every frame (so it updates while open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Commands, views and keys.
    Help,
    Tree,
    Inventory,
    Jobs,
    Invariants,
}

impl View {
    pub fn title(self) -> &'static str {
        match self {
            View::Help => "help",
            View::Tree => "tree",
            View::Inventory => "inventory",
            View::Jobs => "jobs",
            View::Invariants => "invariants",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PanelBody {
    View(View),
    /// A command's multi-line answer.
    Text(Vec<String>),
}

/// A read-only panel over the transcript; Esc closes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub title: String,
    pub body: PanelBody,
    pub scroll: usize,
    pub tone: Tone,
}

impl Panel {
    pub fn view(v: View) -> Panel {
        Panel { title: v.title().into(), body: PanelBody::View(v), scroll: 0, tone: Tone::Info }
    }
    pub fn text(title: impl Into<String>, text: &str, tone: Tone) -> Panel {
        Panel { title: title.into(), body: PanelBody::Text(text.lines().map(str::to_string).collect()), scroll: 0, tone }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointItem {
    pub id: CheckpointId,
    pub entry: EntryId,
    pub label: String,
}

/// `/rewind`: the checkpoints of the current branch, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointList {
    pub items: Vec<CheckpointItem>,
    pub selected: usize,
}

/// What a command opened in front of the transcript. The top of the stack gets the keys.
#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    Panel(Panel),
    Checkpoints(CheckpointList),
    Rewind(RewindPicker),
}

impl Overlay {
    pub fn name(&self) -> &'static str {
        match self {
            Overlay::Panel(_) => "panel",
            Overlay::Checkpoints(_) => "checkpoints",
            Overlay::Rewind(_) => "rewind",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Info,
    Warn,
    Error,
}

/// A one-line status message; it clears on the next key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub tone: Tone,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Notice {
        Notice { text: text.into(), tone: Tone::Info }
    }
    pub fn warn(text: impl Into<String>) -> Notice {
        Notice { text: text.into(), tone: Tone::Warn }
    }
    pub fn error(text: impl Into<String>) -> Notice {
        Notice { text: text.into(), tone: Tone::Error }
    }
}

/// Surface-local state: the only thing the TUI holds that the log does not.
#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub draft: String,
    /// Cursor in `draft`, in chars.
    pub cursor: usize,
    /// Transcript lines scrolled up from the tail (0 = follow); full-screen mode only.
    pub scroll: usize,
    pub hud: bool,
    /// Whether model reasoning is visible. Default on; `/thinking` changes it for this run.
    pub show_thinking: bool,
    /// The highlighted row of the `/` menu.
    pub menu_selected: usize,
    /// Esc closed the menu for this draft; editing reopens it.
    pub menu_dismissed: bool,
    /// The configurable keys (`tui.keys.*`).
    pub keys: Keymap,
}

impl Default for Local {
    fn default() -> Self {
        Local {
            draft: String::new(),
            cursor: 0,
            scroll: 0,
            hud: false,
            show_thinking: true,
            menu_selected: 0,
            menu_dismissed: false,
            keys: Keymap::default(),
        }
    }
}

/// Plan facts the header shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanInfo {
    pub label: String,
    pub hash: String,
    pub world: String,
    pub enforced_by: String,
    /// The origin of `policy.preset` (HUD).
    pub preset_origin: String,
    /// The model as the status line names it: its catalog name, else its id.
    pub model: String,
    /// The model's list price on its route; `None` (not in the catalog) shows token counts.
    pub price: Option<Price>,
    /// The context budget (`model.context_window`, or the model catalog's window: the session's
    /// own `SessionConfig` value) and the ratio of it compaction starts at.
    pub context_window: u64,
    pub compact_at: f64,
    /// The workspace as the status line shows it (`~/…`); empty hides it.
    pub workspace: String,
}

#[derive(Clone, Debug, Default)]
pub struct UiState {
    pub plan: PlanInfo,
    pub state: State,
    pub transcript: Transcript,
    pub tree: Vec<TreeNode>,
    pub inventory: Vec<InvCard>,
    /// Every op of the session (`OpTable::list`); set through [`UiState::set_jobs`].
    pub jobs: Vec<OpInfo>,
    /// What the jobs panel names each running job by (`""`: nothing to name it by), from its
    /// call's arguments in the fold; kept for the running jobs only.
    pub job_labels: BTreeMap<OpId, String>,
    /// The approval card: above every overlay, and answered first.
    pub approval: Option<ApprovalView>,
    /// Cards waiting behind the one shown.
    pub approvals_queued: usize,
    /// Panels and pickers opened by commands; the last one is on top.
    pub overlays: Vec<Overlay>,
    /// A prompt this surface started is still running.
    pub busy: bool,
    pub notice: Option<Notice>,
    pub tool_caps: BTreeMap<String, String>,
    /// The app's slash commands (for the menu and `/help`).
    pub commands: Vec<CommandInfo>,
    pub theme: Theme,
    /// Tokens every model answer in the session log reported, all branches (the spend).
    pub spent: Usage,
    /// Tokens in the context at the last model answer on this branch; `None` before the first
    /// answer and after compaction replaced part of the context (until the next answer).
    pub context_used: Option<u64>,
    pub local: Local,
}

impl UiState {
    /// Builds the projection-only part from the plan; the log part is folded by [`UiState::refold`].
    /// Refuses a `tui.keys.*` value that is not a key spec.
    pub fn new(label: &str, plan: &Plan, loadout: &Loadout, tool_caps: BTreeMap<String, String>) -> Result<UiState, String> {
        let keys = Keymap::from_loadout(loadout).map_err(|e| e.to_string())?;
        let mut ui = UiState {
            plan: PlanInfo {
                label: label.to_string(),
                hash: plan.hash.short(),
                world: plan.world.name.clone(),
                enforced_by: plan.world.enforced_by.clone(),
                preset_origin: loadout.config.get("policy.preset").map(|s| s.winner.render()).unwrap_or_else(|| "default".into()),
                ..PlanInfo::default()
            },
            tool_caps,
            local: Local { show_thinking: loadout.get("tui.thinking").and_then(|v| v.as_bool()).unwrap_or(true), keys, ..Local::default() },
            ..UiState::default()
        };
        ui.refresh_tree(plan, loadout);
        ui.model_facts(&aho_session::SessionConfig::from_loadout(loadout, std::path::PathBuf::new()));
        Ok(ui)
    }

    /// The status line's model name, list price and context budget, from the config the session
    /// runs with.
    pub fn model_facts(&mut self, cfg: &aho_session::SessionConfig) {
        let listed = aho_llm::models::lookup(&cfg.provider, &cfg.model);
        self.plan.model = listed.map_or_else(|| cfg.model.clone(), |m| m.name.to_string());
        self.plan.price = listed.map(|m| m.price);
        self.plan.context_window = cfg.context_window;
        self.plan.compact_at = cfg.compaction_threshold;
    }

    /// Tree and inventory: the plan's nodes governed by resolved config, with runtime state
    /// changes from the fold (a capability broken or disabled mid-session) applied.
    pub fn refresh_tree(&mut self, plan: &Plan, loadout: &Loadout) {
        let mut nodes = aho_inspect::tree(plan);
        let config = if self.state.config.is_empty() { &loadout.config } else { &self.state.config };
        aho_inspect::govern(&mut nodes, config);
        for n in nodes.iter_mut() {
            if let Some(st) = self.state.caps.get(&n.id)
                && matches!(n.state, CapState::Active)
                && !matches!(st, CapState::Active)
            {
                n.state = st.clone();
            }
        }
        self.inventory = aho_inspect::inventory_in(&nodes, plan, None);
        self.tree = nodes;
    }

    /// Refolds the state and the transcript from a branch.
    pub fn refold(&mut self, branch: &[Entry], folds: &CustomFolds) -> Result<(), String> {
        self.transcript = Transcript::from_entries(branch, &self.tool_caps);
        self.state = aho_log::fold(branch, folds).map_err(|e| format!("the session log does not fold: {e}"))?;
        self.spent = self.state.usage;
        self.context_used = None;
        for e in branch {
            self.meter_context(e);
        }
        Ok(())
    }

    fn meter_context(&mut self, e: &Entry) {
        match &e.kind {
            EntryKind::AssistantMessage { usage, .. } => {
                self.context_used = Some(usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens + usage.output_tokens)
            }
            EntryKind::SurfaceReplace { .. } => self.context_used = None,
            _ => {}
        }
    }

    /// Folds one entry of the current branch. `Err` means the fold failed; refold from the log.
    pub fn apply(&mut self, e: &Entry, folds: &CustomFolds) -> Result<(), String> {
        self.transcript.apply(e, &self.tool_caps);
        let prev = std::mem::take(&mut self.state);
        self.state = aho_log::step(prev, e, folds).map_err(|err| format!("entry {} does not fold: {err}", e.id.0))?;
        if let EntryKind::AssistantMessage { usage, .. } = &e.kind {
            self.spent = self.spent + *usage;
        }
        self.meter_context(e);
        Ok(())
    }

    /// Replaces the ops list; labels are looked up (in the fold's calls) only when it changed and
    /// only for running jobs not labelled yet.
    pub fn set_jobs(&mut self, jobs: Vec<OpInfo>) {
        if jobs == self.jobs {
            return;
        }
        let running = |j: &OpInfo| j.detached && j.status.is_none();
        self.job_labels.retain(|op, _| jobs.iter().any(|j| j.op == *op && running(j)));
        for j in jobs.iter().filter(|j| running(j)) {
            if !self.job_labels.contains_key(&j.op) {
                let label = self.state.calls.get(&j.call).and_then(|(_, _, args)| crate::jobs::label(args)).unwrap_or_default();
                self.job_labels.insert(j.op, label);
            }
        }
        self.jobs = jobs;
    }

    /// Whether anything covers the transcript (then the full screen is drawn).
    pub fn covered(&self) -> bool {
        self.approval.is_some() || !self.overlays.is_empty()
    }

    /// The checkpoints of this branch as `/rewind` lists them, newest first.
    pub fn checkpoint_items(&self) -> Vec<CheckpointItem> {
        let mut items: Vec<CheckpointItem> = self
            .transcript
            .blocks
            .iter()
            .filter_map(|b| match &b.body {
                Body::Checkpoint { id, cause, turn } => {
                    let entry = self.state.checkpoints.iter().find(|c| c.0 == *id).map_or(b.entry, |c| c.2);
                    Some(CheckpointItem { id: *id, entry, label: format!("c{} · turn {turn} · {cause}", id.0) })
                }
                _ => None,
            })
            .collect();
        items.reverse();
        items
    }

    /// Busy = a turn is open in the log, or a prompt from this surface has not returned yet.
    pub fn is_busy(&self) -> bool {
        self.busy || self.transcript.turn_open
    }

    pub fn invariant_rows(&self) -> Vec<InvariantRow> {
        invariant_rows(&self.state)
    }
}

/// Invariant verdicts against the workspace's latest checkpoint tree; with no checkpoint yet the
/// recorded verdicts are shown as recorded.
pub fn invariant_rows(state: &State) -> Vec<InvariantRow> {
    match state.checkpoints.last() {
        Some((_, tree, _)) => aho_inspect::invariants(state, *tree),
        None => state
            .invariants
            .iter()
            .map(|(id, (v, h))| InvariantRow { id: id.clone(), verdict: v.clone(), recorded: v.clone(), code_hash: *h, stale: false })
            .collect(),
    }
}

/// Tool name → owning capability, from the registry (static and runtime-registered tools).
pub fn tool_caps(reg: &Registry) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = reg.tools.iter().map(|(c, d, _)| (d.name.to_string(), c.name.to_string())).collect();
    for (c, spec, _) in &reg.dynamic_tools {
        m.insert(spec.name.clone(), c.name.to_string());
    }
    m
}

pub fn principal_label(p: &Principal) -> String {
    match p {
        Principal::Operator => "operator".into(),
        Principal::Engineer => "engineer".into(),
        Principal::Agent { id, turn } => format!("agent {:x} (turn {})", id.0, turn.0),
        Principal::System { name } => name.clone(),
    }
}
