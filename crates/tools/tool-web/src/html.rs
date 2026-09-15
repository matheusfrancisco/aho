//! HTML → readable markdown, without a DOM dependency.
//!
//! A single forward pass over a forgiving tokenizer: `script`/`style`/`head`-like subtrees are
//! dropped, block elements become paragraph breaks, headings/lists/links/emphasis/code/quotes/
//! tables become their markdown forms, entities are decoded, and whitespace collapses outside
//! `<pre>`. Readability heuristic: when the page has a `<main>` or `<article>`, only the first
//! such subtree is rendered (navigation and footers usually live outside it).

use url::Url;

/// Elements whose whole subtree is not page text.
const DROP: &[&str] = &["script", "style", "noscript", "template", "svg", "head", "iframe", "canvas", "select", "button", "object"];
const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "tr",
    "ul",
];

#[derive(Debug)]
enum Token<'a> {
    Text(&'a str),
    Open { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    Close(String),
}

fn tokenize(html: &str) -> Vec<Token<'_>> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    let mut raw_until: Option<String> = None;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        // Inside script/style: only the matching close tag ends the raw text.
        if let Some(end) = &raw_until {
            let rest = &html[i..];
            if rest.as_bytes().get(1) == Some(&b'/') && rest.get(2..2 + end.len()).is_some_and(|n| n.eq_ignore_ascii_case(end)) {
                raw_until = None;
            } else {
                i += 1;
                continue;
            }
        }
        if text_start < i {
            out.push(Token::Text(&html[text_start..i]));
        }
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map(|e| i + e + 3).unwrap_or(b.len());
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").unwrap_or(rest.len());
            out.push(Token::Text(&rest[9..end.max(9)]));
            i += (end + 3).min(rest.len());
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map(|e| i + e + 1).unwrap_or(b.len());
        } else if let Some((tok, len)) = parse_tag(rest) {
            if let Token::Open { name, self_closing: false, .. } = &tok
                && matches!(name.as_str(), "script" | "style" | "textarea" | "title")
            {
                raw_until = Some(name.clone());
            }
            out.push(tok);
            i += len;
        } else {
            // A lone `<` is text.
            text_start = i;
            i += 1;
            continue;
        }
        text_start = i;
    }
    if text_start < b.len() {
        out.push(Token::Text(&html[text_start..]));
    }
    out
}

fn parse_tag(s: &str) -> Option<(Token<'static>, usize)> {
    let b = s.as_bytes();
    let closing = b.get(1) == Some(&b'/');
    let mut i = if closing { 2 } else { 1 };
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-' || b[i] == b':') {
        i += 1;
    }
    if i == start || !b[start].is_ascii_alphabetic() {
        return None;
    }
    let name = s[start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => return None,
            Some(b'>') => {
                i += 1;
                break;
            }
            Some(b'/') => {
                self_closing = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        let a = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = s[a..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let end = s[i + 1..].find(q as char).map(|e| i + 1 + e)?;
                    value = decode_entities(&s[i + 1..end]);
                    i = end + 1;
                }
                _ => {
                    let v = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(&s[v..i]);
                }
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        } else {
            i += 1;
        }
    }
    let tok = if closing { Token::Close(name) } else { Token::Open { name, attrs, self_closing } };
    Some((tok, i))
}

