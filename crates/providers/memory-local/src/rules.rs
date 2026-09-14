//! The model-free extractor: lines the operator marks with `remember:` or `note:`.
//! (Facts the agent wants kept arrive through the `memory_save` tool, not here.)

use std::collections::BTreeSet;

use aho_memory::{Draft, Entry, Extractor, MemoryError, Trigger};
use aho_types::{BoxFuture, Message, Role};

/// Operator-stated facts outrank model-extracted ones.
pub const EXPLICIT_CONFIDENCE: f32 = 0.8;

pub struct RuleExtractor;

/// `remember: …` / `note: …` lines (case-insensitive, bullets and quote markers allowed) from
/// user messages, deduped; lines matching an existing entry verbatim are skipped.
pub fn explicit_notes(window: &[Message], existing: &[Entry]) -> Vec<Draft> {
    let known: BTreeSet<String> = existing.iter().map(|e| e.text.trim().to_lowercase()).collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for m in window.iter().filter(|m| m.role == Role::User) {
        for line in m.text().lines() {
            let line = line.trim_start_matches(|c: char| c.is_whitespace() || "-*•>".contains(c));
            let lower = line.to_lowercase();
            let (tag, rest) = if lower.starts_with("remember:") {
                ("remembered", &line["remember:".len()..])
            } else if lower.starts_with("note:") {
                ("note", &line["note:".len()..])
            } else {
                continue;
            };
            let text = rest.trim();
            let key = text.to_lowercase();
            if text.is_empty() || known.contains(&key) || !seen.insert(key) {
                continue;
            }
            out.push(Draft {
                text: text.to_string(),
                tags: vec![tag.to_string()],
                confidence: EXPLICIT_CONFIDENCE,
                supersedes: Vec::new(),
            });
        }
    }
    out
}

impl Extractor for RuleExtractor {
    fn extract<'a>(
        &'a self,
        _trigger: Trigger,
        window: &'a [Message],
        existing: &'a [Entry],
    ) -> BoxFuture<'a, Result<Vec<Draft>, MemoryError>> {
        let out = explicit_notes(window, existing);
        Box::pin(async move { Ok(out) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_types::ContentBlock;

    #[test]
    fn picks_marked_user_lines_only() {
        let window = vec![
            Message::user(
                "Remember: the staging DB is on port 5433\nsomething else\n  - note: prefer `just test` over cargo test\nREMEMBER:   ",
            ),
            Message::assistant(vec![ContentBlock::text("remember: assistants cannot plant memories this way")]),
            Message::user("remember: the staging DB is on port 5433"),
        ];
        let d = explicit_notes(&window, &[]);
        let texts: Vec<(&str, &str)> = d.iter().map(|x| (x.text.as_str(), x.tags[0].as_str())).collect();
        assert_eq!(texts, vec![("the staging DB is on port 5433", "remembered"), ("prefer `just test` over cargo test", "note")]);
        assert!(d.iter().all(|x| x.confidence == EXPLICIT_CONFIDENCE));
        let known = vec![Entry::new("The staging DB is on port 5433", vec![])];
        assert_eq!(explicit_notes(&window, &known).len(), 1, "already stored verbatim");
    }
}
