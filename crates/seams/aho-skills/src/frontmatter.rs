//! The YAML subset SKILL.md frontmatter actually uses, parsed by hand (no YAML dependency).
//!
//! Supported: `key: value` at column 1, plain / `"double"` / `'single'` scalars (plain scalars
//! may continue on indented lines), `|` and `>` block scalars, `[a, b]` flow lists, block lists
//! (`- item`, indented or at column 1), and nested mappings, which are recognised and skipped so a
//! dropped key takes its whole block with it. Comments (`# …`) and blank lines are ignored. Line
//! numbers are file line numbers (the opening `---` is line 1).

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Empty,
    Scalar(String),
    List(Vec<String>),
    /// A mapping or flow map; contents are not interpreted.
    Nested,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub key: String,
    pub line: u32,
    pub value: Val,
}

#[derive(Debug)]
pub(crate) struct Doc {
    pub entries: Vec<Entry>,
    pub body: String,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ParseErr {
    pub line: u32,
    pub msg: String,
}

fn err(line: u32, msg: impl Into<String>) -> ParseErr {
    ParseErr { line, msg: msg.into() }
}

/// `Ok(None)` when the text does not start with a `---` fence.
pub(crate) fn parse(text: &str) -> Result<Option<Doc>, ParseErr> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return Ok(None);
    }
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| matches!(l.trim_end(), "---" | "..."))
        .map(|(i, _)| i)
        .ok_or_else(|| err(1, "the frontmatter opened here is never closed with `---`"))?;
    let entries = parse_block(&lines[1..close], 2)?;
    let body = lines[close + 1..].join("\n");
    Ok(Some(Doc { entries, body }))
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn is_skippable(l: &str) -> bool {
    let t = l.trim();
    t.is_empty() || t.starts_with('#')
}

fn is_list_item(t: &str) -> bool {
    t == "-" || t.starts_with("- ")
}

fn parse_block(lines: &[&str], first_line: u32) -> Result<Vec<Entry>, ParseErr> {
    let mut out: Vec<Entry> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = first_line + i as u32;
        if raw.trim_start_matches(' ').starts_with('\t') {
            return Err(err(line, "tabs are not allowed for indentation; use spaces"));
        }
        if is_skippable(raw) {
            i += 1;
            continue;
        }
        if indent(raw) > 0 {
            return Err(err(line, "unexpected indentation; top-level keys start at column 1"));
        }
        if is_list_item(raw.trim_end()) {
            return Err(err(line, "a list item needs a `key:` line above it"));
        }
        let (key, rest) = split_key(raw).ok_or_else(|| err(line, "expected `key: value`"))?;
        let rest = strip_comment(rest.trim());
        if let Some(prev) = out.iter().find(|e| e.key == key) {
            return Err(err(line, format!("`{key}` appears twice (lines {} and {line})", prev.line)));
        }
        i += 1;
        let start = i;
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() || indent(l) > 0 || (rest.is_empty() && is_list_item(l.trim_end())) {
                i += 1;
                continue;
            }
            if l.trim_start().starts_with('#') && rest.is_empty() {
                i += 1;
                continue;
            }
            break;
        }
        let block = &lines[start..i];
        let block_line = first_line + start as u32;
        let value = if rest.is_empty() {
            block_value(block, block_line)?
        } else if matches!(rest, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            Val::Scalar(block_scalar(block, rest.starts_with('>')))
        } else {
            let mut joined = rest.to_string();
            for l in block.iter().filter(|l| !is_skippable(l)) {
                joined.push(' ');
                joined.push_str(strip_comment(l.trim()));
            }
            inline_value(&joined, line)?
        };
        out.push(Entry { key, line, value });
    }
    Ok(out)
}

/// `key: rest`; the colon must be followed by whitespace or end the line (so `a:b` is not a key).
fn split_key(raw: &str) -> Option<(String, &str)> {
    let first = raw.chars().next()?;
    if first == '"' || first == '\'' {
        let close = raw[1..].find(first)? + 1;
        let key = raw[1..close].to_string();
        let rest = raw[close + 1..].trim_start().strip_prefix(':')?;
        return Some((key, rest));
    }
    let bytes = raw.as_bytes();
    let pos = (0..bytes.len()).find(|&i| bytes[i] == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ' || bytes[i + 1] == b'\t'))?;
    let key = raw[..pos].trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), &raw[pos + 1..]))
}

/// Cuts a ` # comment` off a plain value (quoted values are handled by their own scanners).
fn strip_comment(s: &str) -> &str {
    if s.starts_with('#') {
        return "";
    }
    if s.starts_with('"') || s.starts_with('\'') || s.starts_with('[') || s.starts_with('{') {
        return s;
    }
    match s.find(" #") {
        Some(p) => s[..p].trim_end(),
        None => s,
    }
}

fn block_value(block: &[&str], first_line: u32) -> Result<Val, ParseErr> {
    let content: Vec<(u32, &str)> =
        block.iter().enumerate().filter(|(_, l)| !is_skippable(l)).map(|(i, l)| (first_line + i as u32, *l)).collect();
    let Some((_, first)) = content.first() else { return Ok(Val::Empty) };
    if !is_list_item(first.trim()) {
        return Ok(Val::Nested);
    }
    let item_indent = indent(first);
    let mut items: Vec<String> = Vec::new();
    for (line, l) in content {
        let t = l.trim();
        if indent(l) == item_indent && is_list_item(t) {
            let item = t.strip_prefix('-').unwrap_or("").trim();
            items.push(unquote(strip_comment(item), line)?);
        } else if let Some(last) = items.last_mut() {
            // continuation of the previous item (folded) or a nested map inside it
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(strip_comment(t));
        }
    }
    Ok(Val::List(items))
}

