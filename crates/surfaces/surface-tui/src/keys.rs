//! Key handling is pure: a key edits surface-local state and may yield one [`Action`] the runtime
//! performs against the session.

use aho_approval::Decided;
use aho_log::{GrantScope, RewindMode};
use aho_types::{CheckpointId, EntryId};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::model::{Body, Focus, MODES, RewindPicker, TABS, UiState};
use crate::slash::{Parsed, parse_line};

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Prompt(String),
    /// `/name args…`: run the registered command; never reaches the model.
    Command {
        name: String,
        args: String,
    },
    Steer {
        text: String,
        urgent: bool,
    },
    Abort,
    /// Answer the card on screen.
    Answer(Decided),
    /// `[p]`: write the suggested preset, then allow this class for the session.
    PresetCut(String),
    /// Compute the plan for `mode` (shown before anything is applied).
    PlanRewind {
        checkpoint: CheckpointId,
        mode: RewindMode,
    },
    Rewind {
        checkpoint: CheckpointId,
        mode: RewindMode,
    },
    Quit,
}

pub fn on_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if ctrl && key.code == KeyCode::Char('c') {
        return Some(Action::Abort);
    }
    if ctrl && key.code == KeyCode::Char('q') {
        return Some(Action::Quit);
    }
    if ui.approval.is_some() {
        return approval_key(ui, key);
    }
    if ui.rewind.is_some() {
        return rewind_key(ui, key);
    }
    if alt {
        match key.code {
            KeyCode::Char('h') => {
                ui.local.hud = !ui.local.hud;
                return None;
            }
            KeyCode::Char(c @ '1'..='4') => {
                ui.local.tab = TABS[(c as usize) - ('1' as usize)];
                ui.local.sidebar_scroll = 0;
                return None;
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Tab {
        ui.local.focus = match ui.local.focus {
            Focus::Input => Focus::Transcript,
            Focus::Transcript => Focus::Sidebar,
            Focus::Sidebar => Focus::Input,
        };
        if ui.local.focus == Focus::Transcript && ui.local.selected.is_none() {
            ui.local.selected = ui.transcript.blocks.len().checked_sub(1);
        }
        return None;
    }
    if key.code == KeyCode::BackTab {
        ui.local.focus = match ui.local.focus {
            Focus::Input => Focus::Sidebar,
            Focus::Transcript => Focus::Input,
            Focus::Sidebar => Focus::Transcript,
        };
        return None;
    }
    match ui.local.focus {
        Focus::Input => input_key(ui, key, ctrl, alt),
        Focus::Transcript => transcript_key(ui, key),
        Focus::Sidebar => sidebar_key(ui, key),
    }
}

fn approval_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    let view = ui.approval.as_ref()?;
    match key.code {
        KeyCode::Char('a') => Some(Action::Answer(Decided::allowed(GrantScope::Once))),
        KeyCode::Char('s') => Some(Action::Answer(Decided::allowed(GrantScope::Session))),
        KeyCode::Char('r') => Some(Action::Answer(Decided::rejected())),
        KeyCode::Char('p') => match view.preset_cut() {
            Some(p) => Some(Action::PresetCut(p.to_string())),
            None => {
                ui.notice = Some(match &view.cut {
                    Some(c) => format!("`{c}` is not a preset change; apply it with the aho CLI, then answer this card"),
                    None => "no single policy change would let this call through; answer with a, s, r or Esc".into(),
                });
                None
            }
        },
        KeyCode::Esc => Some(Action::Answer(Decided::cancelled())),
        _ => None,
    }
}

fn rewind_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    let picker = ui.rewind.as_mut()?;
    let idx = MODES.iter().position(|m| *m == picker.selected).unwrap_or(0);
    let select = |p: &mut RewindPicker, i: usize| {
        p.selected = MODES[i];
        p.plan = None;
        Some(Action::PlanRewind { checkpoint: p.checkpoint, mode: p.selected })
    };
    match key.code {
        KeyCode::Esc => {
            ui.rewind = None;
            None
        }
        KeyCode::Up => select(picker, (idx + MODES.len() - 1) % MODES.len()),
        KeyCode::Down => select(picker, (idx + 1) % MODES.len()),
        KeyCode::Char(c @ '1'..='3') => select(picker, (c as usize) - ('1' as usize)),
        KeyCode::Enter => match &picker.plan {
            Some(Ok(plan)) if plan.mode == picker.selected => {
                let a = Action::Rewind { checkpoint: picker.checkpoint, mode: picker.selected };
                ui.rewind = None;
                Some(a)
            }
            Some(Err(_)) => None,
            _ => {
                ui.notice = Some("the rewind plan is still being computed; press Enter again once it is shown".into());
                None
            }
        },
        _ => None,
    }
}

