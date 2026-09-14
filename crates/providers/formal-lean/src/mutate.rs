//! Textual mutants of the *definitions* in a Lean file (never the theorem statements or their
//! proofs): a caught mutant is one under which some proof no longer goes through.
//! Operators: `<`↔`≤`, `>`↔`≥` (ASCII `<=`/`>=` too), `+`↔`-`, `&&`↔`||`, `true`↔`false`.

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutant {
    pub name: String,
    pub description: String,
    pub source: String,
    /// 1-based line that changed (to tell a broken proof from an ill-typed mutant).
    pub line: u32,
}

const DECL: &[&str] = &["def", "abbrev", "instance", "theorem", "lemma", "example", "structure", "inductive", "class", "axiom", "opaque"];
const MODIFIERS: &[&str] = &["private", "protected", "noncomputable", "partial", "unsafe", "nonrec"];

/// `(keyword, name)` when `l` starts a declaration at column 0.
fn decl_head(l: &str) -> Option<(&str, &str)> {
    if l.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = l.split_whitespace().skip_while(|w| MODIFIERS.contains(w) || w.starts_with("@["));
    let kw = words.next()?;
    DECL.contains(&kw).then(|| (kw, words.next().unwrap_or("")))
}

/// Line ranges `[start, end)` of `def`/`abbrev` bodies (after `:=`) with the definition name.
fn def_bodies(lines: &[&str]) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match decl_head(lines[i]) {
            Some(("def" | "abbrev", name)) => {
                let name = name.trim_end_matches(':').to_string();
                let mut end = i + 1;
                while end < lines.len() && (lines[end].is_empty() || lines[end].starts_with(char::is_whitespace)) {
                    end += 1;
                }
                out.push((name, i, end));
                i = end;
            }
            _ => i += 1,
        }
    }
    out
}

/// Code part of a line: comments dropped, string contents blanked (byte lengths kept).
fn code_only(l: &str) -> String {
    let mut out = String::with_capacity(l.len());
    let mut in_str = false;
    let mut chars = l.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if in_str {
            if c == '\\' {
                if let Some((_, e)) = chars.next() {
                    out.push_str(&" ".repeat(e.len_utf8()));
                }
            } else if c == '"' {
                in_str = false;
            }
            out.push_str(&" ".repeat(c.len_utf8()));
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(' ');
            }
            '-' if chars.peek().map(|(_, n)| *n == '-').unwrap_or(false) => break,
            c => out.push(c),
        }
    }
    out
}

const SWAPS: &[(&str, &str)] =
    &[("≤", "<"), ("<=", "<"), ("≥", ">"), (">=", ">"), ("&&", "||"), ("||", "&&"), ("true", "false"), ("false", "true")];

pub fn mutants(src: &str) -> Vec<Mutant> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(src.to_string());
    for (name, start, end) in def_bodies(&lines) {
        let mut n = 0;
        let mut body_started = false;
        for i in start..end {
            let code = code_only(lines[i]);
            let from = if body_started {
                0
            } else if let Some(p) = code.find(":=") {
                body_started = true;
                p + 2
            } else if i > start && code.trim_start().starts_with('|') {
                // pattern-matching definitions: every arm's right-hand side
                body_started = true;
                code.find("=>").map(|p| p + 2).unwrap_or(code.len())
            } else {
                continue;
            };
            for (j, len, with) in sites(&code, from) {
                n += 1;
                let orig = &lines[i][j..j + len];
                let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                new_lines[i] = format!("{}{with}{}", &lines[i][..j], &lines[i][j + len..]);
                let mut source = new_lines.join("\n");
                source.push('\n');
                if seen.insert(source.clone()) {
                    out.push(Mutant {
                        name: format!("swap:{name}#{n}"),
                        description: format!("`{orig}` → `{with}` in {name} (line {})", i + 1),
                        source,
                        line: (i + 1) as u32,
                    });
                }
            }
        }
    }
    out
}

/// `(byte offset, length, replacement)` for every mutable operator from `from` on.
fn sites(code: &str, from: usize) -> Vec<(usize, usize, &'static str)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut j = from;
    let word = |k: usize| b.get(k).map(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.').unwrap_or(false);
    while j < b.len() {
        if !code.is_char_boundary(j) {
            j += 1;
            continue;
        }
        let rest = &code[j..];
        let mut hit = None;
        for (pat, with) in SWAPS {
            if rest.starts_with(pat) {
                let alpha = pat.chars().all(|c| c.is_ascii_alphabetic());
                if alpha && (j > 0 && word(j - 1) || word(j + pat.len())) {
                    continue;
                }
                hit = Some((pat.len(), *with));
                break;
            }
        }
        if hit.is_none() {
            let prev = if j == 0 { b' ' } else { b[j - 1] };
            let next = b.get(j + 1).copied().unwrap_or(b' ');
            hit = match b[j] {
                b'<' if !matches!(next, b'=' | b'-' | b'|' | b'>' | b'<' | b'$') && !matches!(prev, b'<' | b'-') => Some((1, "≤")),
                b'>' if !matches!(next, b'=' | b'>') && !matches!(prev, b'=' | b'-' | b'>' | b'<' | b'|') => Some((1, "≥")),
                b'+' if !matches!(next, b'+' | b'=') && prev == b' ' && next == b' ' => Some((1, "-")),
                b'-' if !matches!(next, b'-' | b'>' | b'=') && prev == b' ' && next == b' ' => Some((1, "+")),
                _ => None,
            };
        }
        match hit {
            Some((len, with)) => {
                out.push((j, len, with));
                j += len;
            }
            None => j += 1,
        }
    }
    out
}

/// Whether 1-based `line` falls inside a `theorem`/`lemma`/`example` (a broken proof) rather
/// than a definition (an ill-typed mutant).
pub fn in_proof(src: &str, line: u32) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    let upto = (line as usize).min(lines.len());
    lines[..upto].iter().rev().find_map(|l| decl_head(l)).map(|(kw, _)| matches!(kw, "theorem" | "lemma" | "example")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "import Aho.Basic\n\n-- total of the lines\ndef total (xs : List Nat) : Nat :=\n  xs.foldl (fun a b => a + b) 0\n\ndef ok (n : Nat) : Bool := n < 10 && true -- \"x < y\"\n\ntheorem total_nil : total [] = 0 := rfl\n\ntheorem ok_small : ok 3 = true := by decide\n";

    #[test]
    fn only_definition_bodies_are_mutated() {
        let ms = mutants(SRC);
        let names: Vec<&str> = ms.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["swap:total#1", "swap:ok#1", "swap:ok#2", "swap:ok#3"], "{ms:#?}");
        assert!(ms[0].source.contains("fun a b => a - b"));
        assert!(ms[1].source.contains("n ≤ 10 && true"));
        assert!(ms[2].source.contains("n < 10 || true"));
        assert!(ms[3].source.contains("n < 10 && false -- \"x < y\""), "comments are not mutated");
        assert!(ms.iter().all(|m| m.source.contains("theorem ok_small : ok 3 = true := by decide")));
        assert!(!names.iter().any(|n| n.contains("fun") || n.contains("=>")));
    }

    #[test]
    fn errors_are_attributed_to_proofs_or_definitions() {
        assert!(in_proof(SRC, 11));
        assert!(!in_proof(SRC, 5));
        assert!(!in_proof(SRC, 1));
    }
}