fn block_scalar(block: &[&str], folded: bool) -> String {
    let min = block.iter().filter(|l| !l.trim().is_empty()).map(|l| indent(l)).min().unwrap_or(0);
    let lines: Vec<&str> = block.iter().map(|l| if l.len() >= min { &l[min..] } else { l.trim() }).collect();
    let text = if folded {
        let mut out = String::new();
        let mut prev_blank = true;
        for l in lines {
            if l.trim().is_empty() {
                out.push('\n');
                prev_blank = true;
            } else {
                if !prev_blank {
                    out.push(' ');
                }
                out.push_str(l.trim_end());
                prev_blank = false;
            }
        }
        out
    } else {
        lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
    };
    text.trim().to_string()
}

fn inline_value(s: &str, line: u32) -> Result<Val, ParseErr> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or_else(|| err(line, "a `[` list must end with `]` on the same line"))?;
        let mut items = Vec::new();
        for part in split_flow(inner, line)? {
            let part = part.trim();
            if !part.is_empty() {
                items.push(unquote(part, line)?);
            }
        }
        return Ok(Val::List(items));
    }
    if s.starts_with('{') {
        if !s.ends_with('}') {
            return Err(err(line, "a `{` mapping must end with `}`"));
        }
        return Ok(Val::Nested);
    }
    Ok(Val::Scalar(unquote(s, line)?))
}

/// Splits a flow list on commas outside quotes and brackets.
fn split_flow(s: &str, line: u32) -> Result<Vec<&str>, ParseErr> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' | '(' => depth += 1,
                ']' | '}' | ')' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err(err(line, "a quoted value is never closed"));
    }
    parts.push(&s[start..]);
    Ok(parts)
}

fn unquote(s: &str, line: u32) -> Result<String, ParseErr> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        loop {
            match chars.next() {
                None => return Err(err(line, "a `\"` value is never closed")),
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c @ ('"' | '\\' | '/')) => out.push(c),
                    Some(c) => {
                        out.push('\\');
                        out.push(c);
                    }
                    None => return Err(err(line, "a `\"` value is never closed")),
                },
                Some(c) => out.push(c),
            }
        }
        let rest = chars.as_str().trim();
        if !rest.is_empty() && !rest.starts_with('#') {
            return Err(err(line, format!("unexpected text `{rest}` after a quoted value")));
        }
        return Ok(out);
    }
    if let Some(inner) = s.strip_prefix('\'') {
        let mut out = String::new();
        let mut rest = inner;
        loop {
            let Some(p) = rest.find('\'') else { return Err(err(line, "a `'` value is never closed")) };
            out.push_str(&rest[..p]);
            rest = &rest[p + 1..];
            if let Some(r) = rest.strip_prefix('\'') {
                out.push('\'');
                rest = r;
            } else {
                break;
            }
        }
        let rest = rest.trim();
        if !rest.is_empty() && !rest.starts_with('#') {
            return Err(err(line, format!("unexpected text `{rest}` after a quoted value")));
        }
        return Ok(out);
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(fm: &str) -> Vec<Entry> {
        parse(&format!("---\n{fm}---\nbody\n")).unwrap().unwrap().entries
    }

    #[test]
    fn scalars_lists_blocks_and_nested_maps() {
        let e = entries(
            "name: a # trailing comment\nq: \"x: y # not a comment\"\ns: 'it''s'\nflow: [a, \"b, c\", 'd']\nlist:\n  - one\n  - \"two\"\ntop:\n- x\n- y\nlit: |\n  line 1\n  line 2\nfold: >\n  a\n  b\n\n  c\nmap:\n  k: v\n  deeper:\n    - z\nplain: first\n  second\nempty:\n",
        );
        let v = |k: &str| e.iter().find(|x| x.key == k).unwrap().value.clone();
        assert_eq!(v("name"), Val::Scalar("a".into()));
        assert_eq!(v("q"), Val::Scalar("x: y # not a comment".into()));
        assert_eq!(v("s"), Val::Scalar("it's".into()));
        assert_eq!(v("flow"), Val::List(vec!["a".into(), "b, c".into(), "d".into()]));
        assert_eq!(v("list"), Val::List(vec!["one".into(), "two".into()]));
        assert_eq!(v("top"), Val::List(vec!["x".into(), "y".into()]));
        assert_eq!(v("lit"), Val::Scalar("line 1\nline 2".into()));
        assert_eq!(v("fold"), Val::Scalar("a b\nc".into()));
        assert_eq!(v("map"), Val::Nested);
        assert_eq!(v("plain"), Val::Scalar("first second".into()));
        assert_eq!(v("empty"), Val::Empty);
        assert_eq!(e.iter().find(|x| x.key == "lit").unwrap().line, 12);
    }

    #[test]
    fn errors_carry_file_line_numbers() {
        assert_eq!(parse("---\nname: a\n  stray\n").unwrap_err(), err(1, "the frontmatter opened here is never closed with `---`"));
        assert_eq!(parse("---\nname: a\nnope\n---\n").unwrap_err().line, 3);
        assert_eq!(parse("---\na: 1\na: 2\n---\n").unwrap_err().msg, "`a` appears twice (lines 2 and 3)");
        assert_eq!(parse("---\nx: [a, b\n---\n").unwrap_err().line, 2);
        assert_eq!(parse("---\n  x: 1\n---\n").unwrap_err().line, 2);
        assert!(parse("# no frontmatter\n").unwrap().is_none());
    }
}
