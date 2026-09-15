//! Named keys → the bytes an xterm sends. Names are case-insensitive; modifiers are `C-`
//! (also `Ctrl-`, `^`), `M-` (also `Alt-`, `Meta-`, sends ESC first) and `S-Tab`.

/// Keys that submit a line (the classifier treats them as a line break).
pub fn is_submit(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "enter" | "return" | "c-m" | "c-j" | "ctrl-m" | "ctrl-j" | "^m" | "^j")
}

/// Encodes `names` in order. Arrow/Home/End keys follow the program's cursor-key mode (DECCKM).
pub fn encode(names: &[String], application_cursor: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for n in names {
        out.extend(key(n, application_cursor).ok_or_else(|| {
            format!("`{n}` is not a key name (try Enter, Tab, Esc, Backspace, Up, Down, Left, Right, Home, End, PageUp, PageDown, Delete, Insert, F1–F12, Space, C-<key>, M-<key>)")
        })?);
    }
    Ok(out)
}

fn key(name: &str, app: bool) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    for prefix in ["m-", "alt-", "meta-"] {
        if let Some(rest) = strip(&lower, name, prefix) {
            let mut v = vec![0x1b];
            v.extend(key(rest, app)?);
            return Some(v);
        }
    }
    for prefix in ["c-", "ctrl-", "^"] {
        if let Some(rest) = strip(&lower, name, prefix) {
            return ctrl(rest).map(|b| vec![b]);
        }
    }
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let cursor = |c: char| if app { format!("\x1bO{c}").into_bytes() } else { format!("\x1b[{c}").into_bytes() };
    Some(match lower.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "s-tab" | "backtab" => csi("Z"),
        "esc" | "escape" => vec![0x1b],
        "backspace" | "bs" => vec![0x7f],
        "space" => b" ".to_vec(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" | "ins" => csi("2~"),
        "delete" | "del" => csi("3~"),
        "pageup" | "pgup" => csi("5~"),
        "pagedown" | "pgdn" => csi("6~"),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => csi("15~"),
        "f6" => csi("17~"),
        "f7" => csi("18~"),
        "f8" => csi("19~"),
        "f9" => csi("20~"),
        "f10" => csi("21~"),
        "f11" => csi("23~"),
        "f12" => csi("24~"),
        _ => {
            // A single character stands for itself (so `M-x` and `C-c` compose).
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            c.to_string().into_bytes()
        }
    })
}

/// `prefix` matched case-insensitively; the rest keeps its original case.
fn strip<'a>(lower: &str, name: &'a str, prefix: &str) -> Option<&'a str> {
    (lower.starts_with(prefix) && name.len() > prefix.len()).then(|| &name[prefix.len()..])
}

/// Control byte for `C-<rest>`.
fn ctrl(rest: &str) -> Option<u8> {
    let lower = rest.to_ascii_lowercase();
    match lower.as_str() {
        "space" | "@" => return Some(0),
        "?" => return Some(0x7f),
        _ => {}
    }
    let mut chars = lower.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(names: &[&str], app: bool) -> Result<Vec<u8>, String> {
        encode(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>(), app)
    }

    #[test]
    fn control_and_meta_keys_encode_like_xterm() {
        assert_eq!(enc(&["C-c"], false).unwrap(), vec![3]);
        assert_eq!(enc(&["ctrl-D", "^z", "C-["], false).unwrap(), vec![4, 26, 0x1b]);
        assert_eq!(enc(&["M-x"], false).unwrap(), vec![0x1b, b'x']);
        assert_eq!(enc(&["Alt-C-a"], false).unwrap(), vec![0x1b, 1]);
        assert_eq!(enc(&["Enter", "Tab", "Esc", "Backspace", "Space"], false).unwrap(), b"\r\t\x1b\x7f ".to_vec());
        assert_eq!(enc(&["F1", "F5", "F12", "PageDown", "Delete"], false).unwrap(), b"\x1bOP\x1b[15~\x1b[24~\x1b[6~\x1b[3~".to_vec());
    }

    #[test]
    fn arrow_keys_follow_the_cursor_key_mode() {
        assert_eq!(enc(&["Up", "left"], false).unwrap(), b"\x1b[A\x1b[D".to_vec());
        assert_eq!(enc(&["Up", "left"], true).unwrap(), b"\x1bOA\x1bOD".to_vec());
    }

    #[test]
    fn unknown_names_are_refused_with_the_list_of_valid_ones() {
        let e = enc(&["Enter", "Hyper-q"], false).unwrap_err();
        assert!(e.starts_with("`Hyper-q` is not a key name") && e.contains("C-<key>"), "{e}");
        assert!(enc(&["C-1"], false).is_err());
        assert!(is_submit("ENTER") && is_submit("C-m") && !is_submit("Tab"));
    }
}