/// Named entities beyond the XML five that show up in ordinary prose.
const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", " "),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("hellip", "…"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("middot", "·"),
    ("bull", "•"),
    ("times", "×"),
    ("divide", "÷"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("deg", "°"),
    ("sect", "§"),
    ("para", "¶"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
];

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..].find(|c: char| c == ';' || c == '&' || c.is_whitespace() || c == '<').map(|e| e + 1);
        let decoded = match end {
            Some(e) if rest.as_bytes().get(e) == Some(&b';') => {
                let name = &rest[1..e];
                let ch = if let Some(num) = name.strip_prefix('#') {
                    let n = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => num.parse::<u32>().ok(),
                    };
                    n.and_then(char::from_u32).map(|c| c.to_string())
                } else {
                    ENTITIES.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
                };
                ch.map(|c| (c, e + 1))
            }
            _ => None,
        };
        match decoded {
            Some((c, len)) => {
                out.push_str(&c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct Writer {
    out: String,
    /// Pending separator: 0 none, 1 space, 2 line break, 3 paragraph break.
    pending: u8,
    pre: usize,
    quote: usize,
    lists: Vec<Option<u32>>,
    link: Option<(String, usize)>,
}

impl Writer {
    fn want(&mut self, level: u8) {
        self.pending = self.pending.max(level);
    }

    fn flush(&mut self) {
        let at_start = self.out.is_empty();
        match self.pending {
            3 | 2 if !at_start => {
                let breaks = if self.pending == 3 { "\n\n" } else { "\n" };
                while self.out.ends_with(' ') {
                    self.out.pop();
                }
                self.out.push_str(breaks);
                self.prefix();
            }
            3 | 2 => self.prefix(),
            1 if !at_start && !self.out.ends_with([' ', '\n']) => self.out.push(' '),
            _ => {}
        }
        self.pending = 0;
    }

    fn prefix(&mut self) {
        for _ in 0..self.quote {
            self.out.push_str("> ");
        }
    }

    fn push_raw(&mut self, s: &str) {
        self.flush();
        self.out.push_str(s);
    }

    fn text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if self.pre > 0 {
            self.flush();
            self.out.push_str(&decoded);
            return;
        }
        let mut words = decoded.split(|c: char| c.is_whitespace() && c != '\u{a0}').peekable();
        if decoded.starts_with(|c: char| c.is_whitespace() && c != '\u{a0}') {
            self.want(1);
        }
        while let Some(w) = words.next() {
            if w.is_empty() {
                continue;
            }
            self.flush();
            self.out.push_str(&w.replace('\u{a0}', " "));
            if words.peek().is_some() {
                self.want(1);
            }
        }
        if decoded.ends_with(|c: char| c.is_whitespace() && c != '\u{a0}') {
            self.want(1);
        }
    }
}

/// Text content of an HTML fragment (search snippets), whitespace collapsed.
pub fn strip_tags(fragment: &str) -> String {
    let mut s = String::new();
    for t in tokenize(fragment) {
        match t {
            Token::Text(x) => s.push_str(&decode_entities(x)),
            Token::Open { name, .. } | Token::Close(name) if BLOCK.contains(&name.as_str()) || name == "br" => s.push(' '),
            _ => {}
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn resolve(base: Option<&Url>, href: &str) -> String {
    match base.and_then(|b| b.join(href).ok()) {
        Some(u) => u.to_string(),
        None => href.to_string(),
    }
}

/// `(title, markdown)`.
pub fn to_markdown(html: &str, base: Option<&Url>) -> (Option<String>, String) {
    let tokens = tokenize(html);
    let title = title_of(&tokens);
    let base_href = tokens.iter().find_map(|t| match t {
        Token::Open { name, attrs, .. } if name == "base" => attr(attrs, "href").and_then(|h| base.and_then(|b| b.join(h).ok())),
        _ => None,
    });
    let base = base_href.as_ref().or(base);
    let body = main_subtree(&tokens);
    (title, render(body, base))
}

fn title_of(tokens: &[Token<'_>]) -> Option<String> {
    let at = tokens.iter().position(|t| matches!(t, Token::Open { name, .. } if name == "title"))?;
    let mut s = String::new();
    for t in &tokens[at + 1..] {
        match t {
            Token::Text(x) => s.push_str(x),
            _ => break,
        }
    }
    let t = decode_entities(&s).split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

/// The first `<main>` (else `<article>`) subtree, if it has any text; otherwise everything.
fn main_subtree<'t, 'a>(tokens: &'t [Token<'a>]) -> &'t [Token<'a>] {
    for tag in ["main", "article"] {
        let Some(start) = tokens.iter().position(|t| matches!(t, Token::Open { name, .. } if name == tag)) else { continue };
        let mut depth = 0usize;
        for (i, t) in tokens.iter().enumerate().skip(start) {
            match t {
                Token::Open { name, self_closing: false, .. } if name == tag => depth += 1,
                Token::Close(name) if name == tag => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let sub = &tokens[start..=i];
                        if sub.iter().any(|t| matches!(t, Token::Text(x) if !x.trim().is_empty())) {
                            return sub;
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    tokens
}

fn render(tokens: &[Token<'_>], base: Option<&Url>) -> String {
    let mut w = Writer { out: String::new(), pending: 0, pre: 0, quote: 0, lists: Vec::new(), link: None };
    let mut drop_depth: Option<(String, usize)> = None;
    for t in tokens {
        if let Some((tag, depth)) = &mut drop_depth {
            match t {
                Token::Open { name, self_closing: false, .. } if name == tag => *depth += 1,
                Token::Close(name) if name == tag => {
                    *depth -= 1;
                    if *depth == 0 {
                        drop_depth = None;
                    }
                }
                _ => {}
            }
            continue;
        }
        match t {
            Token::Text(x) => w.text(x),
            Token::Open { name, attrs, self_closing } => {
                let name = name.as_str();
                if DROP.contains(&name) || name == "title" || attr(attrs, "hidden").is_some() || attr(attrs, "aria-hidden") == Some("true")
                {
                    if !self_closing && !VOID.contains(&name) {
                        drop_depth = Some((name.to_string(), 1));
                    }
                    continue;
                }
                open(&mut w, name, attrs, base);
            }
            Token::Close(name) => close(&mut w, name),
        }
    }
    finish(w.out)
}

fn open(w: &mut Writer, name: &str, attrs: &[(String, String)], base: Option<&Url>) {
    match name {
        "br" => {
            w.want(2);
            w.flush();
        }
        "hr" => {
            w.want(3);
            w.push_raw("---");
            w.want(3);
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            w.want(3);
            let n = name.as_bytes()[1] - b'0';
            w.push_raw(&format!("{} ", "#".repeat(n as usize)));
        }
        "pre" => {
            w.want(3);
            w.push_raw("```\n");
            w.pre += 1;
        }
        "code" if w.pre == 0 => w.push_raw("`"),
        "strong" | "b" if w.pre == 0 => w.push_raw("**"),
        "em" | "i" if w.pre == 0 => w.push_raw("*"),
        "blockquote" => {
            w.want(3);
            w.quote += 1;
        }
        "ul" => {
            w.want(2);
            w.lists.push(None);
        }
        "ol" => {
            w.want(2);
            let start = attr(attrs, "start").and_then(|s| s.parse().ok()).unwrap_or(1);
            w.lists.push(Some(start));
        }
        "li" => {
            w.want(2);
            let indent = "  ".repeat(w.lists.len().saturating_sub(1));
            let marker = match w.lists.last_mut() {
                Some(Some(n)) => {
                    let m = format!("{n}. ");
                    *n += 1;
                    m
                }
                _ => "- ".to_string(),
            };
            w.push_raw(&format!("{indent}{marker}"));
        }
        "a" => {
            if let Some(href) = attr(attrs, "href").filter(|h| !h.is_empty() && !h.starts_with("javascript:")) {
                let target = if href.starts_with('#') { href.to_string() } else { resolve(base, href) };
                w.push_raw("[");
                w.link = Some((target, w.out.len()));
            }
        }
        "img" => {
            let alt = attr(attrs, "alt").unwrap_or("").trim();
            if !alt.is_empty() {
                let src = attr(attrs, "src").map(|s| resolve(base, s)).unwrap_or_default();
                w.push_raw(&format!("![{alt}]({src})"));
            }
        }
        "td" | "th" => w.push_raw("| "),
        "tr" | "dt" => w.want(2),
        "dd" => {
            w.want(2);
            w.push_raw(": ");
        }
        _ if BLOCK.contains(&name) => w.want(3),
        _ => {}
    }
}

fn close(w: &mut Writer, name: &str) {
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" => w.want(3),
        "pre" if w.pre > 0 => {
            w.pre -= 1;
            if !w.out.ends_with('\n') {
                w.out.push('\n');
            }
            w.out.push_str("```");
            w.want(3);
        }
        "code" if w.pre == 0 => w.push_raw("`"),
        "strong" | "b" if w.pre == 0 => w.push_raw("**"),
        "em" | "i" if w.pre == 0 => w.push_raw("*"),
        "blockquote" => {
            w.quote = w.quote.saturating_sub(1);
            w.want(3);
        }
        "ul" | "ol" => {
            w.lists.pop();
            w.want(3);
        }
        "a" => {
            if let Some((target, at)) = w.link.take() {
                let text_empty = w.out[at..].trim().is_empty();
                if text_empty {
                    w.out.push_str(&target);
                }
                w.out.push_str(&format!("]({target})"));
            }
        }
        "td" | "th" => w.want(1),
        "li" => w.want(2),
        "tr" => {
            w.push_raw("|");
            w.want(2);
        }
        _ if BLOCK.contains(&name) => w.want(3),
        _ => {}
    }
}

/// Trim line ends, collapse runs of blank lines, drop empty emphasis markers.
fn finish(s: String) -> String {
    let s = s.replace("****", "");
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    let mut in_fence = false;
    for line in s.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let line = if in_fence { line } else { line.trim_end() };
        if line.trim().is_empty() && !in_fence {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        to_markdown(html, Url::parse("https://example.com/docs/page.html").ok().as_ref()).1
    }

    #[test]
    fn structure_becomes_markdown() {
        let html = r#"<!doctype html><html><head><title> The &amp; Title </title><style>p{color:red}</style>
            <script>var x = "<p>not text</p>";</script></head><body>
            <h1>Hello&nbsp;world</h1><p>First <b>bold</b> and <em>soft</em>
            paragraph with <a href="/a?b=1&amp;c=2">a link</a> and <code>x &lt; y</code>.</p>
            <ul><li>one</li><li>two<ol><li>nested</li></ol></li></ul>
            <pre>  keep
    spacing &lt;3</pre><blockquote><p>quoted</p></blockquote>
            <table><tr><th>k</th><th>v</th></tr><tr><td>a</td><td>1</td></tr></table>
            <img src="i.png" alt="diagram"><!-- a comment --></body></html>"#;
        let (title, text) = to_markdown(html, Url::parse("https://example.com/docs/page.html").ok().as_ref());
        assert_eq!(title.as_deref(), Some("The & Title"));
        for want in [
            "# Hello world",
            "First **bold** and *soft* paragraph with [a link](https://example.com/a?b=1&c=2) and `x < y`.",
            "- one\n- two\n  1. nested",
            "```\n  keep\n    spacing <3\n```",
            "> quoted",
            "| k | v |\n| a | 1 |",
            "![diagram](https://example.com/docs/i.png)",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
        assert!(!text.contains("not text") && !text.contains("color") && !text.contains("comment"), "{text}");
    }

    #[test]
    fn main_content_wins_over_navigation() {
        let text = md("<body><nav><a href='/'>Home</a></nav><main><h2>Article</h2><p>Body text.</p></main><footer>© 2026</footer></body>");
        assert!(text.contains("## Article") && text.contains("Body text."), "{text}");
        assert!(!text.contains("Home") && !text.contains("2026"), "{text}");
        let text = md("<body><main>  </main><p>outside</p></body>");
        assert!(text.contains("outside"), "an empty <main> falls back to the whole page: {text}");
    }

    #[test]
    fn entities_and_broken_markup_are_tolerated() {
        assert_eq!(decode_entities("a &lt;b&gt; &#65;&#x42; &unknown; & done &mdash;"), "a <b> AB &unknown; & done —");
        let text = md("<p>unclosed <b>bold <p>next < 3 and 5 > 4");
        assert!(text.contains("unclosed **bold") && text.contains("next < 3 and 5 > 4"), "{text}");
    }
}
