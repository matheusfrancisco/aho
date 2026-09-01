//! Tiny glob for content rules (`bash(git *)`, `write(src/**)`, `mcp__github__*`).
//!
//! Two modes:
//! * command mode — `*` matches any run of characters (spaces and `/` included), `?` one character;
//! * path mode — `*` and `?` stop at `/`, `**` crosses directories, `**/` also matches nothing.
//!
//! `\x` matches `x` literally. Matching is a row-by-row DP, linear in pattern × text.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Lit(char),
    One,
    Star,
    DoubleStar,
    DoubleStarSlash,
}

fn compile(pattern: &str, path_mode: bool) -> Vec<Tok> {
    let cs: Vec<char> = pattern.chars().collect();
    let mut out = Vec::with_capacity(cs.len());
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' if i + 1 < cs.len() => {
                out.push(Tok::Lit(cs[i + 1]));
                i += 2;
            }
            '*' if path_mode && cs.get(i + 1) == Some(&'*') => {
                if cs.get(i + 2) == Some(&'/') {
                    out.push(Tok::DoubleStarSlash);
                    i += 3;
                } else {
                    out.push(Tok::DoubleStar);
                    i += 2;
                    while cs.get(i) == Some(&'*') {
                        i += 1;
                    }
                }
            }
            '*' => {
                if out.last() != Some(&Tok::Star) {
                    out.push(Tok::Star);
                }
                i += 1;
            }
            '?' => {
                out.push(Tok::One);
                i += 1;
            }
            c => {
                out.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    out
}

/// True when `text` matches `pattern` in full.
pub(crate) fn matches(pattern: &str, text: &str, path_mode: bool) -> bool {
    let toks = compile(pattern, path_mode);
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    // row[j]: tokens so far match t[..j]
    let mut row = vec![false; n + 1];
    row[0] = true;
    for tok in &toks {
        let mut next = vec![false; n + 1];
        match *tok {
            Tok::Lit(c) => {
                for j in 0..n {
                    if row[j] && t[j] == c {
                        next[j + 1] = true;
                    }
                }
            }
            Tok::One => {
                for j in 0..n {
                    if row[j] && !(path_mode && t[j] == '/') {
                        next[j + 1] = true;
                    }
                }
            }
            Tok::Star => {
                for j in 0..=n {
                    next[j] = row[j] || (j > 0 && next[j - 1] && !(path_mode && t[j - 1] == '/'));
                }
            }
            Tok::DoubleStar => {
                for j in 0..=n {
                    next[j] = row[j] || (j > 0 && next[j - 1]);
                }
            }
            Tok::DoubleStarSlash => {
                // Consumes nothing, or any run that ends in `/`.
                let mut seen = false;
                for j in 0..=n {
                    next[j] = row[j] || (j > 0 && seen && t[j - 1] == '/');
                    seen |= row[j];
                }
            }
        }
        row = next;
    }
    row[n]
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn command_mode_star_crosses_spaces_and_slashes() {
        assert!(matches("rm *", "rm -rf /tmp/x", false));
        assert!(matches("git *", "git push origin main", false));
        assert!(!matches("git *", "gitk", false));
        assert!(matches("*", "", false));
        assert!(!matches("rm", "rm -rf x", false));
    }

    #[test]
    fn path_mode_star_stops_at_separator() {
        assert!(matches("src/*", "src/a.rs", true));
        assert!(!matches("src/*", "src/a/b.rs", true));
        assert!(matches("src/**", "src/a/b.rs", true));
        assert!(matches("src/**/*.rs", "src/a.rs", true));
        assert!(matches("src/**/*.rs", "src/x/y/a.rs", true));
        assert!(!matches("src/**/*.rs", "lib/a.rs", true));
        assert!(matches("spec/**", "spec/SPEC.md", true));
        assert!(!matches("spec/**", "specs/SPEC.md", true));
    }

    #[test]
    fn escapes_and_question_mark() {
        assert!(matches(r"a\*b", "a*b", false));
        assert!(!matches(r"a\*b", "axb", false));
        assert!(matches("a?c", "abc", true));
        assert!(!matches("a?c", "a/c", true));
    }
}
