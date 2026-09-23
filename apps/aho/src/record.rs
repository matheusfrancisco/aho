//! Operator entries for a session some process may be running (03, 05, 12). Two processes never
//! write one log: this process appends when nothing drives the session; when the daemon holds
//! it, the entries go through the daemon's `session.command` on a connection that drives the
//! session for the command (a client driving it there makes this one a read-only stranger, so
//! nothing is written); a live process of its own refuses it.

use std::path::{Path, PathBuf};

use aho_compose::Planned;
use aho_log::{CapDeltaOp, EntryId, EntryKind, IndexedLog, LogStore, State};
use aho_types::*;
use serde_json::json;

use crate::ctx::{Ctx, Fail, SessionFile, branch, err, find_session, hex, owner_alive, sessions};
use crate::serve::Conn;
use crate::session::state_of;
use surface_rpc::caps::plan_entries;

pub enum Writer {
    /// Nothing drives the session: this process appends (through the session index).
    Local(IndexedLog),
    /// The daemon holds it; this connection drives it until [`Writer::finish`].
    Daemon { conn: Conn, id: SessionId, socket: PathBuf, running: bool },
}

impl Writer {
    /// How this process may write operator entries into session `s` (folded as `st`). `Err`
    /// says who drives it instead.
    pub async fn open(ctx: &Ctx, dir: &Path, s: &SessionFile, st: &State) -> Result<Writer, String> {
        if let Some((socket, conn, status)) = crate::serve::holding(ctx, dir, s.id).await.map_err(|e| e.to_string())? {
            let f = conn.request("session.attach", json!({ "session": s.id, "has_local_history": true, "watch": false })).await?;
            if f.payload["takeover"] != json!("allowed") {
                let why = f.payload["reason"].as_str().unwrap_or("another client drives it");
                return Err(format!(
                    "session {} is open in the daemon on {} and another client drives it there ({why})",
                    hex(s.id),
                    socket.display()
                ));
            }
            return Ok(Writer::Daemon { conn, id: s.id, socket, running: status == "running" });
        }
        if let Some(p) = st.meta.as_ref().and_then(|(_, _, p)| p.as_ref()).filter(|p| owner_alive(p)) {
            return Err(format!("session {} is driven by process {} right now", hex(s.id), p.pid));
        }
        crate::ctx::open_writer(dir, s.id).map(Writer::Local).map_err(|e| e.to_string())
    }

    /// A turn is running in the daemon's copy of the session.
    pub fn running(&self) -> bool {
        matches!(self, Writer::Daemon { running: true, .. })
    }

    /// Excludes schedule/workflow turns until this connection reopens or releases the session.
    pub async fn begin_mutation(&self) -> Result<(), String> {
        if let Writer::Daemon { conn, id, .. } = self {
            conn.request("session.mutation", json!({ "session": id })).await?;
        }
        Ok(())
    }

    /// Appends one entry by `Operator`.
    pub async fn append(&self, kind: EntryKind) -> Result<EntryId, String> {
        match self {
            Writer::Local(log) => log.append(None, Principal::Operator, kind).map(|e| e.id).map_err(|e| e.to_string()),
            Writer::Daemon { conn, id, .. } => {
                let f = conn.request("session.command", json!({ "session": id, "kind": kind })).await?;
                serde_json::from_value(f.payload["entry"].clone()).map_err(|e| format!("the daemon's answer names no entry ({e})"))
            }
        }
    }

    /// A host method over the daemon's live session (`plan-mode.enter`, `caps.*`, …). `None`
    /// when this process holds the log itself.
    pub async fn call(&self, method: &str, params: Value) -> Option<Result<Value, String>> {
        match self {
            Writer::Local(_) => None,
            Writer::Daemon { conn, id, .. } => {
                Some(conn.request("session.call", json!({ "session": id, "method": method, "params": params })).await.map(|f| f.payload))
            }
        }
    }

