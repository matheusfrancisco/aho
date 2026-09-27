//! Shell lexer for the blast-radius classifier (split out for the file-size ratchet).

// ───────────────────────────── lexer ─────────────────────────────

/// Stands in for text produced by a command/process substitution: never a readable target.
pub(super) const SUBST: char = '\u{1}';
pub(super) const MAX_DEPTH: u32 = 8;

#[derive(Clone, Debug, Default)]
pub(super) struct Word {
    pub(super) text: String,
    pub(super) heredoc: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Redir {
    /// `>`, `>|`, `&>`: truncates.
    Out,
    /// `>&` / `<&`: fd duplication unless the target is a file name.
    DupOrOut,
    Append,
    In,
    Heredoc,
    Herestring,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Seq,
    Pipe,
    Open,
    Close,
    Redir(Redir),
}

#[derive(Clone, Debug)]
pub(super) enum Tok {
    Word(Word),
    Op(Op),
}

pub(super) struct Lexed {
    pub(super) toks: Vec<Tok>,
    /// Bodies of `$(…)`, backticks, `<(…)`, `>(…)`: they run too.
    pub(super) subs: Vec<String>,
}

pub(super) fn capture_balanced(cs: &[char], mut i: usize, open: char, close: char) -> (String, usize) {
    // `i` points one past the opening delimiter.
    let mut depth = 1;
    let mut out = String::new();
    let mut quote: Option<char> = None;
    while i < cs.len() {
        let c = cs[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' && i + 1 < cs.len() {
                    out.push(c);
                    i += 1;
                    out.push(cs[i]);
                    i += 1;
                    continue;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c == '\\' && i + 1 < cs.len() {
                    out.push(c);
                    i += 1;
                    out.push(cs[i]);
                    i += 1;
                    continue;
                } else if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return (out, i + 1);
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    (out, i)
}

pub(super) fn capture_backtick(cs: &[char], mut i: usize) -> (String, usize) {
    let mut out = String::new();
    while i < cs.len() {
        match cs[i] {
            '`' => return (out, i + 1),
            '\\' if i + 1 < cs.len() => {
                out.push(cs[i + 1]);
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, i)
}

pub(super) fn read_heredoc(cs: &[char], mut i: usize, delim: &str, strip_tabs: bool) -> (String, usize) {
    let mut body = String::new();
    while i < cs.len() {
        let start = i;
        while i < cs.len() && cs[i] != '\n' {
            i += 1;
        }
        let line: String = cs[start..i].iter().collect();
        if i < cs.len() {
            i += 1;
        }
        let cmp = if strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
        if cmp == delim {
            return (body, i);
        }
        body.push_str(&line);
        body.push('\n');
    }
    (body, i)
}

pub(super) struct Lexer {
    pub(super) toks: Vec<Tok>,
    pub(super) subs: Vec<String>,
    pub(super) cur: String,
    pub(super) in_word: bool,
    pub(super) expect_delim: Option<bool>,
    pub(super) pending: Vec<(usize, String, bool)>,
}

impl Lexer {
    pub(super) fn flush(&mut self) {
        if !self.in_word {
            return;
        }
        let text = std::mem::take(&mut self.cur);
        self.in_word = false;
        if let Some(strip) = self.expect_delim.take() {
            self.pending.push((self.toks.len(), text.clone(), strip));
        }
        self.toks.push(Tok::Word(Word { text, heredoc: None }));
    }
    pub(super) fn op(&mut self, op: Op) {
        self.flush();
        self.toks.push(Tok::Op(op));
    }
}

pub(super) fn lex(src: &str) -> Lexed {
    let cs: Vec<char> = src.chars().collect();
    let mut lx = Lexer { toks: Vec::new(), subs: Vec::new(), cur: String::new(), in_word: false, expect_delim: None, pending: Vec::new() };
    let mut i = 0;
    let at = |k: usize| cs.get(k).copied();
    while i < cs.len() {
        let c = cs[i];
        match c {
            ' ' | '\t' | '\r' => {
                lx.flush();
                i += 1;
            }
            '\n' => {
                lx.op(Op::Seq);
                i += 1;
                for (idx, delim, strip) in std::mem::take(&mut lx.pending) {
                    let (body, next) = read_heredoc(&cs, i, &delim, strip);
                    if let Some(Tok::Word(w)) = lx.toks.get_mut(idx) {
                        w.heredoc = Some(body);
                    }
                    i = next;
                }
            }
            '#' if !lx.in_word => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                lx.in_word = true;
                i += 1;
                while i < cs.len() && cs[i] != '\'' {
                    lx.cur.push(cs[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                lx.in_word = true;
                i += 1;
                while i < cs.len() && cs[i] != '"' {
                    match cs[i] {
                        '\\' if matches!(at(i + 1), Some('"' | '\\' | '$' | '`' | '\n')) => {
                            if at(i + 1) != Some('\n') {
                                lx.cur.push(cs[i + 1]);
                            }
                            i += 2;
                        }
                        '$' if at(i + 1) == Some('(') => {
                            let (inner, next) = capture_balanced(&cs, i + 2, '(', ')');
                            lx.subs.push(inner);
                            lx.cur.push(SUBST);
                            i = next;
                        }
                        '`' => {
                            let (inner, next) = capture_backtick(&cs, i + 1);
                            lx.subs.push(inner);
                            lx.cur.push(SUBST);
                            i = next;
                        }
                        ch => {
                            lx.cur.push(ch);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '\\' => {
                lx.in_word = true;
                if let Some(n) = at(i + 1) {
                    if n != '\n' {
                        lx.cur.push(n);
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '$' if at(i + 1) == Some('(') => {
                lx.in_word = true;
                if at(i + 2) == Some('(') {
                    // Arithmetic `$(( … ))`: a number, not a command.
                    let (_, next) = capture_balanced(&cs, i + 3, '(', ')');
                    i = if at(next) == Some(')') { next + 1 } else { next };
                    lx.cur.push('0');
                } else {
                    let (inner, next) = capture_balanced(&cs, i + 2, '(', ')');
                    lx.subs.push(inner);
                    lx.cur.push(SUBST);
                    i = next;
                }
            }
            '$' if at(i + 1) == Some('{') => {
                lx.in_word = true;
                let (inner, next) = capture_balanced(&cs, i + 2, '{', '}');
                lx.cur.push_str("${");
                lx.cur.push_str(&inner);
                lx.cur.push('}');
                i = next;
            }
            '`' => {
                lx.in_word = true;
                let (inner, next) = capture_backtick(&cs, i + 1);
                lx.subs.push(inner);
                lx.cur.push(SUBST);
                i = next;
            }
            '<' | '>' if !lx.in_word && at(i + 1) == Some('(') => {
                let (inner, next) = capture_balanced(&cs, i + 2, '(', ')');
                lx.subs.push(inner);
                lx.in_word = true;
                lx.cur.push(SUBST);
                i = next;
            }
            '|' => match at(i + 1) {
                Some('|') => {
                    lx.op(Op::Seq);
                    i += 2;
                }
                Some('&') => {
                    lx.op(Op::Pipe);
                    i += 2;
                }
                _ => {
                    lx.op(Op::Pipe);
                    i += 1;
                }
            },
            '&' => match (at(i + 1), at(i + 2)) {
                (Some('&'), _) => {
                    lx.op(Op::Seq);
                    i += 2;
                }
                (Some('>'), Some('>')) => {
                    lx.op(Op::Redir(Redir::Append));
                    i += 3;
                }
                (Some('>'), _) => {
                    lx.op(Op::Redir(Redir::Out));
                    i += 2;
                }
                _ => {
                    lx.op(Op::Seq);
                    i += 1;
                }
            },
            ';' => {
                lx.op(Op::Seq);
                i += if at(i + 1) == Some(';') { 2 } else { 1 };
            }
            '(' => {
                lx.op(Op::Open);
                i += 1;
            }
            ')' => {
                lx.op(Op::Close);
                i += 1;
            }
            '>' | '<' => {
                // `2>` / `10<`: a leading all-digit word is the fd, not an argument.
                if lx.in_word && !lx.cur.is_empty() && lx.cur.chars().all(|d| d.is_ascii_digit()) {
                    lx.cur.clear();
                    lx.in_word = false;
                } else {
                    lx.flush();
                }
                let (op, len) = if c == '>' {
                    match at(i + 1) {
                        Some('>') => (Redir::Append, 2),
                        Some('|') => (Redir::Out, 2),
                        Some('&') => (Redir::DupOrOut, 2),
                        _ => (Redir::Out, 1),
                    }
                } else {
                    match (at(i + 1), at(i + 2)) {
                        (Some('<'), Some('<')) => (Redir::Herestring, 3),
                        (Some('<'), Some('-')) => {
                            lx.expect_delim = Some(true);
                            (Redir::Heredoc, 3)
                        }
                        (Some('<'), _) => {
                            lx.expect_delim = Some(false);
                            (Redir::Heredoc, 2)
                        }
                        (Some('&'), _) => (Redir::DupOrOut, 2),
                        (Some('>'), _) => (Redir::In, 2),
                        _ => (Redir::In, 1),
                    }
                };
                lx.toks.push(Tok::Op(Op::Redir(op)));
                i += len;
            }
            ch => {
                lx.in_word = true;
                lx.cur.push(ch);
                i += 1;
            }
        }
    }
    lx.flush();
    Lexed { toks: lx.toks, subs: lx.subs }
}
