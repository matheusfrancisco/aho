//! `12` "Self-dev reload" from the operator's side: `aho selfdev reload` publishes a build,
//! smoke-boots it, flips `current`/`canary`, and hands the running daemon over to it; `status`
//! folds the ledger; `rollback` reloads into the build that ran before. Every phase is an entry in
//! the builds' ledger (`<builds>/ledger.jsonl`, a log only this command writes: the sessions'
//! own logs belong to the daemon while it runs), so what happened is a fold, not a guess.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aho_log::{EntryKind, JsonlLog, LogStore};
use aho_types::{SessionId, Value};
use selfdev::{Builds, Channel, SelfdevConfig};
use serde_json::json;

use crate::cli::SelfdevVerb;
use crate::ctx::{Ctx, Fail, err, find_session};

struct Setup {
    builds: Builds,
    smoke_timeout: Duration,
    ledger: JsonlLog,
    dir: PathBuf,
}

fn setup(ctx: &Ctx) -> Result<Setup, Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    match p.plan.row("selfdev").map(|r| &r.state) {
        Some(aho_log::CapState::Active) => {}
        Some(s) => {
            return Err(Fail::Plan(format!(
                "self-dev is {} here ({}), so nothing was published. It needs the host world: `aho set seams.world host`",
                aho_inspect::state_name(s),
                p.plan
                    .row("selfdev")
                    .map(|r| r.missing.iter().map(|m| m.requirement.clone()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            )));
        }
        None => return Err(err("this aho was built without self-dev (the `selfdev` capability is not linked)")),
    }
    let cfg = <SelfdevConfig as aho_cap::Config>::from_loadout(&p.loadout).map_err(|e| err(format!("selfdev settings: {e}")))?;
    let root = selfdev::expand_home(&cfg.builds_dir, Some(inputs.home.as_os_str())).map_err(err)?;
    std::fs::create_dir_all(&root).map_err(|e| err(format!("could not create the builds directory {} ({e})", root.display())))?;
    let ledger = JsonlLog::open(root.join("ledger.jsonl"))?;
    Ok(Setup { builds: Builds::new(root), smoke_timeout: cfg.smoke_timeout, ledger, dir: ctx.session_dir(&p) })
}

/// The `Selfdev` fold over the ledger's current branch.
fn folded(log: &JsonlLog) -> Result<Value, Fail> {
    let Some(head) = log.head() else { return Ok(Value::Null) };
    let mut state: Option<Value> = None;
    for e in log.read_branch(head)? {
        if let EntryKind::Custom { ns, name, body } = &e.kind
            && ns == selfdev::NS
        {
            state = Some(
                selfdev::fold(state.as_ref(), name, body).map_err(|m| err(format!("the ledger does not fold at entry {}: {m}", e.id.0)))?,
            );
        }
    }
    Ok(state.unwrap_or(Value::Null))
}

fn session_id(dir: &Path, which: Option<&str>) -> Result<Option<SessionId>, Fail> {
    match which {
        Some(w) => Ok(Some(find_session(dir, Some(w))?.id)),
        None => Ok(None),
    }
}

fn short(h: &str) -> &str {
    h.get(..12).unwrap_or(h)
}

/// Publish → smoke → activate → hand the daemon over; the ledger records every phase.
async fn reload_into(s: &Setup, binary: &Path, socket: &Path, session: Option<SessionId>, timeout: u64) -> Result<String, Fail> {
    // Nothing to hand over to without a daemon; refuse before publishing anything.
    let conn = crate::serve::Conn::connect(socket).await.map_err(|e| {
        err(format!("{e}. A reload hands a running daemon's sessions to the new build; start one with `aho serve --detach` first"))
    })?;
    // The session that asked continues first after the reload, so the daemon must hold it.
    if let Some(sid) = session {
        conn.request("session.open", json!({ "id": sid })).await.map_err(|e| err(format!("{e}; nothing was published")))?;
    }
    drop(conn);
    let sock = socket.to_path_buf();
    let wait = Duration::from_secs(timeout);
    let op = crate::boot::operator();
    let hash = selfdev::reload(&op, &s.builds, &s.ledger, binary, s.smoke_timeout, move |path| {
        Box::pin(async move { daemon::client::reload_and_wait(&sock, Some(path), session, wait).await })
    })
    .await
    .map_err(|e| err(e.to_string()))?;
    Ok(hash)
}

pub async fn run(ctx: &Ctx, sub: &SelfdevVerb) -> Result<(), Fail> {
    match sub {
        SelfdevVerb::Reload { binary, socket, session, timeout } => {
            let s = setup(ctx)?;
            let binary = match binary {
                Some(b) => ctx.typed_path(b),
                None => std::env::current_exe().map_err(|e| err(format!("could not find this aho binary ({e}); pass --binary")))?,
            };
            let socket = ctx.socket(socket.as_deref());
            let sid = session_id(&s.dir, session.as_deref())?;
            let hash = reload_into(&s, &binary, &socket, sid, *timeout).await?;
            println!(
                "published {} from {}, smoke passed, `current` and `canary` point at it, and the daemon on {} now runs it",
                short(&hash),
                binary.display(),
                socket.display()
            );
            println!("undo with `aho selfdev rollback`; the phases are in {}", s.builds.root().join("ledger.jsonl").display());
            Ok(())
        }
        SelfdevVerb::Status { json } => {
            let s = setup(ctx)?;
            let state = folded(&s.ledger)?;
            let disk: serde_json::Map<String, Value> = Channel::ALL
                .iter()
                .map(|c| (c.name().to_string(), s.builds.channel(*c).map(Value::String).unwrap_or(Value::Null)))
                .collect();
            if *json {
                println!("{}", serde_json::to_string_pretty(&json!({"builds": s.builds.root(), "channels": disk, "ledger": state}))?);
                return Ok(());
            }
            println!("builds in {}", s.builds.root().display());
            for c in Channel::ALL {
                let on_disk = s.builds.channel(c);
                let logged = state.get("channels").and_then(|m| m.get(c.name())).and_then(Value::as_str);
                let drift = match (on_disk.as_deref(), logged) {
                    (a, b) if a == b => String::new(),
                    (_, Some(l)) => format!("   ⚠ the ledger says {}", short(l)),
                    (_, None) => "   ⚠ the ledger never set it".into(),
                };
                println!("  {:<8} {}{drift}", c.name(), on_disk.as_deref().map(short).unwrap_or("(none)"));
            }
            let published = state.get("published").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            println!("  published builds: {published}");
            if let Some(h) = state.get("running").and_then(Value::as_str) {
                println!("  last handoff: {} (ok)", short(h));
            }
            if let Some(p) = state.get("pending").filter(|p| !p.is_null()) {
                println!(
                    "  pending activation: {} (no handoff recorded yet)",
                    p.get("hash").and_then(Value::as_str).map(short).unwrap_or("?")
                );
            }
            match state.get("restore").and_then(|r| r.get("current")).and_then(Value::as_str) {
                Some(h) => println!("  rollback would restore: {}", short(h)),
                None => println!("  rollback: nothing earlier to restore"),
            }
            Ok(())
        }
        SelfdevVerb::Rollback { socket, session, timeout } => {
            let s = setup(ctx)?;
            let state = folded(&s.ledger)?;
            let Some(target) = state.get("restore").and_then(|r| r.get("current")).and_then(Value::as_str).map(str::to_string) else {
                return Err(err(
                    "there is no earlier build to roll back to: the ledger has no handoff after an earlier build ran. `aho selfdev status` shows the channels",
                ));
            };
            let binary = s.builds.binary(&target);
            let socket = ctx.socket(socket.as_deref());
            let sid = session_id(&s.dir, session.as_deref())?;
            // A rollback is a reload into the earlier build, through the same logged phases.
            let hash = reload_into(&s, &binary, &socket, sid, *timeout).await?;
            println!("rolled back: `current` and `canary` point at {} again, and the daemon on {} runs it", short(&hash), socket.display());
            Ok(())
        }
    }
}
