//! Verbs that open a session log: run, session list/show/verify, checkpoints,
//! rewind, shell, status, rpc.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use aho_approval::{Approval, ApprovalRequest, ChannelApproval, Decided, NeverApproval};
use aho_cap::Registry;
use aho_compose::{LockFile, Planned};
use aho_llm::LateResults;
use aho_log::{CheckpointCause, Entry, EntryKind, GrantScope, JsonlLog, RewindMode, State, Transition};
use aho_policy::Operator;
use aho_session::{Session, SessionEvent};
use aho_types::*;
use serde_json::json;
use surface_print::{Mode, Printer, Style};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::approval::Answer;
use crate::cli::RunArgs;
use crate::ctx::{Ctx, Fail, SessionFile, branch, color, err, find_session, hex, new_session_id, owner_alive, sessions, stdin_tty, when};

pub(crate) fn operator() -> Operator {
    crate::boot::operator()
}

pub(crate) struct Opened {
    pub(crate) session: Arc<Session>,
    pub(crate) id: SessionId,
    pub(crate) path: PathBuf,
    pub(crate) resumed: bool,
    pub(crate) approvals: Option<mpsc::Receiver<ApprovalRequest>>,
    /// Schedule/workflow drivers, for long-lived hosts (`drive = true`).
    pub(crate) drivers: Option<session_drive::Drivers>,
}

/// Boots (or resumes) a session over `<session.dir>/<id>.jsonl`. `prompts` gives it an approval
/// channel; `drive` hosts its schedule and workflow drivers (tui, web, webhook).
pub(crate) async fn open(
    ctx: &Ctx,
    p: &Planned,
    home: &std::path::Path,
    which: Option<&str>,
    prompts: bool,
    drive: bool,
) -> Result<Opened, Fail> {
    let host = crate::boot::Host::new(ctx, p.clone(), home);
    let (id, resumed) = match which {
        Some(w) => (find_session(&host.dir, Some(w))?.id, true),
        None => (new_session_id(), false),
    };
    let path = host.log_path(id);
    crate::serve::refuse_if_daemon_owns(ctx, &host.dir, id).await?;
    let log = Arc::new(crate::ctx::open_writer(&host.dir, id)?);
    let (approval, approvals): (Arc<dyn Approval>, _) = if prompts {
        let (a, rx) = ChannelApproval::new(4);
        (Arc::new(a), Some(rx))
    } else {
        (Arc::new(NeverApproval), None)
    };
    let live = host.boot(id, log, approval, drive).await.map_err(err)?;
    Ok(Opened { session: live.session, id, path, resumed, approvals, drivers: live.drivers })
}

fn tool_caps(reg: &Registry) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = reg.tools.iter().map(|(c, d, _)| (d.name.to_string(), c.name.to_string())).collect();
    m.extend(reg.dynamic_tools.iter().map(|(c, s, _)| (s.name.clone(), c.name.to_string())));
    m
}

async fn next_card(rx: &mut Option<mpsc::Receiver<ApprovalRequest>>) -> Option<ApprovalRequest> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

type Out = Printer<std::io::Stdout, std::io::Stderr>;

/// `[p]` on a card: the same plan → apply as `aho preset <name>`, confirmed by the keypress.
fn write_preset(writer: &surface_rpc::CapsContext, name: &str) -> Result<Vec<String>, surface_rpc::CapsError> {
    let inputs = writer.fresh_inputs()?;
    let prop = surface_rpc::caps::propose(&writer.composition, &inputs, vec![("policy.preset".into(), Value::String(name.to_string()))])?;
    print!("{}", prop.render());
    prop.commit()
}

