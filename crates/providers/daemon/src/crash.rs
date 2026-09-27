//! Session resume: crash detection by `(pid, start_time)` in `SessionMeta.process`. Pids
//! recycle, the pair does not. Liveness flaps are ignored: an identity must be missing on two
//! consecutive scans before a session is marked `Daemon/Crashed`. Recovery is `fork(at)` at the
//! last completed turn, plus a notice naming the tools that may or may not have run.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::process::{Command, Stdio};

use aho_log::{CustomFolds, Entry, EntryId, EntryKind, LogStore, ProcessIdentity, State, fold, step};
use aho_types::*;
use serde_json::json;

use crate::ready::READY_FD_ENV;

/// Namespace and name of the crash marker entry.
pub const NS: &str = "Daemon";
pub const CRASHED: &str = "Crashed";
/// Principal name of the notice [`resume_crashed`] appends.
pub const RECOVERY: &str = "recovery";

/// `etime` has one-second resolution and `SessionMeta.start_time` is written a moment after the
/// process started; a process that started later than this after the recorded time is another
/// process that reuses the pid.
const START_SLACK_MS: u64 = 2_000;

/// Whether the process named by `id` still runs: `ps -o etime= -p <pid>` gives its elapsed time,
/// so its start is about now − etime; a pid whose process started after `id.start_time` (plus
/// slack) was recycled, and the original is gone. If `ps` cannot run, nothing is provable, and
/// the answer is alive.
#[allow(clippy::disallowed_methods, reason = "authority: daemon-crash-probe")]
pub fn identity_alive(id: &ProcessIdentity) -> bool {
    let out = Command::new("ps")
        .args(["-o", "etime=", "-p", &id.pid.to_string()])
        .env_remove(READY_FD_ENV)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return true;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(elapsed_s) = text.lines().find_map(|l| parse_etime(l.trim())) else {
        return false;
    };
    let started = Timestamp::now().0.saturating_sub(elapsed_s.saturating_mul(1000));
    started <= id.start_time.0.saturating_add(START_SLACK_MS)
}

