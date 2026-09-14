//! `lake build` / `lean` console output → goals and diagnostics.
//!
//! Lake prefixes messages as `error: ./Aho/Fold.lean:12:2: unsolved goals`; plain `lean`
//! prints `Aho/Fold.lean:12:2: error: unsolved goals`. Both are read. An `unsolved goals`
//! message is followed by one block per goal — optional `case name`, hypotheses
//! (`x y : Nat`, continuation lines indented), then `⊢ target` — separated by blank lines.

use aho_formal::{Diagnostic, Goal};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeanReport {
    pub diagnostics: Vec<Diagnostic>,
    /// `(diagnostic index, goal)`: which message each goal came from.
    pub goals: Vec<(usize, Goal)>,
    /// `declaration uses 'sorry'`: builds, proves nothing.
    pub sorries: Vec<usize>,
}

impl LeanReport {
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == "error")
    }
}

/// `(file, line, col, severity, message head)` from one message header line.
fn header(l: &str) -> Option<(String, u32, u32, String, String)> {
    let t = l.trim_end();
    // lake: `error: path:L:C: msg`
    for sev in ["error", "warning", "info"] {
        if let Some(rest) = t.strip_prefix(&format!("{sev}: "))
            && let Some((file, line, col, msg)) = located(rest)
        {
            return Some((file, line, col, sev.to_string(), msg));
        }
    }
    // lean: `path:L:C: error: msg`
    let (file, line, col, rest) = located(t)?;
    for sev in ["error", "warning", "info"] {
        if let Some(msg) = rest.strip_prefix(&format!("{sev}:")) {
            return Some((file, line, col, sev.to_string(), msg.trim().to_string()));
        }
    }
    None
}

/// `path.lean:L:C: rest`
fn located(s: &str) -> Option<(String, u32, u32, String)> {
    let idx = s.find(".lean:")?;
    let file = s[..idx + 5].trim();
    let mut parts = s[idx + 6..].splitn(3, ':');
    let line = parts.next()?.trim().parse().ok()?;
    let col = parts.next()?.trim().parse().ok()?;
    let rest = parts.next()?.trim().to_string();
    Some((normalize(file), line, col, rest))
}

fn normalize(f: &str) -> String {
    let mut s = f;
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    s.to_string()
}

pub fn parse(out: &str) -> LeanReport {
    let lines: Vec<&str> = out.lines().collect();
    let mut r = LeanReport::default();
    let mut i = 0;
    while i < lines.len() {
        let Some((file, line, col, severity, head)) = header(lines[i]) else {
            i += 1;
            continue;
        };
        // the message body runs to the next header or a lake status line
        let mut j = i + 1;
        while j < lines.len() && header(lines[j]).is_none() && !is_lake_status(lines[j]) {
            j += 1;
        }
        let body: Vec<&str> = lines[i + 1..j].to_vec();
        let idx = r.diagnostics.len();
        if head.starts_with("unsolved goals") {
            for g in goals(&body) {
                r.goals.push((idx, g));
            }
        }
        if head.contains("declaration uses 'sorry'") || head.contains("declaration uses `sorry`") {
            r.sorries.push(idx);
        }
        let mut message = head.clone();
        let extra: Vec<&str> = body.iter().map(|l| l.trim_end()).filter(|l| !l.trim().is_empty()).collect();
        if !extra.is_empty() {
            message.push('\n');
            message.push_str(&extra.join("\n"));
        }
        r.diagnostics.push(Diagnostic { file, line, col, severity, message });
        i = j;
    }
    r
}

fn is_lake_status(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with('✖')
        || t.starts_with('✔')
        || t.starts_with('⚠')
        || t.starts_with("info: ")
        || t.starts_with("trace: ")
        || t.starts_with("Some required builds logged failures")
        || t.starts_with("error: build failed")
        || t.starts_with("error: Lean exited with code")
        || t.starts_with("Build completed")
        || (t.starts_with('[') && t.contains("] Building"))
}

