//! Parent-log entries: a subagent's entries are mirrored into the parent under `Subagent/*`.
//! All are `EntryKind::Custom { ns: "Subagent", name, body }` appended by
//! `Principal::System(<provider>)`; every body carries `child` (decimal string) and `provider`.
//! Entries derived from the transcript also carry `offset`, the byte right after the record they
//! came from, which is where a resumed tail continues (crash-honest receipts).
//!
//! | name | when | body (besides child, provider) |
//! |---|---|---|
//! | `Receipt` | before the CLI is launched | `label, depth, cwd, tmux_socket, tmux_session, cli_session, state: "launching"` |
//! | `Launched` | tmux started it | `tmux_session, created, pane_pid, attach` |
//! | `Attached` | its transcript appeared: the launch is confirmed | `transcript, ident, cli_session, offset: 0` |
//! | `Sent` | aho typed text into its pane | `seq, starts_run, chars` |
//! | `Delivered` | the transcript shows the prompt | `seq, offset` |
//! | `Delivery` | after a crash, a send that was never confirmed | `seq, delivery: "unknown"` |
//! | `Message` | assistant text | `text, truncated, offset` |
//! | `ToolUse` / `ToolResult` | its own tool calls | `id, name, input` / `id, is_error, content, truncated` |
//! | `TurnEnd` | it finished or abandoned a turn | `stop, last_message, offset` |
//! | `Error` | it reported an error | `message, retrying, offset` |
//! | `Blocked` | it wrote nothing and its screen froze: waiting on its own dialog | `reason, screen` |
//! | `Result` | what `wait` returned for a run | `run, stop, last_message, diagnostic` |
//! | `Recovered` | the next session found it after a crash | `state: "resumed" \| "blocked", certainty, reason, offset` |
//! | `Note` | the tail restarted, an interrupt was sent, a prompt aho did not send appeared | `reason, offset?` |
//! | `Closed` | it is gone | `reason, exit_status?` |

use std::sync::Arc;

use aho_log::{EntryKind, LogStore};
use aho_types::{Principal, SessionId, Value};

pub const NS: &str = "Subagent";

pub const RECEIPT: &str = "Receipt";
pub const LAUNCHED: &str = "Launched";
pub const ATTACHED: &str = "Attached";
pub const SENT: &str = "Sent";
pub const DELIVERED: &str = "Delivered";
pub const DELIVERY: &str = "Delivery";
pub const MESSAGE: &str = "Message";
pub const TOOL_USE: &str = "ToolUse";
pub const TOOL_RESULT: &str = "ToolResult";
pub const TURN_END: &str = "TurnEnd";
pub const ERROR: &str = "Error";
pub const BLOCKED: &str = "Blocked";
pub const RESULT: &str = "Result";
pub const RECOVERED: &str = "Recovered";
pub const NOTE: &str = "Note";
pub const CLOSED: &str = "Closed";

/// Every entry kind this crate writes, as `Manifest::entry_kinds` spells them.
pub const ENTRY_KINDS: &[&str] = &[
    "Subagent/Receipt",
    "Subagent/Launched",
    "Subagent/Attached",
    "Subagent/Sent",
    "Subagent/Delivered",
    "Subagent/Delivery",
    "Subagent/Message",
    "Subagent/ToolUse",
    "Subagent/ToolResult",
    "Subagent/TurnEnd",
    "Subagent/Error",
    "Subagent/Blocked",
    "Subagent/Result",
    "Subagent/Recovered",
    "Subagent/Note",
    "Subagent/Closed",
];

/// Assistant text kept per `Message` entry (bytes).
pub const TEXT_MAX: usize = 16 * 1024;
/// Tool input/output kept per entry (bytes).
pub const TOOL_MAX: usize = 8 * 1024;
/// `Result_::diagnostic` bound (bytes), as the seam requires.
pub const DIAGNOSTIC_MAX: usize = 4096;

/// Writes `Subagent/*` entries into the parent log.
#[derive(Clone)]
pub struct Mirror {
    log: Arc<dyn LogStore>,
    provider: &'static str,
}

impl Mirror {
    pub fn new(log: Arc<dyn LogStore>, provider: &'static str) -> Mirror {
        Mirror { log, provider }
    }

    pub fn log(&self) -> &Arc<dyn LogStore> {
        &self.log
    }

    pub fn append(&self, name: &str, child: SessionId, mut body: Value) -> Result<(), String> {
        if !body.is_object() {
            body = Value::Object(Default::default());
        }
        if let Value::Object(m) = &mut body {
            m.insert("child".into(), Value::String(child.0.to_string()));
            m.insert("provider".into(), Value::String(self.provider.to_string()));
        }
        self.log
            .append(None, Principal::system(self.provider), EntryKind::Custom { ns: NS.into(), name: name.into(), body })
            .map(|_| ())
            .map_err(|e| format!("could not write Subagent/{name} to the session log ({e})"))
    }
}
