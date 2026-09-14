//! Textual mutants of a TLA+ module (13: "must fail on a mutant"). Only the *model* is mutated
//! — the actions reachable from `Next` — never the invariants, so a caught mutant means the
//! invariants constrain the behavior the actions allow.
//!
//! Operators: drop a guard (`/\ P` → `/\ TRUE`), negate a guard (`/\ ~(P)`), swap a comparison
//! (`<` ↔ `<=`, `>` ↔ `>=`), and remove one variable from an `UNCHANGED <<…>>` frame. Guards
//! are the top-level conjunction bullets with no primed variable.

use std::collections::{BTreeSet, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutant {
    pub name: String,
    pub description: String,
    pub source: String,
}

#[derive(Clone, Debug)]
struct Def {
    name: String,
    /// 0-based line of the header (`Name(args) ==`).
    start: usize,
    /// Exclusive end line.
    end: usize,
}

/// Top-level definitions: a line at column 0 of the form `Name ==` / `Name(args) ==`.
fn definitions(lines: &[&str]) -> Vec<Def> {
    let mut heads: Vec<(usize, String)> = Vec::new();
    let mut boundaries: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with(char::is_whitespace) || l.is_empty() {
            continue;
        }
        boundaries.push(i);
        let name: String = l.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if name.is_empty() {
            continue;
        }
        let rest = l[name.len()..].trim_start();
        let rest = if rest.starts_with('(') { rest.split_once(')').map(|(_, t)| t.trim_start()).unwrap_or("") } else { rest };
        if rest.starts_with("==") {
            heads.push((i, name));
        }
    }
    heads
        .into_iter()
        .map(|(start, name)| {
            let end = boundaries.iter().copied().find(|b| *b > start).unwrap_or(lines.len());
            Def { name, start, end }
        })
        .collect()
}

/// Removes `\* …` comments and string literals so scans see only code.
fn code_only(l: &str) -> String {
    let mut out = String::with_capacity(l.len());
    let mut in_str = false;
    let mut chars = l.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if c == '\\' {
                if let Some(e) = chars.next() {
                    out.push_str(&" ".repeat(e.len_utf8()));
                }
            } else if c == '"' {
                in_str = false;
            }
            out.push_str(&" ".repeat(c.len_utf8()));
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(' ');
            continue;
        }
        if c == '\\' && chars.peek() == Some(&'*') {
            break;
        }
        out.push(c);
    }
    out
}

fn idents(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

/// Definitions reachable from `next` whose text contains a prime or `UNCHANGED` (actions).
fn actions<'a>(lines: &[&str], defs: &'a [Def], next: &str) -> Vec<&'a Def> {
    let mut seen: BTreeSet<&'a str> = BTreeSet::new();
    let mut stack: Vec<&'a Def> = defs.iter().filter(|d| d.name == next).collect();
    while let Some(d) = stack.pop() {
        if !seen.insert(d.name.as_str()) {
            continue;
        }
        let body: String = lines[d.start..d.end].iter().map(|l| code_only(l)).collect::<Vec<_>>().join("\n");
        let after_head = body.find("==").map(|p| p + 2).unwrap_or(body.len());
        for w in idents(&body[after_head..]) {
            if let Some(x) = defs.iter().find(|x| x.name == w)
                && !seen.contains(x.name.as_str())
            {
                stack.push(x);
            }
        }
    }
    defs.iter()
        .filter(|d| seen.contains(d.name.as_str()))
        .filter(|d| {
            let body: String = lines[d.start..d.end].iter().map(|l| code_only(l)).collect();
            body.contains('\'') || body.contains("UNCHANGED")
        })
        .collect()
}

#[derive(Clone, Debug)]
struct Bullet {
    /// Line and byte column of the `/\`.
    line: usize,
    col: usize,
    /// Exclusive end line.
    end: usize,
}

