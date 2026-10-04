//! `aho jobs`: a session's background jobs (`list`, `kill <op>`, `detach <op>`). A job lives in
//! the process that runs its session. When the daemon holds the session the verbs go through it:
//! `job.list` needs no ownership, `job.kill` / `job.detach` run on a connection that drives the
//! session for the command (refused while another client drives it there). A session another
//! process drives is out of reach (its own `/jobs` shows them); in a session no process runs, no
//! job runs: `list` folds the log (open jobs are re-run or delivered when it next starts) and
//! `kill` / `detach` have nothing to act on.

use aho_log::State;
use aho_types::*;
use serde_json::json;

use crate::cli::JobsVerb;
use crate::ctx::{Ctx, Fail, branch, err, find_session, hex, owner_alive};
use crate::serve::Conn;
use crate::session::state_of;

pub async fn run(ctx: &Ctx, sub: &JobsVerb, session: Option<&str>, json_out: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    if let Some((socket, conn, _)) = crate::serve::holding(ctx, &dir, s.id).await? {
        let via = format!("through the daemon on {}", socket.display());
        let (method, op, done) = match sub {
            JobsVerb::List => {
                let f = conn.request("job.list", json!({ "session": s.id })).await.map_err(err)?;
                let jobs = f.payload["jobs"].as_array().cloned().unwrap_or_default();
                return list(json_out, s.id, &jobs, Some(&via));
            }
            JobsVerb::Kill { op } => ("job.kill", *op, "cancelled job"),
            JobsVerb::Detach { op } => ("job.detach", *op, "detached into a job:"),
        };
        drive(&conn, s.id, method, op).await.map_err(err)?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "session": hex(s.id), "op": op, "done": method }))?);
        } else {
            let after = if method == "job.kill" {
                "its cancelled outcome reaches the model at the next turn"
            } else {
                "its call was answered with the job id; its outcome reaches the model when it finishes"
            };
            println!("{done} op{} in session {} ({via}); {after}", op.0, hex(s.id));
        }
        return Ok(());
    }
    let (_, entries) = branch(&s.path)?;
    let st = state_of(&entries)?;
    let driver = st.meta.as_ref().and_then(|(_, _, p)| p.as_ref()).filter(|p| owner_alive(p)).map(|p| p.pid);
    match (sub, driver) {
        (JobsVerb::List, None) => list(json_out, s.id, &logged(&st), None),
        (_, Some(pid)) => Err(err(format!(
            "session {} is driven by process {pid} right now and its jobs live there, so nothing was read or changed here. Use `/jobs` in that client, or run the session under `aho serve` to reach its jobs from here",
            hex(s.id)
        ))),
        (_, None) => Err(err(format!(
            "session {} is not running, so no job runs and nothing was changed; its open jobs are re-run or delivered when it next starts",
            hex(s.id)
        ))),
    }
}

/// `job.kill` / `job.detach` on a connection that drives the session for this command only.
async fn drive(conn: &Conn, sid: SessionId, method: &str, op: OpId) -> Result<(), String> {
    let f = conn.request("session.attach", json!({ "session": sid, "has_local_history": true, "watch": false })).await?;
    if f.payload["takeover"] != json!("allowed") {
        let why = f.payload["reason"].as_str().unwrap_or("another client drives it");
        return Err(format!(
            "session {} is open in the daemon and another client drives it there ({why}), so nothing was changed. Use `/jobs` in that client",
            hex(sid)
        ));
    }
    let done = conn.request(method, json!({ "session": sid, "op": op })).await;
    let released = conn.request("session.detach", json!({ "session": sid })).await;
    done?;
    released
        .map(|_| ())
        .map_err(|e| format!("done, but the daemon did not let go of the session ({e}); reconnect to it from its own client"))
}

/// Open jobs as the log has them: no process runs the session, so none is running.
fn logged(st: &State) -> Vec<Value> {
    st.jobs
        .iter()
        .map(|(op, j)| {
            let tool = st.calls.get(&j.call).map(|c| c.0.clone()).unwrap_or_default();
            json!({ "op": op, "call": j.call, "tool": tool, "cause": j.cause, "status": j.delivered, "started": null })
        })
        .collect()
}

/// `via`: the daemon that runs the jobs; `None` = read from the log of a session nothing runs.
fn list(json_out: bool, sid: SessionId, jobs: &[Value], via: Option<&str>) -> Result<(), Fail> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "session": hex(sid), "live": via.is_some(), "jobs": jobs }))?);
        return Ok(());
    }
    let whence = via.map(|v| format!(" ({v})")).unwrap_or_else(|| " (from its log; the session is not running)".into());
    if jobs.is_empty() {
        println!("session {} has no open jobs{whence}", hex(sid));
        return Ok(());
    }
    println!("session {}: {} open job(s){whence}", hex(sid), jobs.len());
    let now = Timestamp::now().0;
    for j in jobs {
        let state = match (&j["status"], j["started"].as_u64()) {
            (Value::Null, _) if via.is_none() => "not running: re-run or delivered when the session next starts".to_string(),
            (Value::Null, Some(t)) => format!("running {}", elapsed(now.saturating_sub(t))),
            (Value::Null, None) => "running".to_string(),
            (s, _) => format!("{}, delivered at the next turn", s["status"].as_str().unwrap_or("finished")),
        };
        let cause = match (j["cause"]["cause"].as_str(), j["cause"]["ms"].as_u64()) {
            (Some("after"), Some(ms)) => format!("detached after {}", elapsed(ms)),
            (Some("requested"), _) => "started in the background".to_string(),
            (Some(c), _) => format!("detached by the {c}"),
            (None, _) => String::new(),
        };
        println!("  op{:<5} {:<12} {state}  · {cause}", j["op"], j["tool"].as_str().unwrap_or("?"));
    }
    Ok(())
}

fn elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 { format!("{s}s") } else { format!("{}m{:02}s", s / 60, s % 60) }
}
