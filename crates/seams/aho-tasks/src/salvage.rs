//! Salvage folds the dead worker's log without forking it: its last tool-call summaries and its
//! latest `Task/Checkpoint`.

use std::collections::BTreeMap;

use aho_log::{Entry, EntryKind, ToolOutcome};
use aho_types::{AgentId, CallId, ContentBlock, Principal, Value};

use crate::NS;
use crate::model::{Salvage, parse_agent};

const ARGS_CHARS: usize = 80;
const RESULT_CHARS: usize = 120;

fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max).collect();
    format!("{cut}…")
}

fn first_text(content: &[ContentBlock]) -> String {
    content.iter().find_map(|b| b.as_text()).unwrap_or("").to_string()
}

impl Salvage {
    /// Folds `entries` (the dead worker's branch, oldest first). With `agent`, only that agent's
    /// tool calls and checkpoints count (one shared log); with `None`, every agent's (a child's own log).
    /// Keeps the last `keep` tool summaries.
    pub fn from_log(entries: &[Entry], agent: Option<AgentId>, keep: usize) -> Salvage {
        let mine = |p: &Principal| match (p, agent) {
            (Principal::Agent { id, .. }, Some(a)) => *id == a,
            (Principal::Agent { .. }, None) => true,
            _ => false,
        };
        let mut calls: BTreeMap<&CallId, (String, String)> = BTreeMap::new();
        let mut summaries = Vec::new();
        let mut checkpoint = None;
        let mut from = agent;
        for e in entries {
            match &e.kind {
                EntryKind::ToolCall { call, tool, args, .. } if mine(&e.by) => {
                    if let Principal::Agent { id, .. } = &e.by {
                        from = Some(*id);
                    }
                    calls.insert(call, (tool.clone(), clip(&args.to_string(), ARGS_CHARS)));
                }
                EntryKind::ToolResult { call, outcome, content, .. } => {
                    if let Some((tool, args)) = calls.remove(call) {
                        let result = match outcome {
                            ToolOutcome::Ok => format!("ok: {}", clip(&first_text(content), RESULT_CHARS)),
                            ToolOutcome::Err { code, reason } => format!("error {code}: {}", clip(reason, RESULT_CHARS)),
                            ToolOutcome::Cancelled { .. } => "cancelled".to_string(),
                        };
                        summaries.push(format!("{tool}({args}) → {result}"));
                    }
                }
                EntryKind::Custom { ns, name, body } if ns == NS && name == "Checkpoint" => {
                    let by = body.get("by").and_then(Value::as_str).and_then(parse_agent);
                    let counts = match agent {
                        Some(a) => by == Some(a),
                        None => true,
                    };
                    if counts {
                        checkpoint = body.get("summary").and_then(Value::as_str).map(str::to_string);
                        from = from.or(by);
                    }
                }
                _ => {}
            }
        }
        // calls that never got a result are still worth knowing about
        for (tool, args) in calls.into_values() {
            summaries.push(format!("{tool}({args}) → no result (the worker died mid-call)"));
        }
        let drop = summaries.len().saturating_sub(keep);
        summaries.drain(..drop);
        Salvage { from, checkpoint, summaries }
    }
}
