//! Positions and URIs. The model speaks 1-based lines and 1-based character columns; LSP speaks
//! 0-based lines and columns counted in the negotiated position encoding (UTF-16 by default).

/// `general.positionEncodings` as negotiated in `initialize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    /// The server's `capabilities.positionEncoding`; absent means UTF-16 (LSP 3.17).
    pub fn parse(s: Option<&str>) -> Encoding {
        match s {
            Some("utf-8") => Encoding::Utf8,
            Some("utf-32") => Encoding::Utf32,
            _ => Encoding::Utf16,
        }
    }

    fn width(self, c: char) -> usize {
        match self {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
            Encoding::Utf32 => 1,
        }
    }
}

/// Line `n` (0-based) without its terminator; `None` past the end.
pub fn line(text: &str, n: usize) -> Option<&str> {
    text.split('\n').nth(n).map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// A 0-based character column → the server's column unit on `line`. Past the end clamps to it.
pub fn to_lsp_col(line: &str, chars: usize, enc: Encoding) -> u32 {
    let units: usize = line.chars().take(chars).map(|c| enc.width(c)).sum();
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// The server's column on `line` → a 0-based character column. A unit inside a character
/// (half a surrogate pair, a continuation byte) maps to that character.
pub fn from_lsp_col(line: &str, units: u32, enc: Encoding) -> usize {
    let target = units as usize;
    let mut seen = 0usize;
    for (i, c) in line.chars().enumerate() {
        if seen >= target {
            return i;
        }
        seen += enc.width(c);
        if seen > target {
            return i;
        }
    }
    line.chars().count()
}

/// `file://` URI for an absolute path, percent-encoding everything but unreserved bytes and `/`.
pub fn path_to_uri(abs: &str) -> String {
    let mut out = String::with_capacity(abs.len() + 7);
    out.push_str("file://");
    for b in abs.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The absolute path in a `file://` URI; `None` for other schemes or broken escapes.
pub fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path`: only the empty host and `localhost` name this machine.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Char columns (0-based) where `word` starts on `line` as a whole identifier.
pub fn word_starts(line: &str, word: &str) -> Vec<usize> {
    if word.is_empty() {
        return Vec::new();
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for (byte, _) in line.match_indices(word) {
        let before = line[..byte].chars().next_back();
        let after = line[byte + word.len()..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push(line[..byte].chars().count());
        }
    }
    out
}

/// Applies LSP text edits (0-based ranges in `enc` units) to `text`. Edits must not overlap.
pub fn apply_edits(text: &str, edits: &[(u32, u32, u32, u32, String)], enc: Encoding) -> Result<String, String> {
    let offset = |l: u32, c: u32| -> Result<usize, String> {
        let mut start = 0usize;
        for (i, ln) in text.split('\n').enumerate() {
            if i == l as usize {
                let body = ln.strip_suffix('\r').unwrap_or(ln);
                let ch = from_lsp_col(body, c, enc);
                let within: usize = body.chars().take(ch).map(char::len_utf8).sum();
                return Ok(start + within);
            }
            start += ln.len() + 1;
        }
        if l as usize == text.split('\n').count() && c == 0 {
            return Ok(text.len());
        }
        Err(format!("an edit points at line {} past the end of the file", l + 1))
    };
    let mut spans = Vec::with_capacity(edits.len());
    for (sl, sc, el, ec, new) in edits {
        let (a, b) = (offset(*sl, *sc)?, offset(*el, *ec)?);
        if b < a {
            return Err(format!("an edit ends before it starts (line {})", sl + 1));
        }
        spans.push((a, b, new.as_str()));
    }
    spans.sort_by_key(|s| (s.0, s.1));
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err("the server returned overlapping edits".into());
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (a, b, new) in spans {
        out.push_str(&text[at..a]);
        out.push_str(new);
        at = b;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_convert_per_encoding_around_multibyte_characters() {
        let l = "a😀é b";
        // chars: a(1/1/1) 😀(4/2/1) é(2/1/1) ' ' b
        assert_eq!(to_lsp_col(l, 2, Encoding::Utf16), 3);
        assert_eq!(to_lsp_col(l, 2, Encoding::Utf8), 5);
        assert_eq!(to_lsp_col(l, 2, Encoding::Utf32), 2);
        for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            for chars in 0..=l.chars().count() {
                assert_eq!(from_lsp_col(l, to_lsp_col(l, chars, enc), enc), chars, "{enc:?} {chars}");
            }
        }
        // Half a surrogate pair maps to the character it belongs to; past the end clamps.
        assert_eq!(from_lsp_col(l, 2, Encoding::Utf16), 1);
        assert_eq!(from_lsp_col(l, 99, Encoding::Utf16), 5);
        assert_eq!(to_lsp_col(l, 99, Encoding::Utf16), 6);
    }

    #[test]
    fn uris_round_trip_paths_with_spaces_and_unicode() {
        let p = "/tmp/my dir/ñ#1.rs";
        let u = path_to_uri(p);
        assert_eq!(u, "file:///tmp/my%20dir/%C3%B1%231.rs");
        assert_eq!(uri_to_path(&u).as_deref(), Some(p));
        assert_eq!(uri_to_path("file://localhost/a/b").as_deref(), Some("/a/b"));
        assert_eq!(uri_to_path("https://x/y"), None);
        assert_eq!(uri_to_path("file:///a%2"), None);
    }

    #[test]
    fn lines_strip_crlf_and_word_starts_respect_identifier_boundaries() {
        assert_eq!(line("a\r\nbc\n", 1), Some("bc"));
        assert_eq!(line("a", 3), None);
        assert_eq!(word_starts("add(adder, add_one, add)", "add"), vec![0, 20]);
        assert_eq!(word_starts("é add", "add"), vec![2]);
    }

    #[test]
    fn edits_apply_back_to_front_and_overlaps_are_refused() {
        let t = "fn add() {}\nlet x = add();\n";
        let e = vec![(0, 3, 0, 6, "sum".to_string()), (1, 8, 1, 11, "sum".to_string())];
        assert_eq!(apply_edits(t, &e, Encoding::Utf16).unwrap(), "fn sum() {}\nlet x = sum();\n");
        let bad = vec![(0, 0, 0, 5, "x".to_string()), (0, 3, 0, 6, "y".to_string())];
        assert!(apply_edits(t, &bad, Encoding::Utf16).is_err());
        let append = vec![(2, 0, 2, 0, "// end\n".to_string())];
        assert_eq!(apply_edits(t, &append, Encoding::Utf16).unwrap(), format!("{t}// end\n"));
    }
}
