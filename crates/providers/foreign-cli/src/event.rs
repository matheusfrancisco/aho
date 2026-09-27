//! A foreign CLI's transcript in aho's vocabulary. Each CLI crate owns its own normalizer (a
//! forked adapter is a copy, never a shared abstraction); this is only the output type they
//! share with the engine.

use aho_types::{StopReason, Value};

/// One normalized transcript event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The CLI's own session identity, as its transcript states it.
    Session {
        id: String,
        cwd: Option<String>,
        version: Option<String>,
    },
    /// The CLI started working on a prompt.
    TurnStarted,
    /// The CLI accepted a user prompt (typed, pasted, or queued while it worked). This is the
    /// delivery confirmation for text aho typed into its pane.
    UserPrompt,
    /// Visible assistant text.
    AssistantText {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        is_error: bool,
        content: String,
    },
    /// The CLI finished (or abandoned) the turn.
    TurnEnd {
        stop: StopReason,
        last_message: Option<String>,
    },
    /// The CLI reported an error. `retrying` = it said it will try again by itself.
    Error {
        message: String,
        retrying: bool,
    },
}

/// The outcome for one transcript record.
#[derive(Clone, Debug, PartialEq)]
pub enum Normalized {
    /// Mapped into these events (possibly none, e.g. an assistant record holding only a
    /// redacted thinking block).
    Events(Vec<Event>),
    /// Deliberately not mirrored; the reason says why.
    Ignored(&'static str),
    /// A record kind no recorded session of the real binary has shown. Surfaced, never guessed.
    Unknown,
    /// Not a JSON object.
    Malformed(String),
}

/// One record: its kind name (as the CLI spells it, e.g. `system/turn_duration`) and outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub kind: String,
    pub out: Normalized,
}

impl Record {
    pub fn events(&self) -> &[Event] {
        match &self.out {
            Normalized::Events(e) => e,
            _ => &[],
        }
    }
}

/// At most `max` bytes of `s`, cut on a character boundary, with a marker when cut.
pub fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &s[..end]), true)
}

/// Joins the text of a JSON value that is either a string or a list of `{type:"text", text}`
/// blocks (the two shapes tool results take in both CLIs' transcripts).
pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| match b {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clip_cuts_on_a_char_boundary_and_marks_the_cut() {
        assert_eq!(clip("abc", 3), ("abc".into(), false));
        let (s, cut) = clip("aé", 2);
        assert!(cut);
        assert_eq!(s, "a…");
    }

    #[test]
    fn text_of_reads_both_result_shapes() {
        assert_eq!(text_of(&json!("x")), "x");
        assert_eq!(
            text_of(&json!([{"type": "text", "text": "a"}, {"type": "tool_reference", "tool_name": "b"}, {"type": "text", "text": "c"}])),
            "a\nc"
        );
        assert_eq!(text_of(&json!(null)), "");
    }
}
