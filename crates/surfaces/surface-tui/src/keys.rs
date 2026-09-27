//! Key handling is pure: a key edits surface-local state and may yield one [`Action`] the runtime
//! performs against the session. The approval card answers first, then the top overlay, then the
//! `/` menu while it is open, then the prompt.

use aho_approval::Decided;
use aho_log::{GrantScope, RewindMode};
use aho_types::CheckpointId;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::menu;
use crate::model::{CheckpointList, MODES, Notice, Overlay, Panel, RewindPicker, UiState, View};
use crate::slash::{HELP, Parsed, parse_line};

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Prompt(String),
    /// `/name args…`: run the app's command; never reaches the model.
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
    if !ui.overlays.is_empty() {
        return overlay_key(ui, key);
    }
    if alt && key.code == KeyCode::Char('h') {
        ui.local.hud = !ui.local.hud;
        return None;
    }
    if let Some((items, selected)) = menu::open(ui)
        && let Some(done) = menu_key(ui, key, &items, selected)
    {
        return done;
    }
    input_key(ui, key, ctrl, alt)
}

/// The menu's keys; `None` = not a menu key, the prompt handles it.
fn menu_key(ui: &mut UiState, key: KeyEvent, items: &[menu::Suggestion], selected: usize) -> Option<Option<Action>> {
    let n = items.len();
    let l = &mut ui.local;
    match key.code {
        KeyCode::Up => l.menu_selected = (selected + n - 1) % n,
        KeyCode::Down => l.menu_selected = (selected + 1) % n,
        KeyCode::PageUp => l.menu_selected = selected.saturating_sub(menu::MENU_ROWS),
        KeyCode::PageDown => l.menu_selected = (selected + menu::MENU_ROWS).min(n - 1),
        KeyCode::Esc => l.menu_dismissed = true,
        KeyCode::Tab => complete(ui, &items[selected].insert),
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
            let s = &items[selected];
            complete(ui, &s.insert);
            // A name that needs its argument stops here: the menu now offers the values.
            if !s.run {
                return Some(None);
            }
            return None;
        }
        _ => return None,
    }
    Some(None)
}

fn complete(ui: &mut UiState, insert: &str) {
    let l = &mut ui.local;
    l.draft = insert.to_string();
    l.cursor = l.draft.chars().count();
    l.menu_selected = 0;
    l.menu_dismissed = false;
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
                ui.notice = Some(Notice::warn(match &view.cut {
                    Some(c) => format!("`{c}` is not a preset change; apply it with the aho CLI, then answer this card"),
                    None => "no single policy change would let this call through; answer with a, s, r or Esc".into(),
                }));
                None
            }
        },
        KeyCode::Esc => Some(Action::Answer(Decided::cancelled())),
        _ => None,
    }
}

fn overlay_key(ui: &mut UiState, key: KeyEvent) -> Option<Action> {
    let top = ui.overlays.last_mut()?;
    match top {
        Overlay::Panel(p) => {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    ui.overlays.pop();
                }
                KeyCode::Up | KeyCode::Char('k') => p.scroll = p.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => p.scroll += 1,
                KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(10),
                KeyCode::PageDown | KeyCode::Char(' ') => p.scroll += 10,
                KeyCode::Home | KeyCode::Char('g') => p.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => p.scroll = usize::MAX / 2,
                _ => {}
            }
            None
        }
        Overlay::Checkpoints(list) => {
            let n = list.items.len().max(1);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    ui.overlays.pop();
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    list.selected = (list.selected + n - 1) % n;
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    list.selected = (list.selected + 1) % n;
                    None
                }
                KeyCode::Enter => {
                    let item = list.items.get(list.selected)?.clone();
                    ui.overlays.push(Overlay::Rewind(RewindPicker {
                        checkpoint: item.id,
                        entry: item.entry,
                        selected: RewindMode::Both,
                        plan: None,
                    }));
                    Some(Action::PlanRewind { checkpoint: item.id, mode: RewindMode::Both })
                }
                _ => None,
            }
        }
        Overlay::Rewind(picker) => {
            let idx = MODES.iter().position(|m| *m == picker.selected).unwrap_or(0);
            let select = |p: &mut RewindPicker, i: usize| {
                p.selected = MODES[i];
                p.plan = None;
                Some(Action::PlanRewind { checkpoint: p.checkpoint, mode: p.selected })
            };
            match key.code {
                KeyCode::Esc => {
                    ui.overlays.pop();
                    None
                }
                KeyCode::Up => select(picker, (idx + MODES.len() - 1) % MODES.len()),
                KeyCode::Down => select(picker, (idx + 1) % MODES.len()),
                KeyCode::Char(c @ '1'..='3') => select(picker, (c as usize) - ('1' as usize)),
                KeyCode::Enter => match &picker.plan {
                    Some(Ok(plan)) if plan.mode == picker.selected => {
                        let a = Action::Rewind { checkpoint: picker.checkpoint, mode: picker.selected };
                        // Applying closes the whole rewind flow (the list under the picker too).
                        ui.overlays.retain(|o| !matches!(o, Overlay::Rewind(_) | Overlay::Checkpoints(_)));
                        Some(a)
                    }
                    Some(Err(_)) => None,
                    _ => {
                        ui.notice = Some(Notice::warn("the rewind plan is still being computed; press Enter again once it is shown"));
                        None
                    }
                },
                _ => None,
            }
        }
    }
}