/// Renders events in order; approval cards are shown and answered inline, so a card never
/// interleaves with streaming text. Stops at `done`, after draining what is already queued.
async fn print_loop(
    mut p: Out,
    mut events: broadcast::Receiver<SessionEvent>,
    mut cards: Option<mpsc::Receiver<ApprovalRequest>>,
    mut done: oneshot::Receiver<()>,
    caps: BTreeMap<String, String>,
    writer: surface_rpc::CapsContext,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            biased;
            ev = events.recv() => match ev {
                Ok(ev) => p.event(&ev)?,
                Err(broadcast::error::RecvError::Lagged(n)) => p.event(&SessionEvent::Error(format!("the display fell {n} events behind; `aho session show` has the full transcript")))?,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            card = next_card(&mut cards) => match card {
                Some((card, reply)) => {
                    p.finish()?;
                    print!("{}", surface_print::render_approval(&card, caps.get(&card.tool).map(String::as_str)));
                    std::io::stdout().flush()?;
                    let preset = surface_print::preset_cut(&card);
                    let offer = preset.is_some();
                    let answer = tokio::task::spawn_blocking(move || crate::approval::read_answer(offer))
                        .await
                        .unwrap_or(Answer::Decided(Decided::cancelled()));
                    let decided = match (answer, preset) {
                        (Answer::Preset, Some(name)) => match write_preset(&writer, &name) {
                            Ok(lines) => {
                                for l in lines {
                                    println!("{l}");
                                }
                                Decided::allowed(GrantScope::Session)
                            }
                            Err(e) => {
                                p.event(&SessionEvent::Error(format!("the preset was not written ({e}); the call is cancelled")))?;
                                Decided::cancelled()
                            }
                        },
                        (Answer::Decided(d), _) => d,
                        (Answer::Preset, None) => Decided::cancelled(),
                    };
                    if reply.send(decided).is_err() {
                        p.event(&SessionEvent::Error("the call stopped waiting before the answer arrived (turn aborted)".into()))?;
                    }
                }
                None => cards = None,
            },
            _ = &mut done => break,
        }
    }
    while let Ok(ev) = events.try_recv() {
        p.event(&ev)?;
    }
    p.finish()
}

fn read_line_blocking() -> Option<String> {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s),
    }
}

pub async fn run(ctx: &Ctx, a: &RunArgs) -> Result<(), Fail> {
    // While `aho serve` runs, it owns this project's sessions and `aho run` is its client.
    // Settings for this invocation only (a script, --set, a pinned plan) need a plan of their
    // own, so those runs stay in this process.
    let g = &ctx.global;
    let own_plan = a.script.is_some()
        || a.lock
        || a.plan.is_some()
        || a.strict
        || !g.sets.is_empty()
        || !g.disable.is_empty()
        || g.profile.is_some()
        || g.loadout.is_some();
    if !own_plan && let Some((socket, conn)) = crate::serve::live(ctx).await {
        if !a.json {
            eprintln!("(through the daemon on {}; `aho serve --stop` stops it)", socket.display());
        }
        let args = crate::cli::AttachArgs { session: a.session.clone(), socket: None, prompt: a.prompt.clone(), json: a.json, hud: a.hud };
        return crate::serve::attach_with(ctx, &socket, conn, &args).await;
    }
    let mut sets = Vec::new();
    if let Some(script) = &a.script {
        let path = std::fs::canonicalize(ctx.typed_path(script))
            .map_err(|e| err(format!("model script {} cannot be opened ({e})", script.display())))?;
        sets.push(("provider.scripted.script".to_string(), path.display().to_string()));
        sets.push(("model.provider".to_string(), "scripted".to_string()));
    }
    let inputs = ctx.inputs(&sets, a.strict)?;
    let planned = ctx.load(&inputs)?;
    if a.lock {
        LockFile::read(&ctx.project)?.verify(&planned)?;
    }
    if let Some(want) = &a.plan {
        let have = planned.plan.hash.to_hex();
        if want.len() < 4 || !have.starts_with(want.as_str()) {
            return Err(Fail::Plan(format!(
                "the resolved plan {} is not the pinned plan {want}. The session was not started; run `aho plan` to see what changed.",
                planned.plan.hash.short()
            )));
        }
    }
    let prompts = !a.json && stdin_tty() && planned.loadout.str("policy.ask") != Some("never");
    let o = open(ctx, &planned, &inputs.home, a.session.as_deref(), prompts, false).await?;
    let mode = if a.json { Mode::Json } else { Mode::Text };
    let mut printer =
        Printer::new(mode, Style { color: !a.json && color(), hud: a.hud, echo_user: false }, std::io::stdout(), std::io::stderr())
            .with_registry(o.session.registry());
    let events = o.session.subscribe();
    if a.json {
        for e in o.session.subscribe_from(None) {
            printer.entry(&e)?;
        }
    } else if o.resumed {
        let brief = aho_inspect::catch_up(&o.session.fold()?);
        println!("{}", aho_inspect::render_catch_up(&brief));
    }
    let (done_tx, done_rx) = oneshot::channel();
    let printer_task =
        tokio::spawn(print_loop(printer, events, o.approvals, done_rx, tool_caps(o.session.registry()), ctx.caps(inputs.clone())));
    let aborter = {
        let s = o.session.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                s.abort();
            }
        })
    };

    let mut last = Transition::Completed;
    let mut ran = 0;
    let repl = a.prompt.is_none() && stdin_tty() && !a.json;
    if let Some(p) = &a.prompt {
        last = o.session.prompt(p.clone(), None).await?;
        ran += 1;
    } else if repl {
        loop {
            print!("› ");
            std::io::stdout().flush()?;
            let Some(line) = tokio::task::spawn_blocking(read_line_blocking).await.map_err(|e| err(e.to_string()))? else { break };
            if line.trim().is_empty() {
                continue;
            }
            last = o.session.prompt(line.trim_end().to_string(), None).await?;
            ran += 1;
        }
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        if !text.trim().is_empty() {
            last = o.session.prompt(text, None).await?;
            ran += 1;
        }
    }
    aborter.abort();
    // The printer drains what is queued and stops; if it already stopped, there is nothing to drain.
    if done_tx.send(()).is_err() {
        return Err(err("the transcript printer stopped early"));
    }
    printer_task.await.map_err(|e| err(e.to_string()))??;
    if !a.json {
        let paint = |s: String| if color() { format!("\x1b[2m{s}\x1b[0m") } else { s };
        println!(
            "{}",
            paint(format!(
                "session {} · {}  (resume: aho run --session {})",
                hex(o.id),
                o.path.display(),
                &hex(o.id)[..12.min(hex(o.id).len())]
            ))
        );
    }
    match last {
        _ if ran == 0 && !repl => Err(err("no prompt was given; pass -p <prompt> or pipe one on stdin")),
        Transition::Aborted(_) | Transition::PromptTooLong | Transition::MaxTurns => Err(Fail::Reported),
        _ => Ok(()),
    }
}