/// Top-level `/\` bullets of a definition: those at the smallest column any bullet starts at.
fn bullets(lines: &[&str], d: &Def) -> Vec<Bullet> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(d.end).skip(d.start) {
        let code = code_only(line);
        let from = if i == d.start { code.find("==").map(|p| p + 2).unwrap_or(code.len()) } else { 0 };
        let rest = &code[from..];
        let trimmed = rest.trim_start();
        if trimmed.starts_with("/\\") {
            starts.push((i, from + (rest.len() - trimmed.len())));
        }
    }
    let Some(col) = starts.iter().map(|s| s.1).min() else { return Vec::new() };
    let top: Vec<(usize, usize)> = starts.into_iter().filter(|s| s.1 == col).collect();
    top.iter()
        .map(|&(line, c)| {
            let end = (line + 1..d.end)
                .find(|&j| {
                    let code = code_only(lines[j]);
                    let indent = code.len() - code.trim_start().len();
                    !code.trim().is_empty() && indent <= c
                })
                .unwrap_or(d.end);
            Bullet { line, col: c, end }
        })
        .collect()
}

fn bullet_code(lines: &[&str], b: &Bullet) -> String {
    (b.line..b.end).map(|i| code_only(lines[i])).collect::<Vec<_>>().join("\n")[b.col..].to_string()
}

fn replace_lines(lines: &[&str], from: usize, to: usize, with: &[String]) -> String {
    let mut out: Vec<String> = lines[..from].iter().map(|s| s.to_string()).collect();
    out.extend(with.iter().cloned());
    out.extend(lines[to..].iter().map(|s| s.to_string()));
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Every mutant of `src`'s actions reachable from `next`, deduplicated by resulting text, in
/// the order a capped run should spend its budget: guards of every action first, then
/// comparisons, then frames (a removed `UNCHANGED` variable usually leaves the successor
/// unspecified, which TLC reports as an error rather than a verdict).
pub fn mutants(src: &str, next: &str) -> Vec<Mutant> {
    let mut out = generate(src, next);
    let rank = |m: &Mutant| match m.name.split(':').next().unwrap_or("") {
        "drop-guard" | "negate-guard" => 0,
        "swap-cmp" => 1,
        _ => 2,
    };
    out.sort_by_key(rank);
    out
}

fn generate(src: &str, next: &str) -> Vec<Mutant> {
    let lines: Vec<&str> = src.lines().collect();
    let defs = definitions(&lines);
    let acts = actions(&lines, &defs, next);
    let mut out: Vec<Mutant> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(src.to_string());
    let mut push = |m: Mutant, out: &mut Vec<Mutant>| {
        if seen.insert(m.source.clone()) {
            out.push(m);
        }
    };

    for d in &acts {
        let bs = bullets(&lines, d);
        let mut guard_no = 0;
        for b in &bs {
            let code = bullet_code(&lines, b);
            let body = code.trim_start_matches("/\\").trim();
            if body.starts_with("UNCHANGED") {
                for m in unchanged_mutants(&lines, d, b) {
                    push(m, &mut out);
                }
                continue;
            }
            if code.contains('\'') {
                continue;
            }
            guard_no += 1;
            // drop: the bullet becomes TRUE (keeps the list well-formed wherever it sits)
            let first = lines[b.line];
            let prefix = &first[..b.col];
            let dropped = vec![format!("{prefix}/\\ TRUE")];
            push(
                Mutant {
                    name: format!("drop-guard:{}#{guard_no}", d.name),
                    description: format!("dropped guard {guard_no} of {} (`{}`)", d.name, one_line(body)),
                    source: replace_lines(&lines, b.line, b.end, &dropped),
                },
                &mut out,
            );
            // negate: /\ ~( … ) around the whole bullet
            let mut negated: Vec<String> = (b.line..b.end).map(|i| lines[i].to_string()).collect();
            let head = &negated[0][b.col..];
            let after = head.trim_start_matches("/\\");
            let pad = &after[..after.len() - after.trim_start().len()];
            negated[0] = format!("{prefix}/\\{pad}~({}", after.trim_start());
            let last = negated.len() - 1;
            let code_len = code_only(&negated[last]).trim_end().len();
            let (code, comment) = negated[last].split_at(code_len);
            negated[last] = format!("{code}){comment}");
            push(
                Mutant {
                    name: format!("negate-guard:{}#{guard_no}", d.name),
                    description: format!("negated guard {guard_no} of {} (`{}`)", d.name, one_line(body)),
                    source: replace_lines(&lines, b.line, b.end, &negated),
                },
                &mut out,
            );
        }
        for m in comparison_mutants(&lines, d) {
            push(m, &mut out);
        }
    }
    out
}

fn one_line(s: &str) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 60 { format!("{}…", t.chars().take(60).collect::<String>()) } else { t }
}

