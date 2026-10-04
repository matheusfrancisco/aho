//! The terminal loop: one interactive owner. Session events, approval cards, key presses
//! and finished background actions are the only inputs; every draw is a pure function of the fold.
//!
//! Inline by default: finished blocks go to the terminal's scrollback and only the bottom is
//! redrawn ([`InlineScreen`]); while an approval card or an overlay is in front, the full screen
//! is drawn on the alternate screen, and closing it returns to the inline view. `fullscreen`
//! keeps the whole session on the alternate screen instead.

use std::collections::VecDeque;
use std::io::Stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aho_approval::{ApprovalRequest, Decided};
use aho_compose::Planned;
use aho_fs::SnapshotHandle;
use aho_log::{CustomFolds, EntryKind, GrantScope, RewindMode, Transition};
use aho_policy::{Operator, OperatorProof};
use aho_session::{AbortScope, Session, SessionError, SessionEvent};
use aho_types::{CheckpointId, EntryId, Usage};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use tokio::sync::{broadcast, mpsc};

use crate::frames::{Frame, FrameCapture, LAYOUT_OSCILLATION, Probe};
use crate::inline::InlineScreen;
use crate::keys::{Action, on_key};
use crate::model::{ApprovalView, Notice, Overlay, Panel, RewindPlan, Tone, UiState, tool_caps};
use crate::render::draw;
use crate::slash::{Commands, Reply};
use crate::theme::Theme;

/// Frames kept for visual debugging.
pub const FRAME_RING: usize = 256;

/// Builds the screen state from the session: the fold of the current branch plus the plan's
/// tree and inventory, the live operations table, and the model facts of the status line (its
/// catalog name and price, the spend over the whole log, the context budget).
pub fn ui_from_session(session: &Session, planned: &Planned) -> Result<UiState, String> {
    let id = format!("{:x}", session.id.0);
    let short = if id.len() > 8 { format!("{}…", &id[..8]) } else { id };
    let mut ui = UiState::new(&format!("aho · session {short}"), &planned.plan, &planned.loadout, tool_caps(session.registry()))?;
    let branch = match session.log().head() {
        Some(h) => session.log().read_branch(h).map_err(|e| format!("could not read the session log: {e}"))?,
        None => Vec::new(),
    };
    ui.refold(&branch, &session.registry().entry_kinds)?;
    ui.refresh_tree(&planned.plan, &planned.loadout);
    ui.set_jobs(session.ops().list());
    ui.model_facts(session.config());
    // Abandoned branches were paid for too.
    ui.spent = session.log().read_from(None).iter().fold(Usage::default(), |sum, e| match &e.kind {
        EntryKind::AssistantMessage { usage, .. } => sum + *usage,
        _ => sum,
    });
    Ok(ui)
}

/// The effect of a rewind to `checkpoint` in `mode`. Nothing is applied; the current workspace is
/// hashed into the shadow store to diff against the target tree.
pub async fn rewind_plan(session: &Session, checkpoint: CheckpointId, mode: RewindMode) -> Result<RewindPlan, String> {
    let state = session.fold().map_err(|e| e.to_string())?;
    let (_, tree, at) = state
        .checkpoints
        .iter()
        .find(|c| c.0 == checkpoint)
        .copied()
        .ok_or_else(|| format!("checkpoint c{} is not on the current branch", checkpoint.0))?;
    let restore = if mode == RewindMode::ChatOnly {
        None
    } else {
        let snap = session
            .registry()
            .handle::<SnapshotHandle>()
            .ok_or_else(|| "this world has no Snapshot provider, so files cannot be restored; `chat` mode still works".to_string())?;
        let now = snap.0.tree().await.map_err(|e| format!("could not hash the current workspace: {e}"))?;
        let d = snap.0.diff(now, tree).await.map_err(|e| format!("could not diff against c{}: {e}", checkpoint.0))?;
        Some((d.changed, d.deleted))
    };
    let kill = session
        .ops()
        .running()
        .iter()
        .filter_map(|(op, _, _)| session.ops().info(*op))
        .map(|i| format!("op{} {}", i.op.0, i.tool))
        .collect();
    Ok(RewindPlan { checkpoint, mode, fork_at: (mode != RewindMode::CodeOnly).then_some(at), restore, kill })
}

enum Done {
    Turn(Result<Transition, String>),
    Steered,
    Rewound(Result<EntryId, String>),
    Plan(CheckpointId, RewindMode, Result<RewindPlan, String>),
}