// ───────────────────────────── session list / show / verify ─────────────────────────────

pub(crate) fn state_of(entries: &[Entry]) -> Result<State, Fail> {
    Ok(aho_log::fold(entries, &Default::default())?)
}

pub fn list(ctx: &Ctx, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let mut rows: Vec<(SessionFile, String, String, u64, Option<String>)> = Vec::new();
    for s in sessions(&dir)? {
        let (_, entries) = branch(&s.path)?;
        let (status, reason, turns, last) = match state_of(&entries) {
            Ok(st) => {
                let alive = st.meta.as_ref().and_then(|(_, _, pi)| pi.as_ref()).is_some_and(owner_alive);
                let (status, reason) = aho_inspect::session_status(&st, alive);
                let last = aho_inspect::catch_up(&st).last_prompt;
                (format!("{status:?}"), reason, st.turns, last)
            }
            Err(e) => ("Invalid".into(), e.to_string(), 0, None),
        };
        rows.push((s, status, reason, turns, last));
    }
    if json {
        let v: Vec<Value> = rows
            .iter()
            .map(|(s, status, reason, turns, last)| json!({"id": hex(s.id), "path": s.path, "status": status, "reason": reason, "turns": turns, "last_prompt": last, "modified": when(s.modified)}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no sessions in {} yet; start one with `aho run`", dir.display());
    }
    for (s, status, _, turns, last) in rows {
        let last = last.map(|l| surface_print::clip(&l, 50)).unwrap_or_default();
        println!("{:<28} {:<9} {:>3} turns  {}  {last}", hex(s.id), status, turns, when(s.modified));
    }
    Ok(())
}

/// `aho session query`: one read of the index, never a directory scan.
pub fn query(ctx: &Ctx, text: &str, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let q = aho_inspect::parse_session_query(text).map_err(|e| err(format!("{e}; nothing was searched")))?;
    let page = crate::ctx::session_index(&ctx.session_dir(&p))?.query(&q).map_err(|e| err(e.to_string()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        println!("{}", aho_inspect::render_session_query(&page));
    }
    Ok(())
}

/// `aho session index [--rebuild]`: what the index covers; `--rebuild` re-reads every log.
pub fn index(ctx: &Ctx, rebuild: bool, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let index = crate::ctx::session_index(&ctx.session_dir(&p))?;
    let report = if rebuild { Some(index.rebuild().map_err(|e| err(e.to_string()))?) } else { None };
    let status = index.status().map_err(|e| err(e.to_string()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&json!({ "status": status, "rebuilt": report }))?);
        return Ok(());
    }
    if let Some(r) = &report {
        println!("rebuilt: {} session(s), {} entries", r.indexed_sessions, r.indexed_entries);
    }
    println!("{}: {} session(s), {} entries indexed", status.sessions_dir.display(), status.indexed_sessions, status.indexed_entries);
    for i in &status.issues {
        println!("  unavailable {}: {}", i.session_id, i.reason);
    }
    Ok(())
}

pub fn show(ctx: &Ctx, id: &str) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let s = find_session(&ctx.session_dir(&p), Some(id))?;
    let (_, entries) = branch(&s.path)?;
    let mut printer = Printer::new(Mode::Text, Style { color: color(), hud: false, echo_user: true }, std::io::stdout(), std::io::stderr())
        .with_manifests(&ctx.comp.manifests());
    for e in &entries {
        printer.entry(e)?;
    }
    printer.finish()?;
    println!("\n{}", aho_inspect::render_catch_up(&aho_inspect::catch_up(&state_of(&entries)?)));
    Ok(())
}

/// The late-results rule of the route the session used (replay must use the same rule).
fn late_policy(state: &State) -> LateResults {
    let provider = state.config.get("model.provider").and_then(|s| s.value.as_str()).unwrap_or("openai").to_string();
    let protocol = state.config.get(&format!("provider.{provider}.protocol")).and_then(|s| s.value.as_str()).map(str::to_string);
    match (provider.as_str(), protocol.as_deref()) {
        (_, Some("anthropic-messages")) | ("anthropic", None) => llm_anthropic_messages::LATE_RESULTS,
        (_, Some("openai-responses")) | ("scripted", None) => LateResults::SameCallId,
        _ => llm_openai_completions::LATE_RESULTS,
    }
}

pub async fn verify(ctx: &Ctx, id: Option<&str>, formal: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let s = find_session(&ctx.session_dir(&p), id)?;
    println!("session {} ({})", hex(s.id), s.path.display());
    let mut failures = 0;
    let mut check = |ok: bool, text: String| {
        println!("  {} {text}", if ok { "✓" } else { "✕" });
        if !ok {
            failures += 1;
        }
    };
    let (_, entries) = match branch(&s.path) {
        Ok(b) => b,
        Err(Fail::Error(e)) => {
            check(false, format!("the log cannot be read: {e}"));
            return Err(Fail::Plan("session verify failed".into()));
        }
        Err(e) => return Err(e),
    };
    let broken = entries.windows(2).find(|w| w[1].prev_hash != w[0].hash || w[1].parent != Some(w[0].id));
    check(
        broken.is_none(),
        match broken {
            None => format!("hash chain over {} entries", entries.len()),
            Some(w) => format!("hash chain breaks at entry {} (its parent/prev_hash does not name entry {})", w[1].id.0, w[0].id.0),
        },
    );
    let state = match state_of(&entries) {
        Ok(st) => {
            check(true, "fold: every log rule holds".into());
            st
        }
        Err(e) => {
            check(false, format!("fold: {e}"));
            return Err(Fail::Plan("session verify failed".into()));
        }
    };
    let customs = entries.iter().filter(|e| matches!(e.kind, EntryKind::Custom { .. })).count();
    if customs > 0 {
        println!("  · {customs} capability-owned entries are not re-folded offline (their folds load with the capability)");
    }
    match aho_session::verify_replay(&entries, &Default::default(), late_policy(&state)) {
        Ok(n) => check(true, format!("replay: {n} model request(s) re-derive their recorded hash")),
        Err(e) => check(false, format!("replay: {e}")),
    }
    let alive = state.meta.as_ref().and_then(|(_, _, pi)| pi.as_ref()).is_some_and(owner_alive);
    let orphaned: Vec<&CallId> = state.open_calls.iter().filter(|c| !state.open_ops.values().any(|(call, _, _)| call == *c)).collect();
    let dangling = state.turn.is_none() && !orphaned.is_empty();
    check(
        !dangling,
        if dangling {
            format!(
                "{} tool call(s) have no result and no running operation: {}",
                orphaned.len(),
                orphaned.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(", ")
            )
        } else {
            "every tool call has a result or a running operation".into()
        },
    );
    if !alive && !state.open_ops.is_empty() {
        println!(
            "  · {} operation(s) were running when the owner stopped; resuming closes them as failed or uncertain",
            state.open_ops.len()
        );
    }
    let mut checked = 0;
    for m in ctx.comp.manifests() {
        for inv in m.invariants {
            checked += 1;
            if let Err(e) = (inv.check)(&state) {
                check(false, format!("{} invariant `{}`: {e}", m.id.name, inv.name));
            }
        }
    }
    if checked > 0 {
        println!("  · {checked} capability invariant(s) checked");
    }
    match state.plan_hash {
        Some(h) if h == p.plan.hash => println!("  · sealed plan {} matches the current configuration", h.short()),
        Some(h) => println!(
            "  · sealed plan {}; the current configuration plans {} (`aho status` shows the drift)",
            h.short(),
            p.plan.hash.short()
        ),
        None => println!("  · no plan was sealed in this log"),
    }
    if formal {
        failures += crate::invariants::verify_formal(ctx, &entries).await?;
    }
    if failures > 0 { Err(Fail::Plan(format!("session verify found {failures} problem(s)"))) } else { Ok(()) }
}

/// `aho session branches`: one line per leaf of the log tree, the current head marked.
pub fn branches(ctx: &Ctx, id: Option<&str>) -> Result<(), Fail> {
    use aho_log::LogStore;
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let s = find_session(&ctx.session_dir(&p), id)?;
    let log = JsonlLog::open(&s.path)?;
    let head = log.head();
    let mut heads = log.heads();
    heads.sort();
    println!("session {}: {} branch(es)", hex(s.id), heads.len());
    for h in heads {
        let entries = log.read_branch(h)?;
        let prompt = entries.iter().rev().find_map(|e| match &e.kind {
            EntryKind::UserMessage { form: Form::Prompt, text, .. } => Some(surface_print::clip(text, 50)),
            _ => None,
        });
        let turns = entries.iter().filter(|e| matches!(e.kind, EntryKind::TurnStart { .. })).count();
        let mark = if Some(h) == head { "  ← head" } else { "" };
        println!("  entry {:<6} {:>3} entries  {turns} turn(s)  {}{mark}", h.0, entries.len(), prompt.unwrap_or_default());
    }
    Ok(())
}

// ───────────────────────────── checkpoints / rewind ─────────────────────────────

pub(crate) struct Mark {
    pub(crate) id: CheckpointId,
    pub(crate) tree: Hash,
    pub(crate) at: EntryId,
    pub(crate) turn: u64,
    pub(crate) prompt: String,
    cause: CheckpointCause,
}

pub(crate) fn marks(entries: &[Entry]) -> Vec<Mark> {
    let (mut turn, mut prompt) = (0, String::new());
    let mut out = Vec::new();
    for e in entries {
        match &e.kind {
            EntryKind::TurnStart { turn: t } => turn = t.0,
            EntryKind::UserMessage { form: Form::Prompt, text, .. } => prompt = text.clone(),
            EntryKind::WorkspaceCheckpoint { id, tree, cause, .. } => {
                out.push(Mark { id: *id, tree: *tree, at: e.id, turn, prompt: prompt.clone(), cause: cause.clone() })
            }
            _ => {}
        }
    }
    out
}

fn cause(c: &CheckpointCause) -> String {
    match c {
        CheckpointCause::ToolResult(call) => format!("after {}", call.0),
        CheckpointCause::TurnEnd => "turn end".into(),
        CheckpointCause::Manual => "manual".into(),
        CheckpointCause::PreRewind => "before a rewind".into(),
    }
}

pub fn checkpoints(ctx: &Ctx, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let s = find_session(&ctx.session_dir(&p), session)?;
    let (_, entries) = branch(&s.path)?;
    let ms = marks(&entries);
    if ms.is_empty() {
        println!("no checkpoints on session {} yet (they are taken after mutating tool calls and at turn end)", hex(s.id));
    }
    let n = ms.len();
    for (i, m) in ms.iter().enumerate() {
        let head = if i + 1 == n { "  ← head" } else { "" };
        println!(
            "c{:<3} turn {:<3} {:<40} {} · tree {}{head}",
            m.id.0,
            m.turn,
            surface_print::clip(&m.prompt, 38),
            cause(&m.cause),
            m.tree.short()
        );
    }
    Ok(())
}

pub(crate) fn parse_checkpoint(s: &str) -> Result<CheckpointId, Fail> {
    s.trim_start_matches('c')
        .parse::<u64>()
        .map(CheckpointId)
        .map_err(|_| err(format!("`{s}` is not a checkpoint (use `c2` or `2`; `aho checkpoints` lists them)")))
}

pub async fn rewind(ctx: &Ctx, target: &str, chat: bool, code: bool, session: Option<&str>) -> Result<(), Fail> {
    let to = parse_checkpoint(target)?;
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let s: SessionFile = find_session(&ctx.session_dir(&p), session)?;
    let (_, entries) = branch(&s.path)?;
    let ms = marks(&entries);
    let m = ms.iter().find(|m| m.id == to).ok_or_else(|| {
        let have: Vec<String> = ms.iter().map(|m| format!("c{}", m.id.0)).collect();
        err(format!(
            "session {} has no checkpoint c{}; it has: {}",
            hex(s.id),
            to.0,
            if have.is_empty() { "none".into() } else { have.join(" ") }
        ))
    })?;
    let mode = if chat {
        RewindMode::ChatOnly
    } else if code {
        RewindMode::CodeOnly
    } else {
        RewindMode::Both
    };
    println!("plan:");
    if mode != RewindMode::CodeOnly {
        println!("  fork the conversation at entry {} (turn {}: {})", m.at.0, m.turn, surface_print::clip(&m.prompt, 50));
    }
    if mode != RewindMode::ChatOnly {
        println!("  restore the workspace to tree {} (ignored paths untouched)", m.tree.short());
        println!("  a PreRewind checkpoint of the current disk is taken first, so this is reversible");
    }
    if mode == RewindMode::CodeOnly {
        println!("  the conversation stays on this branch; the model is told the files changed");
    }
    println!("  running operations of this session are stopped first");
    if !crate::ctx::confirm(ctx.global.yes)? {
        println!("nothing changed.");
        return Ok(());
    }
    let o = open(ctx, &p, &inputs.home, Some(&hex(s.id)), false, false).await?;
    let e = o.session.rewind(&operator(), to, mode).await?;
    let pre = o.session.log().get(e).and_then(|x| match x.kind {
        EntryKind::WorkspaceRestored { from, .. } => Some(from),
        _ => None,
    });
    match (pre, mode) {
        (Some(pre), RewindMode::CodeOnly) if pre != to => {
            println!("restored the workspace to c{} (entry {}); undo with `aho rewind c{} --code`", to.0, e.0, pre.0)
        }
        (Some(pre), _) if pre != to => println!(
            "rewound to c{} (entry {}); the previous branch is kept (`aho session branches`), and the disk before the rewind is its checkpoint c{}",
            to.0, e.0, pre.0
        ),
        _ => println!("rewound to c{} (entry {})", to.0, e.0),
    }
    Ok(())
}

// ───────────────────────────── shell / status / rpc ─────────────────────────────

pub async fn shell(ctx: &Ctx, session: Option<&str>, cmd: &[String]) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    if cmd.is_empty() && p.plan.world.name == "ssh" {
        // The remote root does not exist here: the argv cds there itself, so it runs from any cwd.
        let cfg = <world_ssh::SshConfig as aho_cap::Config>::from_loadout(&p.loadout).map_err(|e| err(format!("{e:?}")))?;
        let world = world_ssh::SshWorld::connect(cfg).map_err(|e| err(format!("{e:?}")))?;
        let argv = aho_sandbox::Sandbox::operator_shell(&*world.sandbox());
        let Some((prog, args)) = argv.split_first() else {
            return Err(err("the ssh world returned no shell command; nothing was started"));
        };
        println!("operator shell in {} on the ssh world (enforced by: {}); exit to return", world.root(), p.plan.world.enforced_by);
        let status = std::process::Command::new(prog).args(args).status().map_err(|e| err(format!("could not start {prog} ({e})")))?;
        return if status.success() { Ok(()) } else { Err(Fail::Reported) };
    }
    if cmd.is_empty() && p.plan.world.name == "namespace" {
        return crate::sandbox::namespace_shell(ctx, &p, session);
    }
    if cmd.is_empty() {
        let sh = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        let ws = ctx.workspace(&p);
        println!(
            "operator shell in {} (world: {}, enforced by: {}); exit to return",
            ws.display(),
            p.plan.world.name,
            p.plan.world.enforced_by
        );
        let status = std::process::Command::new(&sh).current_dir(&ws).status().map_err(|e| err(format!("could not start {sh} ({e})")))?;
        return if status.success() { Ok(()) } else { Err(Fail::Reported) };
    }
    let which = match session {
        Some(w) => Some(w.to_string()),
        None => sessions(&ctx.session_dir(&p))?.first().map(|s| hex(s.id)),
    };
    let o = open(ctx, &p, &inputs.home, which.as_deref(), false, false).await?;
    let out = o.session.operator_command(&operator(), &cmd.join(" ")).await?;
    print!("{out}");
    if !out.ends_with('\n') && !out.is_empty() {
        println!();
    }
    println!("(logged to session {})", hex(o.id));
    Ok(())
}