/// One mutant per variable of a literal `UNCHANGED <<a, b>>` / `UNCHANGED a` frame.
fn unchanged_mutants(lines: &[&str], d: &Def, b: &Bullet) -> Vec<Mutant> {
    let line = lines[b.line];
    let code = code_only(line);
    let Some(u) = code[b.col..].find("UNCHANGED").map(|p| p + b.col) else { return Vec::new() };
    let after = &code[u + "UNCHANGED".len()..];
    let trimmed = after.trim_start();
    let expr_start = u + "UNCHANGED".len() + (after.len() - trimmed.len());
    let (vars, expr_end): (Vec<&str>, usize) = if let Some(inner) = trimmed.strip_prefix("<<") {
        let Some(close) = inner.find(">>") else { return Vec::new() };
        (inner[..close].split(',').map(str::trim).filter(|v| !v.is_empty()).collect(), expr_start + 2 + close + 2)
    } else {
        let v: String = trimmed.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let len = v.len();
        (vec![&trimmed[..len]], expr_start + len)
    };
    if vars.iter().any(|v| !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return Vec::new();
    }
    vars.iter()
        .map(|v| {
            let rest: Vec<&str> = vars.iter().copied().filter(|x| x != v).collect();
            let replacement = if rest.is_empty() {
                // the whole frame goes: `/\ UNCHANGED x` → `/\ TRUE`
                format!("{}/\\ TRUE{}", &line[..b.col], &line[expr_end..])
            } else {
                format!("{}UNCHANGED <<{}>>{}", &line[..u], rest.join(", "), &line[expr_end..])
            };
            let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            new_lines[b.line] = replacement;
            let mut source = new_lines.join("\n");
            source.push('\n');
            Mutant { name: format!("unchanged:{}-{v}", d.name), description: format!("{} no longer keeps `{v}` unchanged", d.name), source }
        })
        .collect()
}

/// One mutant per `<`, `<=`, `>`, `>=` (and `=<`, `\leq`, `\geq`) in the action's code.
fn comparison_mutants(lines: &[&str], d: &Def) -> Vec<Mutant> {
    let mut out = Vec::new();
    let mut n = 0;
    for i in d.start..d.end {
        let code = code_only(lines[i]);
        let from = if i == d.start { code.find("==").map(|p| p + 2).unwrap_or(code.len()) } else { 0 };
        let b = code.as_bytes();
        let mut j = from;
        while j < b.len() {
            let found = cmp_at(b, j);
            if let Some((len, with)) = found {
                n += 1;
                let orig = &code[j..j + len];
                let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                new_lines[i] = format!("{}{with}{}", &lines[i][..j], &lines[i][j + len..]);
                let mut source = new_lines.join("\n");
                source.push('\n');
                out.push(Mutant {
                    name: format!("swap-cmp:{}#{n}", d.name),
                    description: format!("`{orig}` → `{with}` in {} (line {})", d.name, i + 1),
                    source,
                });
                j += len;
            } else {
                j += 1;
            }
        }
    }
    out
}

