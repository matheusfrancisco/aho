//! The terminal loop: one interactive owner (12). Session events, approval cards, key presses
//! and finished background actions are the only inputs; every draw is [`draw`] over the fold.

use std::collections::VecDeque;
use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_approval::{ApprovalRequest, Decided};
use aho_compose::Planned;
use aho_fs::SnapshotHandle;
use aho_log::{CustomFolds, EntryKind, GrantScope, RewindMode, Transition};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Session, SessionError, SessionEvent};
use aho_types::{CheckpointId, EntryId};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use tokio::sync::{broadcast, mpsc};

use crate::frames::{Frame, FrameCapture, LAYOUT_OSCILLATION, Probe};
use crate::keys::{Action, on_key};
use crate::model::{ApprovalView, RewindPlan, UiState, tool_caps};
use crate::render::draw;
use crate::slash::Commands;

/// Frames kept for visual debugging.
pub const FRAME_RING: usize = 256;

/// Builds the screen state from the session: the fold of the current branch plus the plan's
/// tree and inventory and the live operations table.
pub fn ui_from_session(session: &Session, planned: &Planned) -> Result<UiState, String> {
    let id = format!("{:x}", session.id.0);
    let short = if id.len() > 8 { format!("{}…", &id[..8]) } else { id };
    let mut ui = UiState::new(&format!("aho · session {short}"), &planned.plan, &planned.loadout, tool_caps(session.registry()));
    let branch = match session.log().head() {
        Some(h) => session.log().read_branch(h).map_err(|e| format!("could not read the session log: {e}"))?,
        None => Vec::new(),
    };
    ui.refold(&branch, &session.registry().entry_kinds)?;
    ui.refresh_tree(&planned.plan, &planned.loadout);
    ui.jobs = session.ops().list();
    Ok(ui)
}

/// What a rewind to `checkpoint` in `mode` would do. Nothing is applied; the current workspace is
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

struct TerminalGuard {
    term: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<TerminalGuard, String> {
        enable_raw_mode().map_err(|e| {
            format!("could not switch the terminal to raw mode ({e}); run aho in an interactive terminal or use `--surface print`")
        })?;
        let mut out = std::io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(format!("could not open the alternate screen: {e}"));
        }
        let term = Terminal::new(CrosstermBackend::new(out)).map_err(|e| format!("could not start the terminal: {e}"))?;
        Ok(TerminalGuard { term })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.term.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste);
        let _ = self.term.show_cursor();
    }
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
}

/// Runs the TUI until the operator quits (ctrl-q, or ctrl-d on an empty draft) or the session
/// closes. `approvals` is the receiver of the `ChannelApproval` the session was booted with;
/// `commands` answers the `/name args…` lines typed in the prompt box.
pub async fn run(
    session: Arc<Session>,
    mut approvals: mpsc::Receiver<ApprovalRequest>,
    planned: Planned,
    commands: Commands,
) -> Result<(), String> {
    let mut events = session.subscribe();
    let ui = ui_from_session(&session, &planned)?;
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
    };
    let mut guard = TerminalGuard::enter()?;
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut approvals_open = true;
    loop {
        rt.draw(&mut guard.term)?;
        if rt.quit {
            break;
        }
        tokio::select! {
            k = keys.next() => match k {
                Some(Ok(Event::Key(k))) => rt.key(k),
                Some(Ok(Event::Paste(text))) => rt.paste(&text),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("reading the terminal failed: {e}")),
                None => break,
            },
            ev = events.recv() => match ev {
                Ok(ev) => rt.session_event(ev),
                Err(broadcast::error::RecvError::Lagged(_)) => rt.refold_head(),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            req = approvals.recv(), if approvals_open => match req {
                Some(r) => rt.enqueue(r),
                None => approvals_open = false,
            },
            Some(d) = done_rx.recv() => rt.finished(d),
            _ = tick.tick() => rt.tick(),
        }
    }
    for (_, reply) in rt.queue.drain(..) {
        let _ = reply.send(Decided::cancelled());
    }
    Ok(())
}

/// What the terminal does with a `/name args…` line: the reply goes to the status line and nothing
/// is sent to the model.
pub(crate) fn run_command(ui: &mut UiState, commands: &Commands, session: &Arc<Session>, name: &str, args: &str) {
    ui.notice = Some(commands.dispatch(session, name, args).text().to_string());
}

