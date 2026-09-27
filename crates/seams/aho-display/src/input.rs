//! The meaning of the `mouse`/`keyboard` tools (session desktop and `vm_*`) over any `Desktop`:
//! text and key names become X keysyms (RFB's key vocabulary), clicks become button-mask
//! sequences. Pure translation plus the ordered events; the desktop checks its own authority.

use crate::{Desktop, DisplayError, KeyEvent, PointerEvent};

/// X keysym of a named key (`Enter`, `Tab`, `Esc`, `F5`, `Left`, …) or of one character.
pub fn keysym(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "esc" | "escape" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
        "ctrl" | "control" => 0xffe3,
        "shift" => 0xffe1,
        "alt" => 0xffe9,
        "super" | "meta" | "cmd" | "win" => 0xffeb,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=12).contains(n)) {
                return Some(0xffbe + n - 1);
            }
            let mut chars = name.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            return Some(char_keysym(c));
        }
    };
    Some(named)
}

/// Latin-1 characters are their own keysym; anything else is `0x01000000 | codepoint`.
pub fn char_keysym(c: char) -> u32 {
    match c {
        '\n' => 0xff0d,
        '\t' => 0xff09,
        c if (0x20..=0x7e).contains(&(c as u32)) || (0xa0..=0xff).contains(&(c as u32)) => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

/// The key events for `text` typed as-is, then each chord in `keys` (`ctrl+c`, `alt+F4`,
/// `Enter`): modifiers go down in order, the key taps, modifiers come up in reverse.
pub fn key_events(text: &str, keys: &[String]) -> Result<Vec<KeyEvent>, DisplayError> {
    let mut out = Vec::new();
    for c in text.chars() {
        let keysym = char_keysym(c);
        out.push(KeyEvent { keysym, down: true });
        out.push(KeyEvent { keysym, down: false });
    }
    for chord in keys {
        let parts: Vec<&str> = if chord == "+" { vec!["+"] } else { chord.split('+').collect() };
        let syms = parts
            .iter()
            .map(|p| {
                keysym(p).ok_or_else(|| {
                    DisplayError::BadArgs(format!("unknown key `{p}` in `{chord}`; use names like Enter, Tab, Esc, F5, Left, ctrl+c"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for s in &syms {
            out.push(KeyEvent { keysym: *s, down: true });
        }
        for s in syms.iter().rev() {
            out.push(KeyEvent { keysym: *s, down: false });
        }
    }
    if out.is_empty() {
        return Err(DisplayError::BadArgs("give `text` to type and/or `keys` to press".into()));
    }
    Ok(out)
}

/// RFB button bit of `left`, `middle`, `right`, `wheel_up`, `wheel_down`.
pub fn button(name: &str) -> Option<u8> {
    Some(match name {
        "left" => 1,
        "middle" => 2,
        "right" => 4,
        "wheel_up" => 8,
        "wheel_down" => 16,
        _ => return None,
    })
}

/// Moves to (x, y); with `click`, presses and releases that button `count` times there.
pub fn mouse(desktop: &dyn Desktop, x: u16, y: u16, click: Option<&str>, count: u8) -> Result<(), DisplayError> {
    desktop.pointer(PointerEvent { x, y, buttons: 0 })?;
    if let Some(name) = click {
        let bit = button(name)
            .ok_or_else(|| DisplayError::BadArgs(format!("unknown button `{name}`; use left, middle, right, wheel_up or wheel_down")))?;
        for _ in 0..count.clamp(1, 3) {
            desktop.pointer(PointerEvent { x, y, buttons: bit })?;
            desktop.pointer(PointerEvent { x, y, buttons: 0 })?;
        }
    }
    Ok(())
}

/// Sends every key event in order; stops at the first failure.
pub fn type_keys(desktop: &dyn Desktop, events: &[KeyEvent]) -> Result<(), DisplayError> {
    events.iter().try_for_each(|e| desktop.key(*e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_press_modifiers_first_and_release_them_last() {
        let e = key_events("", &["ctrl+c".into()]).unwrap();
        let seq: Vec<(u32, bool)> = e.iter().map(|k| (k.keysym, k.down)).collect();
        assert_eq!(seq, vec![(0xffe3, true), (0x63, true), (0x63, false), (0xffe3, false)]);
    }

    #[test]
    fn text_maps_latin1_directly_and_the_rest_to_unicode_keysyms() {
        let e = key_events("aé€\n", &[]).unwrap();
        let downs: Vec<u32> = e.iter().filter(|k| k.down).map(|k| k.keysym).collect();
        assert_eq!(downs, vec![0x61, 0xe9, 0x0100_20ac, 0xff0d]);
        assert_eq!(keysym("F12"), Some(0xffc9));
        assert!(key_events("", &["ctrl+nokey".into()]).is_err());
        assert!(key_events("", &[]).is_err(), "nothing to send is an argument error");
    }
}