/// How `aho tui` uses the terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// The whole session on the alternate screen (the transcript scrolls with PgUp/PgDn) instead
    /// of inline in the terminal's scrollback.
    pub fullscreen: bool,
    /// The colours; the app detects them from the terminal ([`Theme::detect`]).
    pub theme: Theme,
    /// The workspace as the status line shows it (`~/…`); empty hides it.
    pub workspace: String,
}

type Crossterm = CrosstermBackend<Stdout>;

enum Screen {
    Full(Terminal<Crossterm>),
    Inline { inline: InlineScreen<Crossterm>, alt: Option<Terminal<Crossterm>> },
}

/// Raw mode and bracketed paste for the whole run; the alternate screen only in full-screen mode.
/// Dropping it restores the terminal whatever happened.
struct TerminalGuard {
    alt: bool,
}

impl TerminalGuard {
    fn enter(fullscreen: bool) -> Result<TerminalGuard, String> {
        enable_raw_mode()
            .map_err(|e| format!("could not switch the terminal to raw mode ({e}); run aho in an interactive terminal or use `aho run`"))?;
        let mut out = std::io::stdout();
        let r = if fullscreen { execute!(out, EnterAlternateScreen, EnableBracketedPaste) } else { execute!(out, EnableBracketedPaste) };
        if let Err(e) = r {
            let _ = disable_raw_mode();
            return Err(format!("could not prepare the terminal: {e}"));
        }
        Ok(TerminalGuard { alt: fullscreen })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        if self.alt {
            let _ = execute!(out, LeaveAlternateScreen);
        }
        let _ = execute!(out, DisableBracketedPaste, ratatui::crossterm::cursor::Show);
    }
}