impl Runtime {
    fn draw(&mut self, term: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), String> {
        let started = Instant::now();
        let mut probe = Probe::default();
        let ui = &self.ui;
        let completed = term
            .draw(|f| {
                let area = f.area();
                if let Some(pos) = draw(ui, area, f.buffer_mut(), &mut probe) {
                    f.set_cursor_position(pos);
                }
            })
            .map_err(|e| format!("drawing the terminal failed: {e}"))?;
        let frame = Frame::from_buffer(completed.buffer, probe, started.elapsed().as_millis() as u64);
        if let Some(kept) = self.frames.record(frame)
            && let Some(a) = kept.anomalies.iter().find(|a| a.starts_with(LAYOUT_OSCILLATION))
        {
            self.ui.notice = Some(format!("frame {}: {a}; the screen is flickering. Copy this line into a bug report.", kept.seq));
        }
        Ok(())
    }

    fn refold_head(&mut self) {
        match crate::ui_from_session(&self.session, &self.planned) {
            Ok(mut fresh) => {
                fresh.local = std::mem::take(&mut self.ui.local);
                fresh.approval = self.ui.approval.take();
                fresh.approvals_queued = self.ui.approvals_queued;
                fresh.rewind = self.ui.rewind.take();
                fresh.busy = self.ui.busy;
                fresh.notice = self.ui.notice.take();
                fresh.clamp_selection();
                self.ui = fresh;
            }
            Err(e) => self.ui.notice = Some(e),
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
                    self.ui.notice = Some(err);
                    self.refold_head();
                    return;
                }
                match &e.kind {
                    EntryKind::CapDelta { .. } | EntryKind::ConfigResolved { .. } | EntryKind::PolicyEpoch { .. } => {
                        self.ui.refresh_tree(&self.planned.plan, &self.planned.loadout)
                    }
                    EntryKind::OpStarted { .. } | EntryKind::OpDone { .. } | EntryKind::ToolResult { .. } => {
                        self.ui.jobs = self.session.ops().list()
                    }
                    _ => {}
                }
            }
            SessionEvent::Stream(s) => self.ui.transcript.stream(&s),
            SessionEvent::Idle { turn, transition } => {
                self.ui.transcript.clear_live();
                if transition != Transition::Completed {
                    self.ui.notice = Some(format!("turn {} ended: {transition:?}", turn.0));
                }
            }
            SessionEvent::Error(msg) => self.ui.notice = Some(msg),
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
            self.ui.notice = Some(format!("the session stopped waiting for the `{}` card; nothing was decided", card.tool));
        }
        self.show_front();
    }

    fn paste(&mut self, text: &str) {
        if self.ui.approval.is_some() || self.ui.rewind.is_some() {
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
            Action::Abort => {
                self.session.abort();
                self.ui.notice = Some("abort requested: running operations of this turn are being cancelled".into());
            }
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
                self.ui.notice = Some(if urgent {
                    "urgent steer queued for the next request".into()
                } else {
                    "steer queued for the next request".into()
                });
            }
            Action::Answer(d) => self.answer(d),
            Action::PresetCut(preset) => {
                let env = &self.planned.env;
                let file = aho_compose::apply_target(std::path::Path::new(&env.project), std::path::Path::new(&env.home));
                match aho_compose::apply(&file, "policy.preset", serde_json::json!(preset)) {
                    Ok(_) => {
                        let class = self.ui.approval.as_ref().map(|a| a.class.name()).unwrap_or("this class");
                        self.ui.notice = Some(format!(
                            "wrote policy.preset = \"{preset}\" to {}; new sessions start with it. This session now allows {class}.",
                            file.display()
                        ));
                        self.answer(Decided::allowed(GrantScope::Session));
                    }
                    Err(e) => self.ui.notice = Some(format!("could not write the preset: {e}. The card is still open.")),
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
                self.ui.notice = Some(format!("rewinding to c{}…", checkpoint.0));
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
                    self.ui.notice = Some(format!("the turn failed: {e}"));
                }
            }
            Done::Steered => {
                self.ui.busy = false;
                self.ui.notice = Some("a turn was already running; your text was queued as a steer".into());
            }
            Done::Rewound(Ok(id)) => {
                self.ui.notice = Some(format!("rewound (entry {}); the old branch is kept", id.0));
                self.refold_head();
            }
            Done::Rewound(Err(e)) => self.ui.notice = Some(format!("rewind failed: {e}")),
            Done::Plan(cp, mode, plan) => {
                if let Some(p) = self.ui.rewind.as_mut()
                    && p.checkpoint == cp
                    && p.selected == mode
                {
                    p.plan = Some(plan);
                }
            }
        }
    }

    fn tick(&mut self) {
        self.ui.jobs = self.session.ops().list();
        let before = self.queue.len();
        self.queue.retain(|(_, reply)| !reply.is_closed());
        if self.queue.len() != before {
            self.ui.notice = Some("an approval card was withdrawn: the call it asked about is no longer waiting".into());
            self.show_front();
        }
    }
}