pub fn status(ctx: &Ctx, loud: bool, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let found = match session {
        Some(w) => Some(find_session(&ctx.session_dir(&p), Some(w))?),
        None => sessions(&ctx.session_dir(&p))?.into_iter().next(),
    };
    let (state, sess) = match &found {
        Some(s) => {
            let (_, entries) = branch(&s.path)?;
            let st = state_of(&entries)?;
            let alive = st.meta.as_ref().and_then(|(_, _, pi)| pi.as_ref()).is_some_and(owner_alive);
            let ss = aho_inspect::session_status(&st, alive);
            (st, Some((hex(s.id), ss)))
        }
        None => (State::default(), None),
    };
    let st = aho_inspect::status(&p.plan, &state, Timestamp::now());
    let nodes = surface_rpc::caps::nodes(&p);
    if json {
        let v = json!({
            "status": st,
            "session": sess.as_ref().map(|(id, (s, why))| json!({"id": id, "status": s, "reason": why})),
            "diagnostics": if loud { json!(p.loadout.diagnostics) } else { Value::Null },
            "not_active": if loud { json!(nodes.iter().filter(|n| !matches!(n.state, aho_log::CapState::Active)).collect::<Vec<_>>()) } else { Value::Null },
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    print!("{}", aho_inspect::render_status(&st));
    match &sess {
        Some((id, (s, why))) => println!("session {id}: {s:?} ({why})"),
        None => println!("no session yet; start one with `aho run`"),
    }
    if loud {
        if !p.loadout.diagnostics.is_empty() {
            println!("diagnostics:");
            for d in &p.loadout.diagnostics {
                println!("  {:?} {}: {}", d.severity, d.key, d.detail);
            }
        }
        let quiet: Vec<&aho_inspect::TreeNode> = nodes.iter().filter(|n| !matches!(n.state, aho_log::CapState::Active)).collect();
        if !quiet.is_empty() {
            println!("not active:");
            for n in quiet {
                let fix = n.action.as_deref().map(|f| format!("   fix: `{f}`")).unwrap_or_default();
                println!("  {} {:<26} {}{fix}", aho_inspect::glyph(&n.state), n.id, aho_inspect::state_name(&n.state));
            }
        }
        for n in nodes.iter().filter(|n| matches!(n.state, aho_log::CapState::Active)) {
            if let Some(o) = &n.over_preset {
                let fix = o.fix.as_deref().map(|f| format!("   fix: `{f}`")).unwrap_or_default();
                println!("  ! {:<26} {}{fix}", n.id, o.requirement);
            }
        }
    }
    Ok(())
}

/// `aho stats`: exact aggregates over the newest (or named) session's current branch.
/// Everything is computed here from the log; nothing leaves the machine.
pub fn stats_cmd(ctx: &Ctx, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let found = match session {
        Some(w) => Some(find_session(&ctx.session_dir(&p), Some(w))?),
        None => sessions(&ctx.session_dir(&p))?.into_iter().next(),
    };
    let Some(s) = found else {
        if json {
            println!("{}", serde_json::to_string_pretty(&json!({"stats": Value::Null, "session": Value::Null}))?);
        } else {
            println!("no session yet; start one with `aho run`");
        }
        return Ok(());
    };
    let (_, entries) = branch(&s.path)?;
    let stats = aho_inspect::stats(&entries);
    if json {
        println!("{}", serde_json::to_string_pretty(&json!({"stats": stats, "session": hex(s.id)}))?);
        return Ok(());
    }
    print!("{}", aho_inspect::render_stats(&stats));
    Ok(())
}

/// `aho telemetry preview`: the exact JSON one flush would send — the same fold, the same
/// closed vocabulary, the same envelope — so it can be read before anything is sent.
pub fn telemetry_preview(ctx: &Ctx, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let found = match session {
        Some(w) => Some(find_session(&ctx.session_dir(&p), Some(w))?),
        None => sessions(&ctx.session_dir(&p))?.into_iter().next(),
    };
    let Some(s) = found else {
        println!("no session yet; start one with `aho run`");
        return Ok(());
    };
    let (_, entries) = branch(&s.path)?;
    let mut events = aho_inspect::events(&entries);
    events.push(aho_inspect::session_snapshot(&entries));
    let is_ci = std::env::var("CI").is_ok_and(|v| !v.is_empty());
    let ran_from_cargo = std::env::var("CARGO").is_ok();
    let envelope = aho_inspect::envelope(Timestamp::now(), is_ci, ran_from_cargo);
    println!("{}", serde_json::to_string_pretty(&telemetry::payload(&envelope, &events))?);
    Ok(())
}

pub async fn rpc(ctx: &Ctx, socket: Option<&std::path::Path>, session: Option<&str>) -> Result<(), Fail> {
    // While `aho serve` runs it owns the project's sessions, and `aho rpc` speaks the same
    // protocol as the daemon's client. Settings for this invocation only need a plan of their
    // own, so those servers stay in this process (as `aho run` does).
    let g = &ctx.global;
    let own_plan = !g.sets.is_empty() || !g.disable.is_empty() || g.profile.is_some() || g.loadout.is_some();
    if !own_plan && let Some((daemon, conn)) = crate::serve::live(ctx).await {
        return match socket {
            None => {
                crate::rpc_bridge::bridge(conn, session.map(str::to_string), tokio::io::stdin(), tokio::io::stdout()).await.map_err(err)
            }
            Some(path) => {
                let path = ctx.typed_path(path);
                let served = tokio::select! {
                    r = crate::rpc_bridge::serve_unix(&daemon, conn, session, &path) => r,
                    _ = tokio::signal::ctrl_c() => Ok(()),
                };
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
                served
            }
        };
    }
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let o = open(ctx, &p, &inputs.home, session, true, false).await?;
    let caps = ctx.caps(inputs);
    let rctx = surface_rpc::RpcContext::new(&o.session, o.approvals, Some(caps));
    match socket {
        None => surface_rpc::serve_io(tokio::io::stdin(), tokio::io::stdout(), o.session, rctx).await.map_err(|e| err(e.to_string())),
        Some(path) => {
            let path = ctx.typed_path(path);
            eprintln!("serving session {} on {} (ctrl-c stops)", hex(o.id), path.display());
            let served = tokio::select! {
                r = surface_rpc::serve_unix(&path, o.session, rctx) => r.map_err(|e| err(e.to_string())),
                _ = tokio::signal::ctrl_c() => Ok(()),
            };
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            served
        }
    }
}