/// `/rewind`: the checkpoint list, or a notice when there is none yet.
pub fn open_rewind(ui: &mut UiState) -> Option<Action> {
    let items = ui.checkpoint_items();
    if items.is_empty() {
        ui.notice = Some(Notice::warn("no checkpoint on this branch yet; one is taken at every turn end"));
        return None;
    }
    ui.overlays.push(Overlay::Checkpoints(CheckpointList { items, selected: 0 }));
    None
}

/// The TUI's own commands (the views); `None` = not one of them, the app's registry runs it.
fn view_command(ui: &mut UiState, name: &str) -> Option<Option<Action>> {
    let open = |ui: &mut UiState, v: View| {
        ui.overlays.push(Overlay::Panel(Panel::view(v)));
        Some(None)
    };
    match name {
        HELP => open(ui, View::Help),
        "tree" => open(ui, View::Tree),
        "inventory" => open(ui, View::Inventory),
        "jobs" => open(ui, View::Jobs),
        "invariants" => open(ui, View::Invariants),
        "rewind" => Some(open_rewind(ui)),
        "hud" => {
            ui.local.hud = !ui.local.hud;
            ui.notice = Some(Notice::info(if ui.local.hud { "HUD on: entry ids and origins" } else { "HUD off" }));
            Some(None)
        }
        "thinking" => {
            ui.local.show_thinking = !ui.local.show_thinking;
            ui.notice = Some(Notice::info(if ui.local.show_thinking {
                "Thinking shown for new output; `/thinking` hides it"
            } else {
                "Thinking hidden for new output; terminal scrollback already printed is unchanged"
            }));
            Some(None)
        }
        "quit" | "exit" => Some(Some(Action::Quit)),
        _ => None,
    }
}

fn input_key(ui: &mut UiState, key: KeyEvent, ctrl: bool, alt: bool) -> Option<Action> {
    let edited = matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete) && !ctrl;
    let action = edit(ui, key, ctrl, alt);
    if edited || (ctrl && key.code == KeyCode::Char('u')) {
        ui.local.menu_selected = 0;
        ui.local.menu_dismissed = false;
    }
    action
}

fn edit(ui: &mut UiState, key: KeyEvent, ctrl: bool, alt: bool) -> Option<Action> {
    let len = ui.local.draft.chars().count();
    match key.code {
        KeyCode::Enter => {
            let text = ui.local.draft.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let l = &mut ui.local;
            l.draft.clear();
            l.cursor = 0;
            l.scroll = 0;
            l.menu_selected = 0;
            l.menu_dismissed = false;
            let text = match parse_line(&text) {
                Parsed::Command { name, args } => {
                    if args.is_empty()
                        && let Some(done) = view_command(ui, name)
                    {
                        return done;
                    }
                    return Some(Action::Command { name: name.into(), args: args.into() });
                }
                Parsed::Prompt(p) => p.to_string(),
            };
            if alt {
                Some(Action::Steer { text, urgent: true })
            } else if ui.is_busy() {
                Some(Action::Steer { text, urgent: false })
            } else {
                Some(Action::Prompt(text))
            }
        }
        KeyCode::Char('d') if ctrl && ui.local.draft.is_empty() => Some(Action::Quit),
        _ => {
            let l = &mut ui.local;
            match key.code {
                KeyCode::Char('a') if ctrl => l.cursor = 0,
                KeyCode::Char('e') if ctrl => l.cursor = len,
                KeyCode::Char('u') if ctrl => {
                    l.draft = l.draft.chars().skip(l.cursor).collect();
                    l.cursor = 0;
                }
                KeyCode::Char('w') if ctrl => {
                    let chars: Vec<char> = l.draft.chars().collect();
                    let mut at = l.cursor;
                    while at > 0 && chars[at - 1] == ' ' {
                        at -= 1;
                    }
                    while at > 0 && chars[at - 1] != ' ' {
                        at -= 1;
                    }
                    l.draft = chars[..at].iter().chain(&chars[l.cursor..]).collect();
                    l.cursor = at;
                    l.menu_dismissed = false;
                }
                KeyCode::Char(c) if !ctrl => {
                    let at = byte_at(&l.draft, l.cursor);
                    l.draft.insert(at, c);
                    l.cursor += 1;
                }
                KeyCode::Backspace if l.cursor > 0 => {
                    let at = byte_at(&l.draft, l.cursor - 1);
                    l.draft.remove(at);
                    l.cursor -= 1;
                }
                KeyCode::Delete if l.cursor < len => {
                    let at = byte_at(&l.draft, l.cursor);
                    l.draft.remove(at);
                }
                KeyCode::Left => l.cursor = l.cursor.saturating_sub(1),
                KeyCode::Right => l.cursor = (l.cursor + 1).min(len),
                KeyCode::Home => l.cursor = 0,
                KeyCode::End => l.cursor = len,
                KeyCode::PageUp => l.scroll += 10,
                KeyCode::PageDown => l.scroll = l.scroll.saturating_sub(10),
                _ => {}
            }
            None
        }
    }
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}
