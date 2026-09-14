//! TLC's console output → a closed result. The text is TLC's own (no `-tool` mode), so the
//! parser keys on the sentences TLC has printed for years:
//! `Model checking completed. No error has been found.`, `Error: Invariant X is violated.`,
//! `Error: Deadlock reached.`, `***Parse Error***`, and the `State N: <action>` trace blocks.

use aho_formal::tla;
use aho_types::Value;
use serde_json::{Map, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    Invariant(String),
    /// Action or temporal property (`PROPERTY`).
    Property(String),
    Deadlock,
}

impl Violation {
    pub fn describe(&self) -> String {
        match self {
            Violation::Invariant(n) => format!("invariant {n}"),
            Violation::Property(n) => format!("property {n}"),
            Violation::Deadlock => "deadlock (a reachable state has no successor)".into(),
        }
    }
}

/// One state of an error trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceState {
    pub step: u32,
    /// Action name (`Next`, `Charge`, `Initial predicate`, `Stuttering`, `Back to state 2`).
    pub action: String,
    /// Where TLC says the action is (`line 6, col 9 to line 6, col 29 of module Counter`).
    pub location: String,
    /// `(variable, value as TLC printed it)` in TLC's order.
    pub vars: Vec<(String, String)>,
}

impl TraceState {
    pub fn to_value(&self) -> Value {
        let mut vars = Map::new();
        let mut raw = Map::new();
        for (k, v) in &self.vars {
            vars.insert(k.clone(), tla::parse_value(v));
            raw.insert(k.clone(), Value::String(v.clone()));
        }
        json!({"step": self.step, "action": self.action, "location": self.location, "vars": vars, "tla": raw})
    }
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlcResult {
    Pass {
        depth: Option<u32>,
        distinct: Option<u64>,
    },
    Violation {
        what: Violation,
        trace: Vec<TraceState>,
    },
    /// SANY rejected the module or the config (syntax, unknown operator, bad cfg).
    ParseError(String),
    /// TLC could not evaluate the spec (type error, unassigned variable, assumption false).
    /// Not a verdict about the property.
    Error {
        message: String,
        trace: Vec<TraceState>,
    },
}

pub fn parse(out: &str, exit_code: i32) -> TlcResult {
    let lines: Vec<&str> = out.lines().collect();
    let trace = parse_trace(&lines);
    let depth = lines.iter().find_map(|l| {
        l.trim().strip_prefix("The depth of the complete state graph search is ").and_then(|r| r.trim_end_matches('.').trim().parse().ok())
    });
    let distinct = lines.iter().find_map(|l| {
        let (_, rest) = l.split_once(" states generated, ")?;
        rest.split_whitespace().next()?.parse().ok()
    });

    if let Some(msg) = parse_error(&lines, exit_code) {
        return TlcResult::ParseError(msg);
    }
    for l in &lines {
        let t = l.trim();
        if let Some(name) = t.strip_prefix("Error: Invariant ").and_then(|r| r.strip_suffix(" is violated.")) {
            return TlcResult::Violation { what: Violation::Invariant(name.trim().to_string()), trace };
        }
        if let Some(name) = t.strip_prefix("Error: Invariant ").and_then(|r| r.strip_suffix(" is violated by the initial state:")) {
            return TlcResult::Violation { what: Violation::Invariant(name.trim().to_string()), trace };
        }
        if let Some(name) = t.strip_prefix("Error: Action property ").and_then(|r| r.strip_suffix(" is violated.")) {
            return TlcResult::Violation { what: Violation::Property(name.trim().to_string()), trace };
        }
        if t == "Error: Temporal properties were violated." {
            return TlcResult::Violation { what: Violation::Property("temporal".into()), trace };
        }
        if t == "Error: Deadlock reached." {
            return TlcResult::Violation { what: Violation::Deadlock, trace };
        }
    }
    if lines.iter().any(|l| l.contains("Model checking completed. No error has been found.")) && exit_code == 0 {
        return TlcResult::Pass { depth, distinct };
    }
    let message = error_message(&lines).unwrap_or_else(|| {
        if exit_code == 0 {
            "TLC finished without its completion line; the output was cut short".into()
        } else {
            format!("TLC exited with code {exit_code} without a verdict")
        }
    });
    TlcResult::Error { message, trace }
}

/// SANY / config failures (exit 150 = spec parse, 151 = config parse). Collects the lines
/// that say what is wrong.
fn parse_error(lines: &[&str], exit_code: i32) -> Option<String> {
    let start = lines.iter().position(|l| l.contains("***Parse Error***") || l.trim_start().starts_with("*** Errors:"));
    let failed = lines.iter().any(|l| l.contains("Error: Parsing or semantic analysis failed."));
    match start {
        Some(s) => {
            let msg: Vec<&str> = lines[s..]
                .iter()
                .map(|l| l.trim())
                .take_while(|l| !l.starts_with("Error: Parsing or semantic analysis failed.") && !l.starts_with("Starting..."))
                .filter(|l| !l.is_empty() && !l.starts_with("Residual stack trace"))
                .take(12)
                .collect();
            Some(msg.join("\n"))
        }
        None if failed => Some("Parsing or semantic analysis failed.".into()),
        None if exit_code == 150 || exit_code == 151 => error_message(lines),
        None => None,
    }
}

/// The first `Error:` paragraph that is not the trace preamble.
fn error_message(lines: &[&str]) -> Option<String> {
    let i = lines.iter().position(|l| {
        let t = l.trim();
        t.starts_with("Error:")
            && !t.starts_with("Error: The behavior up to this point is:")
            && !t.starts_with("Error: The following behavior")
    })?;
    let mut msg = vec![lines[i].trim().trim_start_matches("Error:").trim()];
    for l in &lines[i + 1..] {
        let t = l.trim();
        if t.is_empty() || t.starts_with("Error: The behavior") || t.starts_with("State ") || t.starts_with("Error: The following behavior")
        {
            break;
        }
        msg.push(t.trim_start_matches("Error:").trim());
        if msg.len() >= 8 {
            break;
        }
    }
    Some(msg.join(" "))
}

fn parse_trace(lines: &[&str]) -> Vec<TraceState> {
    let mut out: Vec<TraceState> = Vec::new();
    let mut cur: Option<TraceState> = None;
    for l in lines {
        if let Some(s) = state_header(l) {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some(s);
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let t = l.trim_end();
        if t.trim().is_empty() {
            if let Some(done) = cur.take() {
                out.push(done);
            }
            continue;
        }
        let body = t.trim_start();
        if let Some(rest) = body.strip_prefix("/\\ ")
            && let Some((k, v)) = rest.split_once(" = ")
        {
            c.vars.push((k.trim().to_string(), v.trim().to_string()));
            continue;
        }
        if (c.vars.is_empty() || !t.starts_with(' '))
            && let Some((k, v)) = body.split_once(" = ")
            && tla::is_ident(k.trim())
            && c.vars.is_empty()
        {
            c.vars.push((k.trim().to_string(), v.trim().to_string()));
            continue;
        }
        if let Some(last) = c.vars.last_mut() {
            last.1.push(' ');
            last.1.push_str(body);
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

/// `State 3: <Next line 6, col 9 to line 6, col 29 of module Counter>`, `4: Stuttering`,
/// `5: Back to state 2`.
fn state_header(l: &str) -> Option<TraceState> {
    let t = l.trim();
    let rest = t.strip_prefix("State ").unwrap_or(t);
    let (n, tail) = rest.split_once(':')?;
    let step: u32 = n.trim().parse().ok()?;
    let tail = tail.trim();
    if !(t.starts_with("State ") || tail == "Stuttering" || tail.starts_with("Back to state")) {
        return None;
    }
    let inner = tail.trim_start_matches('<').trim_end_matches('>').trim();
    let (action, location) = match inner.find(" line ") {
        Some(i) => (inner[..i].trim().to_string(), inner[i + 1..].trim().to_string()),
        None => (inner.to_string(), String::new()),
    };
    Some(TraceState { step, action, location, vars: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_variable_states_with_wrapped_values() {
        let out = "Error: Invariant Inv is violated.\nError: The behavior up to this point is:\nState 1: <Initial predicate>\n/\\ log = <<>>\n/\\ open = {}\n\nState 2: <Call(\"c1\") line 12, col 5 to line 14, col 30 of module Log>\n/\\ log = << [kind |-> \"call\",\n      call |-> \"c1\"] >>\n/\\ open = {\"c1\"}\n\n";
        let TlcResult::Violation { what, trace } = parse(out, 12) else { panic!() };
        assert_eq!(what, Violation::Invariant("Inv".into()));
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[1].action, "Call(\"c1\")");
        assert_eq!(trace[1].location, "line 12, col 5 to line 14, col 30 of module Log");
        assert_eq!(trace[1].var("log"), Some("<< [kind |-> \"call\", call |-> \"c1\"] >>"));
        assert_eq!(trace[1].to_value()["vars"]["log"], json!([{"kind": "call", "call": "c1"}]));
        assert_eq!(trace[1].to_value()["vars"]["open"], json!(["c1"]));
    }

    #[test]
    fn lasso_markers_are_states() {
        let out = "Error: Temporal properties were violated.\n\nError: The following behavior constitutes a counter-example:\n\nState 1: <Initial predicate>\nx = 0\n\n2: Stuttering\n";
        let TlcResult::Violation { what, trace } = parse(out, 13) else { panic!() };
        assert_eq!(what, Violation::Property("temporal".into()));
        assert_eq!(trace.iter().map(|s| s.action.as_str()).collect::<Vec<_>>(), ["Initial predicate", "Stuttering"]);
        assert_eq!(trace[0].vars, [("x".to_string(), "0".to_string())]);
    }

    #[test]
    fn evaluation_errors_are_not_verdicts() {
        let out = "Error: Successor state is not completely specified by action Result of the next-state relation. The following variable is not assigned: open.\n\nError: The behavior up to this point is:\nState 1: <Initial predicate>\nopen = {}\n\n";
        let TlcResult::Error { message, trace } = parse(out, 75) else { panic!() };
        assert!(message.starts_with("Successor state is not completely specified"), "{message}");
        assert_eq!(trace.len(), 1);
        assert!(matches!(parse("", 1), TlcResult::Error { .. }));
        assert!(matches!(parse("Model checking completed. No error has been found.\n", 0), TlcResult::Pass { .. }));
    }
}
