//! Content-type awareness: what a fetched body is, and how its bytes become text.

/// What a response body is, by `content-type` or (when absent) by sniffing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Html,
    Text,
    /// A raster format models accept as an image block.
    Image(String),
    Binary(String),
}

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
const TEXT_APPLICATION: &[&str] = &[
    "application/json",
    "application/xml",
    "application/javascript",
    "application/x-javascript",
    "application/ecmascript",
    "application/yaml",
    "application/x-yaml",
    "application/toml",
    "application/x-sh",
    "application/sql",
    "application/graphql",
    "application/x-ndjson",
];

pub fn classify(media: Option<&str>, body: &[u8]) -> Kind {
    match media {
        Some("text/html" | "application/xhtml+xml") => Kind::Html,
        Some(m) if m.starts_with("text/") || TEXT_APPLICATION.contains(&m) || m.ends_with("+json") || m.ends_with("+xml") => Kind::Text,
        Some(m) if IMAGE_TYPES.contains(&m) => Kind::Image(m.to_string()),
        Some(m) if m != "application/octet-stream" => Kind::Binary(m.to_string()),
        _ => sniff(body),
    }
}

fn sniff(body: &[u8]) -> Kind {
    let head: Vec<u8> = body.iter().skip_while(|b| b.is_ascii_whitespace()).take(64).map(u8::to_ascii_lowercase).collect();
    if head.starts_with(b"<!doctype html") || head.starts_with(b"<html") || head.starts_with(b"<head") || head.starts_with(b"<body") {
        return Kind::Html;
    }
    for (magic, media) in [(&b"\x89PNG"[..], "image/png"), (b"\xff\xd8\xff", "image/jpeg"), (b"GIF8", "image/gif")] {
        if body.starts_with(magic) {
            return Kind::Image(media.into());
        }
    }
    if body.len() >= 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        return Kind::Image("image/webp".into());
    }
    let sample = &body[..body.len().min(4096)];
    if sample.contains(&0)
        || (std::str::from_utf8(sample).is_err() && std::str::from_utf8(&sample[..sample.len().saturating_sub(3)]).is_err())
    {
        return Kind::Binary("application/octet-stream".into());
    }
    Kind::Text
}

/// `<meta charset=…>` or `<meta http-equiv=content-type content="…; charset=…">` in the first KiB.
pub fn meta_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    let at = head.find("charset=")?;
    let rest = head[at + 8..].trim_start_matches(['"', '\'']);
    let cs: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')).collect();
    (!cs.is_empty()).then_some(cs)
}

/// windows-1252 0x80–0x9F; undefined slots map to U+FFFD.
const CP1252_HIGH: [char; 32] = [
    '€', '\u{fffd}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{fffd}', 'Ž', '\u{fffd}', '\u{fffd}', '‘', '’', '“', '”',
    '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{fffd}', 'ž', 'Ÿ',
];

/// Decodes `body` by `charset` (BOM first). Returns the text and, if the charset was not one aho
/// decodes, a note saying it was read as UTF-8.
pub fn decode(body: &[u8], charset: Option<&str>) -> (String, Option<String>) {
    if let Some(rest) = body.strip_prefix(b"\xef\xbb\xbf") {
        return (String::from_utf8_lossy(rest).into_owned(), None);
    }
    if let Some(rest) = body.strip_prefix(b"\xff\xfe") {
        return (utf16(rest, u16::from_le_bytes), None);
    }
    if let Some(rest) = body.strip_prefix(b"\xfe\xff") {
        return (utf16(rest, u16::from_be_bytes), None);
    }
    match charset.unwrap_or("utf-8") {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => (String::from_utf8_lossy(body).into_owned(), None),
        // WHATWG: every latin1 label means windows-1252.
        "iso-8859-1" | "latin1" | "l1" | "iso8859-1" | "windows-1252" | "cp1252" | "x-cp1252" => {
            (body.iter().map(|&b| if (0x80..0xa0).contains(&b) { CP1252_HIGH[(b - 0x80) as usize] } else { b as char }).collect(), None)
        }
        "utf-16le" => (utf16(body, u16::from_le_bytes), None),
        "utf-16be" | "utf-16" => (utf16(body, u16::from_be_bytes), None),
        other => (
            String::from_utf8_lossy(body).into_owned(),
            Some(format!("charset `{other}` is not decoded by aho; the text was read as UTF-8 and may contain replacement characters")),
        ),
    }
}

fn utf16(b: &[u8], f: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| f(*c)).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_and_sniffing_pick_the_right_kind() {
        assert_eq!(classify(Some("text/html"), b""), Kind::Html);
        assert_eq!(classify(Some("application/ld+json"), b""), Kind::Text);
        assert_eq!(classify(Some("image/png"), b""), Kind::Image("image/png".into()));
        assert_eq!(classify(Some("application/pdf"), b"%PDF"), Kind::Binary("application/pdf".into()));
        assert_eq!(classify(None, b"  <!DOCTYPE html><p>x"), Kind::Html);
        assert_eq!(classify(Some("application/octet-stream"), b"\x89PNG\r\n"), Kind::Image("image/png".into()));
        assert_eq!(classify(None, b"plain words"), Kind::Text);
        assert_eq!(classify(None, b"a\0b"), Kind::Binary("application/octet-stream".into()));
    }

    #[test]
    fn charsets_decode() {
        assert_eq!(decode(b"caf\xe9 \x93q\x94", Some("iso-8859-1")).0, "café “q”");
        assert_eq!(decode(b"\xef\xbb\xbfhi", Some("iso-8859-1")).0, "hi");
        assert_eq!(decode(b"\xff\xfeh\0i\0", None).0, "hi");
        let (text, note) = decode(b"x", Some("koi8-r"));
        assert_eq!(text, "x");
        assert!(note.unwrap().contains("koi8-r"));
        assert_eq!(meta_charset(b"<html><head><meta charset=\"Windows-1252\">").as_deref(), Some("windows-1252"));
    }
}