fn input_key(ui: &mut UiState, key: KeyEvent, ctrl: bool, alt: bool) -> Option<Action> {
    let l = &mut ui.local;
    let len = l.draft.chars().count();
    match key.code {
        KeyCode::Enter => {
            let text = l.draft.trim().to_string();
            if text.is_empty() {
                return None;
            }
            l.draft.clear();
            l.cursor = 0;
            l.scroll = 0;
            let text = match parse_line(&text) {
                Parsed::Command { name, args } => return Some(Action::Command { name: name.into(), args: args.into() }),
                Parsed::Prompt(p) => p.to_string(),
            };
            if alt {
                Some(Action::Steer { text, urgent: true })
            } else if ui.busy || ui.transcript.turn_open {
                Some(Action::Steer { text, urgent: false })
            } else {
                Some(Action::Prompt(text))
            }
        }
        KeyCode::Char('d') if ctrl && l.draft.is_empty() => Some(Action::Quit),
        KeyCode::Char('a') if ctrl => {
            l.cursor = 0;
            None
        }
        KeyCode::Char('e') if ctrl => {
            l.cursor = len;
            None
        }
        KeyCode::Char('u') if ctrl => {
            l.draft = l.draft.chars().skip(l.cursor).collect();
            l.cursor = 0;
            None
        }
        KeyCode::Char(c) if !ctrl => {
            let at = byte_at(&l.draft, l.cursor);
            l.draft.insert(at, c);
            l.cursor += 1;
            None
        }
        KeyCode::Backspace if l.cursor > 0 => {
            let at = byte_at(&l.draft, l.cursor - 1);
            l.draft.remove(at);
            l.cursor -= 1;
            None
        }
        KeyCode::Delete if l.cursor < len => {
            let at = byte_at(&l.draft, l.cursor);
            l.draft.remove(at);
            None
        }
        KeyCode::Left => {
            l.cursor = l.cursor.saturating_sub(1);
            None
        }
        KeyCode::Right => {
            l.cursor = (l.cursor + 1).min(len);
            None
        }
        KeyCode::Home => {
            l.cursor = 0;
            None
        }
        KeyCode::End => {
            l.cursor = len;
            None
        }
        KeyCode::PageUp => {
            l.scroll += 10;
            None
        }
        KeyCode::PageDown => {
            l.scroll = l.scroll.saturating_sub(10);
            None
        }
        _ => None,
    }
}

fn transcript_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    let n = ui.transcript.blocks.len();
    let l = &mut ui.local;
    match key.code {
        KeyCode::Up => {
            l.selected = Some(l.selected.map_or(n.saturating_sub(1), |s| s.saturating_sub(1)));
            None
        }
        KeyCode::Down => {
            l.selected = l.selected.map(|s| (s + 1).min(n.saturating_sub(1)));
            None
        }
        KeyCode::End => {
            l.selected = n.checked_sub(1);
            l.scroll = 0;
            None
        }
        KeyCode::PageUp => {
            l.scroll += 10;
            None
        }
        KeyCode::PageDown => {
            l.scroll = l.scroll.saturating_sub(10);
            None
        }
        KeyCode::Esc => {
            l.focus = Focus::Input;
            None
        }
        KeyCode::Enter | KeyCode::Char('r') => open_rewind(ui),
        _ => None,
    }
}

/// Enter on a checkpoint marker opens the picker with `both` preselected and asks for its plan.
pub fn open_rewind(ui: &mut UiState) -> Option<Action> {
    let (checkpoint, entry) = {
        let b = ui.selected_block()?;
        match &b.body {
            Body::Checkpoint { id, .. } => (*id, b.entry),
            _ => return None,
        }
    };
    let entry: EntryId = ui.state.checkpoints.iter().find(|c| c.0 == checkpoint).map_or(entry, |c| c.2);
    ui.rewind = Some(RewindPicker { checkpoint, entry, selected: RewindMode::Both, plan: None });
    Some(Action::PlanRewind { checkpoint, mode: RewindMode::Both })
}

fn sidebar_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    let l = &mut ui.local;
    let idx = TABS.iter().position(|t| *t == l.tab).unwrap_or(0);
    match key.code {
        KeyCode::Left => {
            l.tab = TABS[(idx + TABS.len() - 1) % TABS.len()];
            l.sidebar_scroll = 0;
        }
        KeyCode::Right => {
            l.tab = TABS[(idx + 1) % TABS.len()];
            l.sidebar_scroll = 0;
        }
        KeyCode::Up => l.sidebar_scroll = l.sidebar_scroll.saturating_sub(1),
        KeyCode::Down => l.sidebar_scroll += 1,
        KeyCode::PageUp => l.sidebar_scroll = l.sidebar_scroll.saturating_sub(10),
        KeyCode::PageDown => l.sidebar_scroll += 10,
        KeyCode::Esc => l.focus = Focus::Input,
        _ => {}
    }
    None
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}