/// Reads the terminal on its own thread with a short poll, so the reader lock is free between
/// polls (an inline viewport asks the terminal for the cursor position when it opens or resizes;
/// a reader blocked in an endless poll would hold that answer until the next key).
fn spawn_reader(stop: Arc<AtomicBool>) -> (mpsc::UnboundedReceiver<std::io::Result<Event>>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    let ev = event::read();
                    let failed = ev.is_err();
                    if tx.send(ev).is_err() || failed {
                        return;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    (rx, handle)
}

struct Runtime {
    session: Arc<Session>,
    planned: Planned,
    ui: UiState,
    folds: CustomFolds,
    queue: VecDeque<ApprovalRequest>,
    operator: Arc<Operator>,
    done: mpsc::UnboundedSender<Done>,
    commands: Commands,
    frames: FrameCapture,
    quit: bool,
    /// The transcript was replaced since the last draw (the inline screen re-anchors).
    refolded: bool,
}

/// Rows the inline viewport opens with (the prompt box plus the hint row).
const INLINE_START: u16 = 4;

/// Runs the TUI until the operator quits (ctrl-q, `/quit`, or ctrl-d on an empty draft) or the
/// session closes. `approvals` is the receiver of the `ChannelApproval` the session was booted
/// with; `commands` answers the `/name args…` lines typed in the prompt box.
pub async fn run(
    session: Arc<Session>,
    mut approvals: mpsc::Receiver<ApprovalRequest>,
    planned: Planned,
    commands: Commands,
    options: RunOptions,
) -> Result<(), String> {
    let mut events = session.subscribe();
    let mut ui = ui_from_session(&session, &planned)?;
    ui.commands = commands.info();
    ui.theme = options.theme.clone();
    ui.plan.workspace = options.workspace.clone();
    let (done, mut done_rx) = mpsc::unbounded_channel();
    let mut rt = Runtime {
        folds: session.registry().entry_kinds.clone(),
        session,
        planned,
        ui,
        queue: VecDeque::new(),
        operator: Arc::new(Operator::authenticate(OperatorProof::local_tty())),
        done,
        commands,
        frames: FrameCapture::new(FRAME_RING),
        quit: false,
        refolded: false,
    };
    let guard = TerminalGuard::enter(options.fullscreen)?;
    let term_err = |e: std::io::Error| format!("could not start the terminal: {e}");
    let mut screen = if options.fullscreen {
        Screen::Full(Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(term_err)?)
    } else {
        Screen::Inline { inline: InlineScreen::new(CrosstermBackend::new(std::io::stdout()), INLINE_START).map_err(term_err)?, alt: None }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (mut keys, reader) = spawn_reader(stop.clone());
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut approvals_open = true;
    let result = loop {
        if let Err(e) = rt.draw(&mut screen) {
            break Err(e);
        }
        if rt.quit {
            break Ok(());
        }
        tokio::select! {
            k = keys.recv() => match k {
                Some(Ok(Event::Key(k))) => rt.key(k),
                Some(Ok(Event::Paste(text))) => rt.paste(&text),
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(format!("reading the terminal failed: {e}")),
                None => break Ok(()),
            },
            ev = events.recv() => match ev {
                Ok(ev) => rt.session_event(ev),
                Err(broadcast::error::RecvError::Lagged(_)) => rt.refold_head(),
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
            req = approvals.recv(), if approvals_open => match req {
                Some(r) => rt.enqueue(r),
                None => approvals_open = false,
            },
            Some(d) = done_rx.recv() => rt.finished(d),
            _ = tick.tick() => rt.tick(),
        }
        if let Screen::Inline { inline, .. } = &mut screen
            && std::mem::take(&mut rt.refolded)
        {
            inline.reanchor(&rt.ui);
        }
    };
    stop.store(true, Ordering::Release);
    let _ = reader.join();
    for (_, reply) in rt.queue.drain(..) {
        let _ = reply.send(Decided::cancelled());
    }
    // Leave the transcript in the scrollback and the shell prompt under it.
    if let Screen::Inline { inline, alt } = &mut screen {
        if alt.take().is_some() {
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        }
        let _ = inline.finish();
    }
    drop(screen);
    drop(guard);
    result
}

/// Handles a `/name args…` line: a one-line reply is a notice, a longer one opens a panel;
/// nothing is sent to the model.
pub(crate) fn run_command(ui: &mut UiState, commands: &Commands, session: &Arc<Session>, name: &str, args: &str) {
    let reply = commands.dispatch(session, name, args);
    let tone = match &reply {
        Reply::Output(_) => Tone::Info,
        Reply::Unknown(_) => Tone::Warn,
        Reply::Failed(_) => Tone::Error,
    };
    let text = reply.text();
    if text.contains('\n') {
        ui.overlays.push(Overlay::Panel(Panel::text(format!("/{name}"), text, tone)));
    } else {
        ui.notice = Some(Notice { text: text.to_string(), tone });
    }
}

impl Runtime {
    fn draw(&mut self, screen: &mut Screen) -> Result<(), String> {
        let started = Instant::now();
        let mut probe = Probe::default();
        let ui = &self.ui;
        let failed = |e: std::io::Error| format!("drawing the terminal failed: {e}");
        let full = |term: &mut Terminal<Crossterm>, probe: &mut Probe| -> std::io::Result<ratatui::buffer::Buffer> {
            let done = term.draw(|f| {
                let area = f.area();
                if let Some(pos) = draw(ui, area, f.buffer_mut(), probe) {
                    f.set_cursor_position(pos);
                }
            })?;
            Ok(done.buffer.clone())
        };
        let buffer = match screen {
            Screen::Full(term) => full(term, &mut probe).map_err(failed)?,
            Screen::Inline { inline, alt } => {
                if ui.covered() {
                    if alt.is_none() {
                        execute!(std::io::stdout(), EnterAlternateScreen).map_err(failed)?;
                        let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(failed)?;
                        // A new terminal assumes a blank screen; not every emulator blanks the
                        // alternate one, so clear it before the first frame.
                        term.clear().map_err(failed)?;
                        *alt = Some(term);
                    }
                    let term = alt.as_mut().ok_or_else(|| "the overlay screen is gone".to_string())?;
                    full(term, &mut probe).map_err(failed)?
                } else {
                    if alt.take().is_some() {
                        execute!(std::io::stdout(), LeaveAlternateScreen).map_err(failed)?;
                        inline.invalidate().map_err(failed)?;
                    }
                    inline.draw(ui, &mut probe).map_err(failed)?
                }
            }
        };
        let frame = Frame::from_buffer(&buffer, probe, started.elapsed().as_millis() as u64);
        if let Some(kept) = self.frames.record(frame)
            && let Some(a) = kept.anomalies.iter().find(|a| a.starts_with(LAYOUT_OSCILLATION))
        {
            self.ui.notice =
                Some(Notice::warn(format!("frame {}: {a}; the screen is flickering. Copy this line into a bug report.", kept.seq)));
        }
        Ok(())
    }

    fn refold_head(&mut self) {
        match crate::ui_from_session(&self.session, &self.planned) {
            Ok(mut fresh) => {
                fresh.local = std::mem::take(&mut self.ui.local);
                fresh.approval = self.ui.approval.take();
                fresh.approvals_queued = self.ui.approvals_queued;
                fresh.overlays = std::mem::take(&mut self.ui.overlays);
                fresh.busy = self.ui.busy;
                fresh.notice = self.ui.notice.take();
                fresh.commands = std::mem::take(&mut self.ui.commands);
                fresh.theme = self.ui.theme.clone();
                fresh.plan.workspace = std::mem::take(&mut self.ui.plan.workspace);
                self.ui = fresh;
                self.refolded = true;
            }
            Err(e) => self.ui.notice = Some(Notice::error(e)),
        }
    }

    fn session_event(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::Entry(e) => {
                if self.ui.transcript.already_seen(&e) {
                    return;
                }
                if !self.ui.transcript.continues(&e) {
                    self.refold_head();
                    return;
                }
                if let Err(err) = self.ui.apply(&e, &self.folds) {
                    self.ui.notice = Some(Notice::error(err));
                    self.refold_head();
                    return;
                }
                match &e.kind {
                    EntryKind::CapDelta { .. } | EntryKind::ConfigResolved { .. } | EntryKind::PolicyEpoch { .. } => {
                        self.ui.refresh_tree(&self.planned.plan, &self.planned.loadout)
                    }
                    EntryKind::OpStarted { .. }
                    | EntryKind::OpDone { .. }
                    | EntryKind::ToolResult { .. }
                    | EntryKind::OpDetached { .. }
                    | EntryKind::JobDelivered { .. } => self.ui.set_jobs(self.session.ops().list()),
                    _ => {}
                }
            }
            SessionEvent::Stream(s) => self.ui.transcript.stream(&s),
            SessionEvent::Idle { turn, transition } => {
                self.ui.transcript.clear_live();
                if transition != Transition::Completed {
                    self.ui.notice = Some(Notice::warn(format!("turn {} ended: {transition:?}", turn.0)));
                }
            }
            SessionEvent::Error(msg) => self.ui.notice = Some(Notice::error(msg)),
            SessionEvent::JobFinished { op } => {
                self.ui.set_jobs(self.session.ops().list());
                // `jobs.wake = operator`: no turn starts by itself, so the operator is told.
                if self.session.jobs_config().wake == aho_session::WakeMode::Operator {
                    self.ui.notice =
                        Some(Notice::info(format!("job op{} finished; its outcome reaches the model with your next prompt", op.0)));
                }
            }
        }
    }

    fn enqueue(&mut self, req: ApprovalRequest) {
        self.queue.push_back(req);
        self.show_front();
    }

    fn show_front(&mut self) {
        self.ui.approval = self.queue.front().map(|(card, _)| {
            let cap = self.ui.tool_caps.get(&card.tool).cloned();
            ApprovalView::new(card, cap, &self.ui.state)
        });
        self.ui.approvals_queued = self.queue.len().saturating_sub(1);
    }

    fn answer(&mut self, d: Decided) {
        if let Some((card, reply)) = self.queue.pop_front()
            && reply.send(d).is_err()
        {
            self.ui.notice = Some(Notice::warn(format!("the session stopped waiting for the `{}` card; nothing was decided", card.tool)));
        }
        self.show_front();
    }

    fn paste(&mut self, text: &str) {
        if self.ui.covered() {
            return;
        }
        let l = &mut self.ui.local;
        let at = l.draft.char_indices().nth(l.cursor).map_or(l.draft.len(), |(i, _)| i);
        let clean: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        l.draft.insert_str(at, &clean);
        l.cursor += clean.chars().count();
    }

    fn key(&mut self, k: ratatui::crossterm::event::KeyEvent) {
        self.ui.notice = None;
        let Some(action) = on_key(&mut self.ui, k) else { return };
        match action {
            Action::Quit => self.quit = true,
            Action::Abort(scope) => self.abort(scope),
            Action::Prompt(text) => {
                self.ui.busy = true;
                let s = self.session.clone();
                let done = self.done.clone();
                tokio::spawn(async move {
                    let r = match s.prompt(text.clone(), None).await {
                        Err(SessionError::Busy) => {
                            s.steer(text, false);
                            let _ = done.send(Done::Steered);
                            return;
                        }
                        r => r.map_err(|e| e.to_string()),
                    };
                    let _ = done.send(Done::Turn(r));
                });
            }
            Action::Command { name, args } => {
                run_command(&mut self.ui, &self.commands, &self.session, &name, &args);
                // A command writes the log directly, not through the turn loop, so no session
                // event announces it: refold to show what it changed (policy, plan mode).
                self.refold_head();
            }
            Action::Steer { text, urgent } => {
                self.session.steer(text, urgent);
                self.ui.notice = Some(Notice::info(if urgent {
                    "urgent steer queued for the next request"
                } else {
                    "steer queued for the next request"
                }));
            }
            Action::Answer(d) => self.answer(d),
            Action::PresetCut(preset) => {
                let env = &self.planned.env;
                let file = aho_compose::apply_target(std::path::Path::new(&env.project), std::path::Path::new(&env.home));
                match aho_compose::apply(&file, "policy.preset", serde_json::json!(preset)) {
                    Ok(_) => {
                        let class = self.ui.approval.as_ref().map(|a| a.class.name()).unwrap_or("this class");
                        self.ui.notice = Some(Notice::info(format!(
                            "wrote policy.preset = \"{preset}\" to {}; new sessions start with it. This session now allows {class}.",
                            file.display()
                        )));
                        self.answer(Decided::allowed(GrantScope::Session));
                    }
                    Err(e) => self.ui.notice = Some(Notice::error(format!("could not write the preset: {e}. The card is still open."))),
                }
            }
            Action::PlanRewind { checkpoint, mode } => {
                let s = self.session.clone();
                let done = self.done.clone();
                tokio::spawn(async move {
                    let plan = rewind_plan(&s, checkpoint, mode).await;
                    let _ = done.send(Done::Plan(checkpoint, mode, plan));
                });
            }
            Action::Rewind { checkpoint, mode } => {
                let s = self.session.clone();
                let op = self.operator.clone();
                let done = self.done.clone();
                self.ui.notice = Some(Notice::info(format!("rewinding to c{}…", checkpoint.0)));
                tokio::spawn(async move {
                    let r = s.rewind(&op, checkpoint, mode).await.map_err(|e| e.to_string());
                    let _ = done.send(Done::Rewound(r));
                });
            }
        }
    }

    fn finished(&mut self, d: Done) {
        match d {
            Done::Turn(r) => {
                self.ui.busy = false;
                if let Err(e) = r {
                    self.ui.notice = Some(Notice::error(format!("the turn failed: {e}")));
                }
            }
            Done::Steered => {
                self.ui.busy = false;
                self.ui.notice = Some(Notice::info("a turn was already running; your text was queued as a steer"));
            }
            Done::Rewound(Ok(id)) => {
                self.ui.notice = Some(Notice::info(format!("rewound (entry {}); the old branch is kept", id.0)));
                self.refold_head();
            }
            Done::Rewound(Err(e)) => self.ui.notice = Some(Notice::error(format!("rewind failed: {e}"))),
            Done::Plan(cp, mode, plan) => {
                for o in self.ui.overlays.iter_mut().rev() {
                    if let Overlay::Rewind(p) = o
                        && p.checkpoint == cp
                        && p.selected == mode
                    {
                        p.plan = Some(plan);
                        break;
                    }
                }
            }
        }
    }

    /// Cancels by scope and says what was cancelled and what keeps running.
    fn abort(&mut self, scope: AbortScope) {
        self.ui.set_jobs(self.session.ops().list());
        let jobs = crate::jobs::running(&self.ui).count();
        let cancelled = self.session.abort_scope(scope);
        let text = match scope {
            AbortScope::Turn if jobs > 0 => {
                format!("abort: this turn's running operations are being cancelled; {jobs} background job(s) keep running")
            }
            AbortScope::Turn => "abort: this turn's running operations are being cancelled".to_string(),
            AbortScope::All => format!("abort all: the turn and {jobs} background job(s) are being cancelled"),
            AbortScope::Op(op) if cancelled => format!("abort: op{} is being cancelled", op.0),
            AbortScope::Op(op) => format!("op{} is not running; nothing was cancelled", op.0),
        };
        self.ui.notice = Some(Notice::info(text));
    }

    fn tick(&mut self) {
        self.ui.set_jobs(self.session.ops().list());
        let before = self.queue.len();
        self.queue.retain(|(_, reply)| !reply.is_closed());
        if self.queue.len() != before {
            self.ui.notice = Some(Notice::warn("an approval card was withdrawn: the call it asked about is no longer waiting"));
            self.show_front();
        }
    }
}