fn goals(body: &[&str]) -> Vec<Goal> {
    let mut out = Vec::new();
    let mut case: Option<String> = None;
    let mut hyps: Vec<String> = Vec::new();
    let mut target: Option<String> = None;
    let flush = |case: &mut Option<String>, hyps: &mut Vec<String>, target: &mut Option<String>, out: &mut Vec<Goal>| {
        if let Some(t) = target.take() {
            out.push(Goal { name: case.take().unwrap_or_default(), hypotheses: std::mem::take(hyps), target: t });
        }
    };
    for l in body {
        let t = l.trim_end();
        if t.trim().is_empty() {
            flush(&mut case, &mut hyps, &mut target, &mut out);
            continue;
        }
        let body = t.trim_start();
        let indented = t.len() != body.len();
        if let Some(rest) = body.strip_prefix('⊢') {
            target = Some(rest.trim().to_string());
        } else if let Some(name) = body.strip_prefix("case ").filter(|_| !indented && target.is_none() && hyps.is_empty()) {
            case = Some(name.trim().to_string());
        } else if indented {
            match (&mut target, hyps.last_mut()) {
                (Some(t), _) => {
                    t.push(' ');
                    t.push_str(body);
                }
                (None, Some(h)) => {
                    h.push(' ');
                    h.push_str(body);
                }
                (None, None) => hyps.push(body.to_string()),
            }
        } else {
            if target.is_some() {
                // a new goal without a blank separator
                flush(&mut case, &mut hyps, &mut target, &mut out);
            }
            hyps.push(body.to_string());
        }
    }
    flush(&mut case, &mut hyps, &mut target, &mut out);
    out
}

/// Name of the declaration enclosing `line` (1-based) in Lean source: the nearest
/// `theorem|lemma|def|example|instance|abbrev` header at or above it.
pub fn enclosing_decl(src: &str, line: u32) -> Option<String> {
    let lines: Vec<&str> = src.lines().collect();
    let upto = (line as usize).min(lines.len());
    for l in lines[..upto].iter().rev() {
        let mut words = l.split_whitespace().skip_while(|w| {
            matches!(*w, "private" | "protected" | "noncomputable" | "partial" | "unsafe" | "nonrec") || w.starts_with("@[")
        });
        match words.next() {
            Some(kw @ ("theorem" | "lemma" | "def" | "abbrev" | "instance")) => {
                return Some(words.next().map(|n| n.trim_end_matches(':').to_string()).unwrap_or_else(|| kw.to_string()));
            }
            Some("example") => return Some("example".into()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_header_styles_are_read() {
        assert_eq!(
            header("error: ./././Aho/Fold.lean:12:2: unsolved goals"),
            Some(("Aho/Fold.lean".into(), 12, 2, "error".into(), "unsolved goals".into()))
        );
        assert_eq!(
            header("Aho/Fold.lean:3:8: warning: declaration uses 'sorry'"),
            Some(("Aho/Fold.lean".into(), 3, 8, "warning".into(), "declaration uses 'sorry'".into()))
        );
        assert_eq!(header("✖ [2/3] Building Aho.Fold"), None);
    }

    #[test]
    fn goals_split_on_blank_lines_and_keep_cases() {
        let body = [
            "case succ",
            "k : Nat",
            "ih : 2 * gauss k = k * k",
            "⊢ k * k + 2 * (k + 1) =",
            "    (k + 1) * (k + 1)",
            "",
            "x y : Nat",
            "h : x < y",
            "⊢ x + 1 ≤ y",
        ];
        let gs = goals(&body);
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0].name, "succ");
        assert_eq!(gs[0].hypotheses, ["k : Nat", "ih : 2 * gauss k = k * k"]);
        assert_eq!(gs[0].target, "k * k + 2 * (k + 1) = (k + 1) * (k + 1)");
        assert_eq!(gs[1].name, "");
        assert_eq!(gs[1].target, "x + 1 ≤ y");
    }

    #[test]
    fn declarations_are_found_above_the_error() {
        let src = "import Mathlib\n\n@[simp] theorem fold_nil : fold [] = 0 := by\n  simp\n\nprivate def helper (x : Nat) : Nat :=\n  x\n";
        assert_eq!(enclosing_decl(src, 4).as_deref(), Some("fold_nil"));
        assert_eq!(enclosing_decl(src, 7).as_deref(), Some("helper"));
        assert_eq!(enclosing_decl(src, 1), None);
    }
}