/// `[[dd-]hh:]mm:ss` → seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let num = |p: &str| if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse::<u64>().ok() } else { None };
    let (days, clock) = match s.split_once('-') {
        Some((d, rest)) => (num(d)?, rest),
        None => (0, s),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [m, sec] => (0, num(m)?, num(sec)?),
        [h, m, sec] => (num(h)?, num(m)?, num(sec)?),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

/// Two-scan confirmation. Holds only what the previous scan saw; no globals.
#[derive(Debug, Default)]
pub struct CrashScanner {
    missing: BTreeMap<SessionId, ProcessIdentity>,
    reported: HashSet<(SessionId, ProcessIdentity)>,
}

impl CrashScanner {
    pub fn new() -> CrashScanner {
        CrashScanner::default()
    }

    /// Returns the candidates whose identity was dead on this scan and on the previous one (the
    /// same identity both times). A live sighting, a changed identity, or a session that is no
    /// longer a candidate resets the count; a confirmed `(session, identity)` is returned once.
    pub fn scan(
        &mut self,
        candidates: &[(SessionId, ProcessIdentity)],
        alive: &dyn Fn(&ProcessIdentity) -> bool,
    ) -> Vec<(SessionId, ProcessIdentity)> {
        let present: BTreeSet<SessionId> = candidates.iter().map(|(s, _)| *s).collect();
        self.missing.retain(|s, _| present.contains(s));
        let mut confirmed = Vec::new();
        for &(session, id) in candidates {
            if alive(&id) {
                self.missing.remove(&session);
                continue;
            }
            if self.reported.contains(&(session, id)) {
                continue;
            }
            if self.missing.get(&session) == Some(&id) {
                self.missing.remove(&session);
                self.reported.insert((session, id));
                confirmed.push((session, id));
            } else {
                self.missing.insert(session, id);
            }
        }
        confirmed
    }
}

fn branch(log: &dyn LogStore) -> Result<Vec<Entry>, String> {
    match log.head() {
        Some(h) => log.read_branch(h).map_err(|e| format!("the session log could not be read ({e}); we changed nothing")),
        None => Ok(Vec::new()),
    }
}

fn folded(entries: &[Entry]) -> Result<State, String> {
    fold(entries, &CustomFolds::new())
        .map_err(|e| format!("the session log does not fold ({e}); we changed nothing. Inspect the log before resuming it"))
}

fn in_flight(s: &State) -> bool {
    s.turn.is_some() || !s.open_calls.is_empty() || !s.open_ops.is_empty()
}

/// The last `Daemon/Crashed` body on the branch that no recovery notice has answered yet.
fn marker(entries: &[Entry]) -> Option<(EntryId, Value)> {
    let mut found = None;
    for e in entries {
        match &e.kind {
            EntryKind::Custom { ns, name, body } if ns == NS && name == CRASHED => found = Some((e.id, body.clone())),
            EntryKind::UserMessage { form: Form::Notice, .. } if matches!(&e.by, Principal::System { name } if name == RECOVERY) => {
                found = None
            }
            _ => {}
        }
    }
    found
}

fn marked_identity(body: &Value) -> Option<ProcessIdentity> {
    let pid = u32::try_from(body.get("pid")?.as_u64()?).ok()?;
    let start_time = Timestamp(body.get("start_time")?.as_u64()?);
    Some(ProcessIdentity { pid, start_time })
}

/// The unresolved crash marker on the current branch, if any: `(entry id, body)`.
pub fn crashed_marker(log: &dyn LogStore) -> Option<(EntryId, Value)> {
    marker(&branch(log).ok()?)
}

/// The identity to check when this session may have been orphaned by a crash: the last
/// `SessionMeta.process` differs from `me` and the fold shows work in flight (an open turn, open
/// calls or open ops). An identity already marked crashed on this branch is not a candidate
/// again, so a restarted daemon does not mark it twice.
pub fn candidate(log: &dyn LogStore, me: ProcessIdentity) -> Option<ProcessIdentity> {
    let meta = log.tail_meta()?;
    let EntryKind::SessionMeta { process: Some(process), .. } = meta.kind else {
        return None;
    };
    if process == me {
        return None;
    }
    let entries = branch(log).ok()?;
    if !in_flight(&folded(&entries).ok()?) {
        return None;
    }
    if marker(&entries).and_then(|(_, body)| marked_identity(&body)) == Some(process) {
        return None;
    }
    Some(process)
}

/// Appends `Daemon/Crashed{pid, start_time, open_turn, open_calls, uncertain}`; `uncertain` names
/// the tools of open ops with an external effect (they may or may not have happened).
pub fn mark_crashed(log: &dyn LogStore, identity: ProcessIdentity) -> Result<EntryId, String> {
    let state = folded(&branch(log)?)?;
    let uncertain: Vec<String> = state
        .open_ops
        .values()
        .filter(|(_, _, external)| *external)
        .map(|(call, _, _)| state.calls.get(call).map(|c| c.0.clone()).unwrap_or_else(|| call.0.clone()))
        .collect();
    let open_calls: Vec<String> = state.open_calls.iter().map(|c| c.0.clone()).collect();
    let body = json!({
        "pid": identity.pid,
        "start_time": identity.start_time.0,
        "open_turn": state.turn.map(|t| t.0),
        "open_calls": open_calls,
        "uncertain": uncertain,
    });
    log.append(None, Principal::system("daemon"), EntryKind::Custom { ns: NS.to_string(), name: CRASHED.to_string(), body })
        .map(|e| e.id)
        .map_err(|e| format!("could not record the crash in the session log ({e}); the session stays as it was"))
}

/// Recovers a session marked crashed. If the head is inside an open turn, forks at the last
/// `TurnEnd` whose fold has no open calls or ops (the turn in flight is lost; nothing is copied or
/// rewritten). Then appends a `Notice` saying so and listing the tools that may or may not have
/// run. Returns the fork point (`None` when the head was not inside a turn and no fork was
/// needed).
pub fn resume_crashed(log: &dyn LogStore) -> Result<Option<EntryId>, String> {
    let entries = branch(log)?;
    let (_, body) = marker(&entries).ok_or_else(|| {
        "no unresolved crash is recorded on this session, so there is nothing to resume; we changed nothing. Open it normally".to_string()
    })?;
    let fork = if folded(&entries)?.turn.is_some() {
        let folds = CustomFolds::new();
        let mut s = State::default();
        let mut at = None;
        for e in &entries {
            s = step(s, e, &folds).map_err(|err| format!("the session log does not fold ({err}); we changed nothing"))?;
            if matches!(e.kind, EntryKind::TurnEnd { .. }) && s.open_calls.is_empty() && s.open_ops.is_empty() {
                at = Some(e.id);
            }
        }
        let at = at.ok_or_else(|| {
            "the crash hit before any turn completed with no calls in flight, so there is no point to fork at; we left the log as it is. Start a new session (the crashed log stays for inspection)".to_string()
        })?;
        log.fork(at).map_err(|e| format!("could not move the session back to entry {} ({e}); we changed nothing", at.0))?;
        Some(at)
    } else {
        None
    };
    log.append(None, Principal::system(RECOVERY), EntryKind::UserMessage { form: Form::Notice, text: notice(&body), client_key: None })
        .map_err(|e| format!("the session was moved back to the last completed turn, but the recovery notice could not be written ({e}). Resume again to retry"))?;
    Ok(fork)
}

fn notice(body: &Value) -> String {
    let pid = body.get("pid").and_then(Value::as_u64).map(|p| format!(" (pid {p})")).unwrap_or_default();
    let tools: Vec<&str> =
        body.get("uncertain").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut text = format!(
        "The previous turn was lost: the harness process{pid} crashed while it was running, and this session was resumed from the last completed turn."
    );
    if tools.is_empty() {
        text.push_str(" No tool with an external effect was running, so there is nothing to double-check.");
    } else {
        text.push_str(&format!(
            " These tools were running and may or may not have run: {}. Check their effects before repeating them.",
            tools.join(", ")
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::parse_etime;

    #[test]
    fn etime_formats() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(parse_etime("01:02:03"), Some(3_723));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3_723));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("7"), None);
        assert_eq!(parse_etime("a:b"), None);
        assert_eq!(parse_etime("1-2-03:04"), None);
    }
}
