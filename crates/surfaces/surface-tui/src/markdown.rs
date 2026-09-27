//! Assistant text as the terminal shows it: the markdown a model writes (headings, `**bold**`,
//! `*emphasis*`, `` `code` ``, fenced code blocks, lists, quotes) becomes styled rows, wrapped
//! by display width with styles kept across the wrap. Unclosed markers stay literal.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Splits one line into styled runs: `**bold**`, `*em*` / `_em_`, `` `code` ``.
fn inline(text: &str, base: Style, t: &Theme) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let flush = |plain: &mut String, out: &mut Vec<(String, Style)>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), base));
        }
    };
    let find = |from: usize, pat: &[char]| (from..chars.len()).find(|&j| chars[j..].starts_with(pat));
    while i < chars.len() {
        let c = chars[i];
        let (open, style): (&[char], Style) = match c {
            '`' => (&['`'], t.accent),
            '*' if chars.get(i + 1) == Some(&'*') => (&['*', '*'], base.add_modifier(Modifier::BOLD)),
            '*' | '_' if chars.get(i + 1).is_some_and(|n| !n.is_whitespace()) => {
                (if c == '*' { &['*'] } else { &['_'] }, base.add_modifier(Modifier::ITALIC))
            }
            _ => {
                plain.push(c);
                i += 1;
                continue;
            }
        };
        match find(i + open.len(), open) {
            Some(end) if end > i + open.len() => {
                flush(&mut plain, &mut out);
                out.push((chars[i + open.len()..end].iter().collect(), style));
                i = end + open.len();
            }
            _ => {
                plain.push(c);
                i += 1;
            }
        }
    }
    flush(&mut plain, &mut out);
    out
}

/// Greedy word wrap of styled runs to `width` columns; the first row gets `first`, the rest
/// `rest` as their prefix.
fn wrap_runs(runs: &[(String, Style)], width: usize, first: Span<'static>, rest: Span<'static>, fill: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = vec![first];
    let pw = |s: &Span<'_>| UnicodeWidthStr::width(s.content.as_ref());
    let mut w = pw(&cur[0]);
    let start = w;
    let rest_w = pw(&rest);
    for (text, style) in runs {
        for word in text.split_inclusive(' ') {
            let ww = UnicodeWidthStr::width(word);
            if w + ww > width && w > start.max(rest_w) {
                let row = std::mem::replace(&mut cur, vec![rest.clone()]);
                lines.push(Line::from(row).style(fill));
                w = rest_w;
            }
            cur.push(Span::styled(word.to_string(), *style));
            w += ww;
        }
    }
    lines.push(Line::from(cur).style(fill));
    lines
}

/// Assistant text → rows.
pub fn render(text: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows = Vec::new();
    let mut fence: Option<String> = None;
    for raw in text.replace('\t', "    ").split('\n') {
        let trimmed = raw.trim_start();
        if let Some(lang) = trimmed.strip_prefix("```") {
            match fence.take() {
                Some(_) => {}
                None => {
                    fence = Some(lang.trim().to_string());
                    if !lang.trim().is_empty() {
                        rows.push(Line::from(Span::styled(format!("  {}", lang.trim()), t.dim)).style(t.tool_pending));
                    }
                }
            }
            continue;
        }
        if fence.is_some() {
            // Code keeps its spacing: hard-wrapped, tinted, never re-flowed.
            let mut rest = raw;
            loop {
                let (head, tail) = split_width(rest, width.saturating_sub(2));
                rows.push(Line::from(vec![Span::styled("  ", t.dim), Span::styled(head.to_string(), t.text)]).style(t.tool_pending));
                if tail.is_empty() {
                    break;
                }
                rest = tail;
            }
            continue;
        }
        if trimmed.is_empty() {
            rows.push(Line::default());
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let runs = inline(trimmed[hashes..].trim(), t.title, t);
            rows.extend(wrap_runs(&runs, width, Span::raw(""), Span::raw(""), Style::default()));
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let pad = " ".repeat(indent.min(8));
        let bullet = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ "));
        let numbered = trimmed.split_once(". ").filter(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()));
        let (first, rest, body, base) = if let Some(b) = bullet {
            (format!("{pad}• "), format!("{pad}  "), b, t.text)
        } else if let Some((n, b)) = numbered {
            (format!("{pad}{n}. "), format!("{pad}{}", " ".repeat(n.len() + 2)), b, t.text)
        } else if let Some(q) = trimmed.strip_prefix("> ") {
            ("│ ".to_string(), "│ ".to_string(), q, t.muted)
        } else {
            (pad.clone(), pad, trimmed, t.text)
        };
        let marker = if bullet.is_some() || numbered.is_some() { t.accent } else { t.dim };
        let runs = inline(body, base, t);
        rows.extend(wrap_runs(&runs, width, Span::styled(first, marker), Span::styled(rest, marker), Style::default()));
    }
    rows
}

/// The longest prefix of `s` that fits `width` columns, and the rest.
fn split_width(s: &str, width: usize) -> (&str, &str) {
    let mut w = 0;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > width.max(1) {
            return (&s[..i], &s[i..]);
        }
        w += cw;
    }
    (s, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn markers_become_styles_and_unclosed_ones_stay_literal() {
        let t = Theme::default();
        let rows = render("The file says **alpha** and `x = 1`; 2 * 3 is *six*, a **dangling one", 200, &t);
        assert_eq!(text(&rows), ["The file says alpha and x = 1; 2 * 3 is six, a **dangling one"]);
        let bold = rows[0].spans.iter().find(|s| s.content == "alpha").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code: Vec<&Span<'_>> = rows[0].spans.iter().filter(|s| s.style == t.accent).collect();
        assert_eq!(code.iter().map(|s| s.content.as_ref()).collect::<String>(), "x = 1", "code keeps its style across words");
        assert!(rows[0].spans.iter().any(|s| s.content == "six" && s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn headings_lists_quotes_and_code_blocks_keep_their_shape_when_wrapped() {
        let t = Theme::default();
        let md = "# Plan\n- first step that is long enough to wrap around\n2. second\n> quoted\n```rust\nfn main() {}\n```\nafter";
        let rows = text(&render(md, 24, &t));
        assert_eq!(
            rows,
            [
                "Plan",
                "• first step that is",
                "  long enough to wrap",
                "  around",
                "2. second",
                "│ quoted",
                "  rust",
                "  fn main() {}",
                "after",
            ]
        );
    }
}