/// A comparison operator starting at `j` → `(length, replacement)`.
fn cmp_at(b: &[u8], j: usize) -> Option<(usize, &'static str)> {
    let at = |k: usize| b.get(k).copied().unwrap_or(b' ');
    let prev = if j == 0 { b' ' } else { b[j - 1] };
    let rest = &b[j..];
    if rest.starts_with(b"\\leq") && !at(j + 4).is_ascii_alphanumeric() {
        return Some((4, "<"));
    }
    if rest.starts_with(b"\\geq") && !at(j + 4).is_ascii_alphanumeric() {
        return Some((4, ">"));
    }
    if matches!(prev, b'<' | b'>' | b'=' | b'-' | b'|' | b':' | b'\\' | b'/') {
        return None;
    }
    match (at(j), at(j + 1), at(j + 2)) {
        (b'<', b'=', b'>') => None,
        (b'<', b'=', _) => Some((2, "<")),
        (b'=', b'<', _) if !matches!(prev, b'=') => Some((2, "<")),
        (b'>', b'=', _) => Some((2, ">")),
        (b'<', c, _) if !matches!(c, b'<' | b'-' | b'>' | b'=') => Some((1, "<=")),
        (b'>', c, _) if !matches!(c, b'>' | b'=') => Some((1, ">=")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"---- MODULE M ----
EXTENDS Naturals
VARIABLES x, y, log
vars == <<x, y, log>>
Init == x = 0 /\ y = 0 /\ log = <<>>
Inc ==
    /\ x < 3            \* bound
    /\ y <= x
    /\ x' = x + 1
    /\ UNCHANGED <<y, log>>
Log(e) ==
    /\ e \notin {"a<b"}
    /\ log' = <<e>> \o log
    /\ UNCHANGED y
    /\ x' = x
Next == Inc \/ \E e \in {"p"} : Log(e)
Inv == x <= 3
===="#;

    #[test]
    fn mutates_only_actions_reachable_from_next() {
        let ms = mutants(SPEC, "Next");
        let names: Vec<&str> = ms.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"drop-guard:Inc#1") && names.contains(&"negate-guard:Inc#2"), "{names:?}");
        assert!(
            names.contains(&"unchanged:Inc-y") && names.contains(&"unchanged:Inc-log") && names.contains(&"unchanged:Log-y"),
            "{names:?}"
        );
        assert!(names.contains(&"swap-cmp:Inc#1") && names.contains(&"swap-cmp:Inc#2"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("Inv") || n.contains("Init")), "invariants and Init are never mutated: {names:?}");
        let texts: HashSet<&str> = ms.iter().map(|m| m.source.as_str()).collect();
        assert_eq!(texts.len(), ms.len(), "mutants are distinct");
        assert!(ms.iter().all(|m| m.source != SPEC && m.source.contains("---- MODULE M ----")));
    }

    #[test]
    fn a_capped_run_samples_guards_of_every_action_before_frames() {
        let ms = mutants(SPEC, "Next");
        let first_frame = ms.iter().position(|m| m.name.starts_with("unchanged:")).unwrap();
        let last_guard = ms.iter().rposition(|m| m.name.contains("-guard:")).unwrap();
        assert!(last_guard < first_frame, "{:?}", ms.iter().map(|m| &m.name).collect::<Vec<_>>());
        assert!(ms[..first_frame].iter().any(|m| m.name.contains(":Log#")), "the second action's guards come before any frame");
    }

    #[test]
    fn each_operator_rewrites_exactly_what_it_says() {
        let ms = mutants(SPEC, "Next");
        let get = |n: &str| ms.iter().find(|m| m.name == n).unwrap().source.clone();
        assert!(get("drop-guard:Inc#1").contains("    /\\ TRUE\n    /\\ y <= x"));
        assert!(get("negate-guard:Inc#1").contains("    /\\ ~(x < 3)            \\* bound"));
        assert!(get("negate-guard:Log#1").contains("/\\ ~(e \\notin {\"a<b\"})"), "strings are not scanned for operators");
        assert!(get("swap-cmp:Inc#1").contains("/\\ x <= 3 "));
        assert!(get("swap-cmp:Inc#2").contains("/\\ y < x"));
        assert!(get("unchanged:Inc-y").contains("/\\ UNCHANGED <<log>>"));
        assert!(get("unchanged:Log-y").contains("    /\\ TRUE\n    /\\ x' = x"));
        assert!(!ms.iter().any(|m| m.name.starts_with("swap-cmp:Log")), "`<<`, `<>`, `\\o` and strings are not comparisons");
    }

    #[test]
    fn comparison_scanner_skips_other_operators() {
        let hit = |s: &str| (0..s.len()).filter_map(|j| cmp_at(s.as_bytes(), j)).map(|x| x.1).collect::<Vec<_>>();
        assert_eq!(hit("a < b"), ["<="]);
        assert_eq!(hit("a <= b"), ["<"]);
        assert_eq!(hit("a >= b"), [">"]);
        assert_eq!(hit("<<a>> <=> b |-> c -> d :> e <- f"), Vec::<&str>::new());
        assert_eq!(hit("a \\leq b"), ["<"]);
    }
}