    /// Ends the command. Through the daemon, `replan` first boots the session again so it runs
    /// under what was recorded (`session.reopen`); then this connection lets go of it. Returns a
    /// sentence for the operator when there is something to say.
    pub async fn finish(self, replan: bool) -> Result<Option<String>, String> {
        let Writer::Daemon { conn, id, socket, .. } = self else { return Ok(None) };
        let reopened = if replan { Some(conn.request("session.reopen", json!({ "session": id })).await) } else { None };
        let released = conn.request("session.detach", json!({ "session": id })).await;
        let note = match reopened {
            None => format!("through the daemon on {}", socket.display()),
            Some(Ok(f)) => {
                let problems: Vec<String> =
                    f.payload["problems"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect();
                let tail = if problems.is_empty() { String::new() } else { format!(" (closing it reported: {})", problems.join("; ")) };
                format!("through the daemon on {}, which reopened the session under the new plan{tail}", socket.display())
            }
            Some(Err(e)) => format!(
                "through the daemon on {}; it could not reopen the session under the new plan ({e}), so the running session keeps its old plan until it next starts",
                socket.display()
            ),
        };
        released.map_err(|e| format!("{note}, but it did not let go of the session ({e}); reconnect to it from its own client"))?;
        Ok(Some(note))
    }
}

fn op_name(op: &CapDeltaOp) -> &'static str {
    match op {
        CapDeltaOp::Activated => "Activated",
        CapDeltaOp::Deactivated => "Deactivated",
        CapDeltaOp::Broken { .. } => "Broken",
        CapDeltaOp::Replaced { .. } => "Replaced",
        CapDeltaOp::Rejected { .. } => "Rejected",
    }
}

/// The entry as `aho` names it on screen (05: `Cap/Delta{Activated skill.deploy, by: Operator}`).
fn render(k: &EntryKind) -> Option<String> {
    Some(match k {
        EntryKind::CapDelta { id, op, .. } => format!("Cap/Delta{{{} {id}, by: Operator}}", op_name(op)),
        EntryKind::ConfigResolved { key, sourced } => format!("Config/Resolved{{{key} = {}, by: Operator}}", sourced.value),
        EntryKind::PolicyEpoch { epoch, preset, allowed, .. } => format!("Policy/Epoch{{{}, {preset}: {allowed}, by: Operator}}", epoch.0),
        EntryKind::PlanSealed { plan_hash, .. } => format!("Plan/Sealed{{{}, by: Operator}}", plan_hash.short()),
        _ => return None,
    })
}

/// `03`/`05`: after an operator command wrote a config file, the session it concerns (`which`,
/// or the latest) records the change — `CommandRun{name, args}`, the entries that make its fold
/// match `after` (config, capability states, policy epoch citing the command, plan seal),
/// `CommandDone` — so the running session and every projection see it. Through the daemon the
/// session is then reopened under the new plan. Prints what was appended, or why nothing was.
pub async fn record_change(ctx: &Ctx, after: &Planned, which: Option<&str>, name: &str, args: Vec<String>) -> Result<(), Fail> {
    let dir = ctx.session_dir(after);
    let target = match which {
        Some(w) => Some(find_session(&dir, Some(w))?),
        None => sessions(&dir)?.into_iter().next(),
    };
    let Some(s) = target else { return Ok(()) };
    let (_, entries) = branch(&s.path)?;
    let st = state_of(&entries)?;
    let command = format!("aho {name} {}", args.join(" "));
    let w = match Writer::open(ctx, &dir, &s, &st).await {
        Ok(w) => w,
        Err(why) => {
            println!("not appended: {why}. It keeps the plan it booted with; the change applies when it next starts");
            return Ok(());
        }
    };
    if st.turn.is_some() || w.running() {
        let released = w.finish(false).await;
        println!("not appended: session {} is inside a turn. The change applies when it next starts (resuming records it)", hex(s.id));
        return released.map(|_| ()).map_err(err);
    }
    if let Err(why) = w.begin_mutation().await {
        w.finish(false).await.map_err(err)?;
        println!("not appended: {why}. The change applies when the session next starts");
        return Ok(());
    }
    // A driver may have finished a turn since the first fold. Read again after admission is held.
    let (_, entries) = branch(&s.path)?;
    let st = state_of(&entries)?;
    let kinds = plan_entries(&st, after, EntryId(0), &command);
    if kinds.is_empty() {
        w.finish(false).await.map_err(err)?;
        println!("session {} already runs under this plan; nothing appended", hex(s.id));
        return Ok(());
    }
    let run = w.append(EntryKind::CommandRun { name: name.to_string(), args, key: None }).await.map_err(err)?;
    // Built again now that the command has its id: the policy epoch cites it as its authority (04).
    let kinds = plan_entries(&st, after, run, &command);
    let lines: Vec<String> = kinds.iter().filter_map(render).collect();
    for k in kinds {
        w.append(k).await.map_err(err)?;
    }
    w.append(EntryKind::CommandDone { run, kind: "ok".into() }).await.map_err(err)?;
    let via = w.finish(true).await.map_err(err)?;
    for l in &lines {
        println!("appended {l}  → session {}", hex(s.id));
    }
    if let Some(v) = via {
        println!("  ({v})");
    }
    Ok(())
}
