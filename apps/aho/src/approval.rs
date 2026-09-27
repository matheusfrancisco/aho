//! Answering approval cards on the terminal: `y` allow once, `s` allow for the session,
//! `r` reject, `p` switch to the card's preset (when it names one), `Esc` / Ctrl-D / Ctrl-C
//! cancel. One keypress, no Enter.

use std::io::{BufRead, Write};

use aho_approval::Decided;
use aho_log::GrantScope;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Decided(Decided),
    /// Write the card's preset cut, then allow this call for the session.
    Preset,
}

fn key_to_answer(code: KeyCode, mods: KeyModifiers, preset: bool) -> Option<Answer> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let d = match code {
        KeyCode::Char('c' | 'd') if ctrl => Decided::cancelled(),
        KeyCode::Char('y' | 'Y') => Decided::allowed(GrantScope::Once),
        KeyCode::Char('s' | 'S') => Decided::allowed(GrantScope::Session),
        KeyCode::Char('r' | 'R' | 'n' | 'N') => Decided::rejected(),
        KeyCode::Char('p' | 'P') if preset => return Some(Answer::Preset),
        KeyCode::Esc => Decided::cancelled(),
        _ => return None,
    };
    Some(Answer::Decided(d))
}

fn line_to_answer(line: &str, preset: bool) -> Answer {
    Answer::Decided(match line.trim() {
        "y" | "Y" | "yes" => Decided::allowed(GrantScope::Once),
        "s" | "S" => Decided::allowed(GrantScope::Session),
        "r" | "R" | "n" | "N" | "no" => Decided::rejected(),
        "p" | "P" if preset => return Answer::Preset,
        _ => Decided::cancelled(),
    })
}

/// Blocks until one answer; `preset` offers `p`. Raw mode when the terminal allows it;
/// otherwise one line of input (end of input cancels).
pub fn read_answer(preset: bool) -> Answer {
    let mut out = std::io::stdout();
    if crossterm::terminal::enable_raw_mode().is_err() {
        let mut line = String::new();
        return match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => Answer::Decided(Decided::cancelled()),
            Ok(_) => line_to_answer(&line, preset),
        };
    }
    let answer = loop {
        match crossterm::event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if let Some(a) = key_to_answer(k.code, k.modifiers, preset) {
                    break a;
                }
            }
            Ok(_) => {}
            Err(_) => break Answer::Decided(Decided::cancelled()),
        }
    };
    // Leaving raw mode can only fail if the terminal went away; the answer stands either way.
    if crossterm::terminal::disable_raw_mode().is_err() {
        return answer;
    }
    let label = match answer {
        Answer::Preset => "switching preset",
        Answer::Decided(d) => match (d.outcome, d.scope) {
            (aho_log::ApprovalOutcome::AllowedOnce, GrantScope::Once) => "allowed once",
            (aho_log::ApprovalOutcome::AllowedOnce, _) => "allowed for this session",
            (aho_log::ApprovalOutcome::Rejected, _) => "rejected",
            _ => "cancelled",
        },
    };
    if writeln!(out, "{label}").and_then(|_| out.flush()).is_err() {
        return answer;
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_log::ApprovalOutcome;

    fn outcome(a: Option<Answer>) -> Option<ApprovalOutcome> {
        match a {
            Some(Answer::Decided(d)) => Some(d.outcome),
            _ => None,
        }
    }

    #[test]
    fn keys_map_to_the_outcomes() {
        let none = KeyModifiers::NONE;
        assert_eq!(key_to_answer(KeyCode::Char('y'), none, false), Some(Answer::Decided(Decided::allowed(GrantScope::Once))));
        assert_eq!(key_to_answer(KeyCode::Char('s'), none, false), Some(Answer::Decided(Decided::allowed(GrantScope::Session))));
        assert_eq!(outcome(key_to_answer(KeyCode::Char('r'), none, false)), Some(ApprovalOutcome::Rejected));
        assert_eq!(outcome(key_to_answer(KeyCode::Esc, none, false)), Some(ApprovalOutcome::Cancelled));
        assert_eq!(outcome(key_to_answer(KeyCode::Char('d'), KeyModifiers::CONTROL, false)), Some(ApprovalOutcome::Cancelled));
        assert_eq!(key_to_answer(KeyCode::Char('x'), none, false), None, "other keys wait");
        assert_eq!(key_to_answer(KeyCode::Char('p'), none, false), None, "`p` waits when the card names no preset");
        assert_eq!(key_to_answer(KeyCode::Char('p'), none, true), Some(Answer::Preset));
        assert_eq!(line_to_answer("", true), Answer::Decided(Decided::cancelled()), "an empty line never allows");
    }
}
