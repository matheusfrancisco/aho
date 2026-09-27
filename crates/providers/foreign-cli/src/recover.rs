//! Crash-honest receipts: what the parent log says about each child this provider started,
//! folded back into state when a session boots over an existing log.
//!
//! * `Receipt` without `Attached`: the child was *launching* when aho stopped. Whether the CLI
//!   ever came up is unknown, so it becomes `blocked + uncertain`; it is never resumed by guess.
//! * `Attached`: the transcript path, file identity, and the last mirrored offset are known, so
//!   the tail continues exactly where it stopped.
//! * `Sent` without `Delivered`: delivery is `unknown` (the transcript may still confirm it).

use std::path::PathBuf;

use aho_log::{Entry, EntryKind};
use aho_types::{SessionId, StopReason, Value};

use crate::mirror::{self, NS};
use crate::tail::FileIdent;

/// One child, as the parent log remembers it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recovered {
    pub child: SessionId,
    pub label: String,
    pub depth: u32,
    pub cwd: PathBuf,
    pub tmux_session: String,
    pub cli_session: Option<String>,
    /// Receipt write time (ms since the epoch).
    pub launched_ms: u64,
    /// tmux `session_created` + pane pid, from `Launched`: the session's identity.
    pub created: Option<u64>,
    pub pane_pid: Option<u32>,
    pub transcript: Option<PathBuf>,
    pub ident: FileIdent,
    /// Offset of the last transcript record mirrored into the parent.
    pub offset: u64,
    pub closed: bool,
    /// Runs started (the receipt's prompt is run 1).
    pub runs: u64,
    /// The last run has no `Result` yet.
    pub run_open: bool,
    /// Stop and last message of the last finished run.
    pub last_result: Option<(StopReason, Option<String>)>,
    pub unconfirmed: Vec<u64>,
    pub next_seq: u64,
    pub last_text: Option<String>,
}

fn str_of(b: &Value, k: &str) -> Option<String> {
    b.get(k).and_then(Value::as_str).map(str::to_string)
}

fn u64_of(b: &Value, k: &str) -> Option<u64> {
    b.get(k).and_then(Value::as_u64)
}

fn ident_of(b: &Value) -> FileIdent {
    let a = b.get("ident")?.as_array()?;
    Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))
}

/// Folds this provider's `Subagent/*` entries (in log order) into per-child state, oldest first.
pub fn scan(entries: &[Entry], provider: &str) -> Vec<Recovered> {
    let mut out: Vec<Recovered> = Vec::new();
    for e in entries {
        let EntryKind::Custom { ns, name, body } = &e.kind else { continue };
        if ns != NS || body.get("provider").and_then(Value::as_str) != Some(provider) {
            continue;
        }
        let Some(child) = str_of(body, "child").and_then(|c| c.parse::<u128>().ok()).map(SessionId) else { continue };
        if name == mirror::RECEIPT {
            out.retain(|r| r.child != child);
            out.push(Recovered {
                child,
                label: str_of(body, "label").unwrap_or_default(),
                depth: u64_of(body, "depth").and_then(|d| u32::try_from(d).ok()).unwrap_or(1),
                cwd: PathBuf::from(str_of(body, "cwd").unwrap_or_default()),
                tmux_session: str_of(body, "tmux_session").unwrap_or_default(),
                cli_session: str_of(body, "cli_session"),
                launched_ms: e.at.0,
                runs: 1,
                run_open: true,
                unconfirmed: vec![0],
                next_seq: 1,
                ..Recovered::default()
            });
            continue;
        }
        let Some(r) = out.iter_mut().find(|r| r.child == child) else { continue };
        if let Some(o) = u64_of(body, "offset") {
            r.offset = o;
        }
        match name.as_str() {
            mirror::LAUNCHED => {
                r.created = u64_of(body, "created");
                r.pane_pid = u64_of(body, "pane_pid").and_then(|p| u32::try_from(p).ok());
            }
            mirror::ATTACHED => {
                r.transcript = str_of(body, "transcript").map(PathBuf::from);
                r.ident = ident_of(body);
                if let Some(s) = str_of(body, "cli_session") {
                    r.cli_session = Some(s);
                }
            }
            mirror::NOTE => {
                if body.get("ident").is_some() {
                    r.ident = ident_of(body);
                }
            }
            mirror::SENT => {
                if let Some(seq) = u64_of(body, "seq") {
                    r.unconfirmed.push(seq);
                    r.next_seq = r.next_seq.max(seq + 1);
                }
                if body.get("starts_run").and_then(Value::as_bool) == Some(true) {
                    r.runs += 1;
                    r.run_open = true;
                    r.last_text = None;
                }
            }
            mirror::DELIVERED | mirror::DELIVERY => {
                if let Some(seq) = u64_of(body, "seq") {
                    r.unconfirmed.retain(|s| *s != seq);
                }
            }
            mirror::MESSAGE => r.last_text = str_of(body, "text"),
            mirror::RESULT => {
                r.run_open = false;
                let stop =
                    body.get("stop").cloned().and_then(|s| serde_json::from_value::<StopReason>(s).ok()).unwrap_or(StopReason::Error);
                r.last_result = Some((stop, str_of(body, "last_message")));
            }
            mirror::CLOSED => r.closed = true,
            _ => {}
        }
    }
    out
}
