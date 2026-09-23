//! `12` "Process topology: one daemon, thin surfaces" from the CLI: `aho serve` owns every
//! session of the project; `aho attach` (and `aho run` when a daemon is serving) is a thin client
//! over the daemon's unix socket, rendering the same transcript `aho run` prints.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_llm::StreamEvent;
use aho_log::{ApprovalOutcome, Entry, GrantScope, JsonlLog, LogStore, Transition};
use aho_session::SessionEvent;
use aho_types::*;
use daemon::ready::ReadyFd;
use daemon::wire::{self, ClientFrame, ServerFrame};
use serde_json::json;
use surface_print::{Mode, Printer, Style};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};

use crate::approval::Answer;
use crate::cli::{AttachArgs, ServeArgs};
use crate::ctx::{Ctx, Fail, color, err, hex, stdin_tty};

// ───────────────────────────── aho serve ─────────────────────────────

pub async fn serve(ctx: &Ctx, ready: Option<ReadyFd>, a: &ServeArgs) -> Result<(), Fail> {
    let socket = ctx.socket(a.socket.as_deref());
    if a.detach {
        return detach(ctx, &socket, a);
    }
    let mut inputs = ctx.inputs(&[], false)?;
    // The readiness variable is the reload handshake, not a setting (AHO_* env vars are config keys).
    inputs.env.remove(daemon::ready::READY_FD_ENV);
    let planned = ctx.load(&inputs)?;
    let dir = ctx.session_dir(&planned);
    let host = Arc::new(crate::serve_host::AppHost::new(ctx.comp.clone(), ctx.project.clone(), inputs, dir.clone()));
    let opts = daemon::ServeOptions {
        socket: socket.clone(),
        idle_exit: a.idle_exit.map(Duration::from_secs),
        reload_grace: Duration::from_secs(5),
        scan_interval: Duration::from_secs(15),
    };
    eprintln!(
        "aho serve: sessions in {} on {} (plan {}); `aho attach` connects, ctrl-c stops",
        dir.display(),
        socket.display(),
        planned.plan.hash.short()
    );
    let served = daemon::serve(opts, host.clone(), ready);
    tokio::pin!(served);
    let exit = tokio::select! {
        r = &mut served => r,
        _ = tokio::signal::ctrl_c() => {
            // A clean stop: the daemon winds turns down and closes them in the logs. The daemon
            // future keeps being polled meanwhile — it is what accepts this shutdown request.
            let asked = shutdown(&socket);
            tokio::pin!(asked);
            tokio::select! {
                r = &mut served => r,
                sent = &mut asked => match sent {
                    Ok(()) => served.await,
                    Err(e) => {
                        eprintln!("aho: could not ask the daemon to stop ({e}); stopping without closing turns");
                        return Err(Fail::Reported);
                    }
                },
            }
        }
    }
    // The daemon closed every session on its way out (`SessionHost::closing` stopped their drivers).
    .map_err(err)?;
    eprintln!(
        "aho serve: stopped ({})",
        match exit {
            daemon::ServeExit::Idle => "idle: no clients and no running turns",
            daemon::ServeExit::Shutdown => "shut down on request",
        }
    );
    Ok(())
}

/// `aho serve --detach`: the same daemon as a background process; returns once it serves.
fn detach(ctx: &Ctx, socket: &Path, a: &ServeArgs) -> Result<(), Fail> {
    let exe = std::env::current_exe()
        .map_err(|e| err(format!("could not find the running aho binary ({e}); start `aho serve` in the foreground")))?;
    let log_path = ctx.project.join(".aho").join("daemon.log");
    std::fs::create_dir_all(ctx.project.join(".aho"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| err(format!("could not open the daemon log {} ({e}); nothing was started", log_path.display())))?;
    let mut cmd = std::process::Command::new(exe);
    for (k, v) in &ctx.global.sets {
        cmd.arg("--set").arg(format!("{k}={v}"));
    }
    for d in &ctx.global.disable {
        cmd.arg("--disable").arg(d);
    }
    if let Some(p) = &ctx.global.profile {
        cmd.arg("--profile").arg(p);
    }
    if let Some(l) = &ctx.global.loadout {
        cmd.arg("--loadout").arg(l);
    }
    cmd.arg("serve").arg("--socket").arg(socket);
    if let Some(s) = a.idle_exit {
        cmd.arg("--idle-exit").arg(s.to_string());
    }
    cmd.current_dir(&ctx.project).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = daemon::ready::spawn_with_ready(&mut cmd, Duration::from_secs(30))
        .map_err(|e| err(format!("the daemon did not become ready ({e:?}); see {}", log_path.display())))?;
    println!("daemon pid {} serving on {} (log: {}); stop it with `aho serve --stop`", child.id(), socket.display(), log_path.display());
    Ok(())
}

async fn shutdown(socket: &Path) -> Result<(), String> {
    let c = Conn::connect(socket).await?;
    c.request("daemon.shutdown", json!({})).await.map(|_| ())
}

/// `aho serve --stop`.
pub async fn stop(ctx: &Ctx, socket: Option<&Path>) -> Result<(), Fail> {
    let socket = ctx.socket(socket);
    shutdown(&socket).await.map_err(err)?;
    println!("the daemon on {} is shutting down (running turns wind down and are closed in their logs)", socket.display());
    Ok(())
}

// ───────────────────────────── the client ─────────────────────────────

type Pending = Arc<Mutex<BTreeMap<u64, oneshot::Sender<ServerFrame>>>>;

/// Request/reply over the daemon socket with pushed events on a separate queue, so a turn's
/// events can be read while another request (abort, an approval answer) is in flight.
pub struct Conn {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    next: AtomicU64,
    pending: Pending,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<ServerFrame>>,
}

impl Conn {
    pub async fn connect(socket: &Path) -> Result<Conn, String> {
        let stream = UnixStream::connect(socket).await.map_err(|e| {
            format!(
                "no daemon answers on {} ({e}). Start one with `aho serve` (or `aho serve --detach`), or pass --socket",
                socket.display()
            )
        })?;
        let (rd, wr) = stream.into_split();
        let pending: Pending = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let p = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(rd);
            loop {
                match wire::read_frame::<_, ServerFrame>(&mut reader).await {
                    Ok(Some(f)) => {
                        let waiter = f.reply_to.and_then(|id| p.lock().unwrap_or_else(|x| x.into_inner()).remove(&id));
                        // A requester or event reader that stopped listening has nothing left to tell.
                        match waiter {
                            Some(w) => drop(w.send(f)),
                            None => drop(tx.send(f)),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                    Ok(None) | Err(_) => break,
                }
            }
            p.lock().unwrap_or_else(|x| x.into_inner()).clear();
        });
        let c = Conn { writer: tokio::sync::Mutex::new(wr), next: AtomicU64::new(1), pending, events: tokio::sync::Mutex::new(rx) };
        c.request("hello", json!({"min": wire::PROTOCOL_VERSION, "max": wire::PROTOCOL_VERSION, "client_instance": client_instance()}))
            .await?;
        Ok(c)
    }

    /// The reply to `method`; an `error` reply is `Err` with the daemon's message.
    pub async fn request(&self, method: &str, params: Value) -> Result<ServerFrame, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
        {
            let mut w = self.writer.lock().await;
            wire::write_frame(&mut *w, &ClientFrame::new(id, method, params))
                .await
                .map_err(|e| format!("could not send `{method}` to the daemon ({e}); the connection is gone"))?;
        }
        let f = rx.await.map_err(|_| format!("the daemon closed the connection before answering `{method}`"))?;
        if f.is_error() { Err(f.message().unwrap_or("the daemon refused the request").to_string()) } else { Ok(f) }
    }

    pub async fn next_event(&self) -> Option<ServerFrame> {
        self.events.lock().await.recv().await
    }

    /// A pushed frame that has already arrived, without waiting (`None` when there is none, or
    /// while another task waits in [`Conn::next_event`]).
    pub fn try_event(&self) -> Option<ServerFrame> {
        self.events.try_lock().ok()?.try_recv().ok()
    }
}

fn client_instance() -> String {
    format!("aho-cli-{}", std::process::id())
}

/// `Some(conn)` when a daemon answers on the project socket.
pub async fn live(ctx: &Ctx) -> Option<(PathBuf, Conn)> {
    let socket = ctx.socket(None);
    if !socket.exists() {
        return None;
    }
    Conn::connect(&socket).await.ok().map(|c| (socket, c))
}

#[derive(Debug)]
struct Row {
    id: SessionId,
    status: String,
}

async fn list(c: &Conn) -> Result<Vec<Row>, String> {
    let f = c.request("session.list", json!({})).await?;
    let rows = f.payload.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id: SessionId = serde_json::from_value(r.get("session")?.clone()).ok()?;
            Some(Row { id, status: r.get("status").and_then(Value::as_str).unwrap_or("closed").to_string() })
        })
        .collect())
}

/// The daemon's socket, a connection to it, and the session's status there (`idle` or
/// `running`) when the daemon holds session `id` open.
pub async fn holding(ctx: &Ctx, dir: &Path, id: SessionId) -> Result<Option<(PathBuf, Conn, String)>, Fail> {
    if !dir.join(format!("{}.jsonl", hex(id))).exists() {
        return Ok(None);
    }
    let Some((socket, c)) = live(ctx).await else { return Ok(None) };
    let rows = list(&c).await.map_err(err)?;
    let status = rows.into_iter().find(|r| r.id == id && matches!(r.status.as_str(), "idle" | "running")).map(|r| r.status);
    Ok(status.map(|s| (socket, c, s)))
}

/// Two processes never drive one log: a session the daemon holds is reached through it.
pub async fn refuse_if_daemon_owns(ctx: &Ctx, dir: &Path, id: SessionId) -> Result<(), Fail> {
    if let Some((socket, _, _)) = holding(ctx, dir, id).await? {
        return Err(err(format!(
            "session {} is open in the daemon on {}, so this process did not open it too (two processes never drive one log). Use `aho attach {}`, or stop the daemon with `aho serve --stop`",
            hex(id),
            socket.display(),
            short(id)
        )));
    }
    Ok(())
}

fn short(id: SessionId) -> String {
    let h = hex(id);
    h[..12.min(h.len())].to_string()
}

// ───────────────────────────── aho attach ─────────────────────────────

fn stream_event(v: &Value) -> Option<StreamEvent> {
    let text = || v.get("text").and_then(Value::as_str).map(str::to_string);
    let id = || v.get("id").and_then(Value::as_str).map(|s| CallId(s.to_string()));
    let name = || v.get("name").and_then(Value::as_str).map(str::to_string);
    match v.get("kind")?.as_str()? {
        "text" => text().map(StreamEvent::Text),
        "thinking" => text().map(StreamEvent::Thinking),
        "tool_use_start" => Some(StreamEvent::ToolUseStart { id: id()?, name: name()? }),
        "tool_use_end" => Some(StreamEvent::ToolUseEnd { id: id()?, name: name()?, args: v.get("args").cloned().unwrap_or(Value::Null) }),
        _ => None,
    }
}

const CARD_W: usize = 74;

/// The approval card from the daemon's `approval.request` payload (log data only, 05).
fn render_card(p: &Value) -> String {
    let inner = CARD_W - 4;
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let class = match p.get("class") {
        Some(Value::String(c)) => c.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let tool = s("tool");
    let head = format!("class: {class}");
    let gap = inner.saturating_sub(tool.chars().count() + head.chars().count()).max(2);
    let mut rows = vec![format!("{tool}{}{head}", " ".repeat(gap))];
    rows.push(format!("args: {}", surface_print::clip(&p.get("args").map(Value::to_string).unwrap_or_default(), inner - 6)));
    for (i, l) in s("rendered").lines().enumerate() {
        rows.push(if i == 0 { format!("why:  {l}") } else { format!("      {l}") });
    }
    if let Some(cut) = p.get("suggested_cut").and_then(Value::as_str) {
        rows.push(format!("fix:  {cut} — a lasting change; this answer covers one call"));
    }
    rows.push(String::new());
    rows.push("[y] allow once   [s] allow for session   [r] reject   [Esc] cancel".into());
    let mut out = format!("┌ approval {}┐\n", "─".repeat(CARD_W - 4 - "approval".len()));
    for r in rows {
        let text: String = r.chars().take(inner).collect();
        let pad = inner - text.chars().count();
        out.push_str(&format!("│ {text}{} │\n", " ".repeat(pad)));
    }
    out.push_str(&format!("└{}┘\n", "─".repeat(CARD_W - 2)));
    out
}

type Out = Printer<std::io::Stdout, std::io::Stderr>;

struct Client {
    c: Conn,
    sid: SessionId,
    json: bool,
    printer: Out,
}

enum TurnEnd {
    Idle(Transition),
    /// Another client took the session over, or the daemon went away.
    Lost(String),
}

impl Client {
    fn frame(&mut self, f: &ServerFrame) -> Result<(), Fail> {
        if self.json {
            println!("{}", serde_json::to_string(f)?);
            return Ok(());
        }
        let ev = match f.event.as_str() {
            "entry" => match f.payload.get("entry").map(|e| serde_json::from_value::<Entry>(e.clone())) {
                Some(Ok(e)) => SessionEvent::Entry(Box::new(e)),
                Some(Err(e)) => SessionEvent::Error(format!("an entry from the daemon did not decode ({e}); `aho session show` has it")),
                None => SessionEvent::Error(f.message().unwrap_or("the daemon sent an entry it could not encode").to_string()),
            },
            "stream" => match f.payload.get("event").and_then(stream_event) {
                Some(s) => SessionEvent::Stream(s),
                None => return Ok(()),
            },
            "error" => SessionEvent::Error(f.message().unwrap_or("the daemon reported an error").to_string()),
            _ => return Ok(()),
        };
        self.printer.event(&ev)?;
        Ok(())
    }

    fn for_me(&self, f: &ServerFrame) -> bool {
        f.payload.get("session").and_then(|s| serde_json::from_value::<SessionId>(s.clone()).ok()).is_none_or(|s| s == self.sid)
    }

    /// Renders events until the session's next `idle`; answers approval cards inline.
    async fn until_idle(&mut self) -> Result<TurnEnd, Fail> {
        loop {
            let f = tokio::select! {
                f = self.c.next_event() => f,
                _ = tokio::signal::ctrl_c() => {
                    if let Err(e) = self.c.request("session.abort", json!({"session": self.sid})).await {
                        self.printer.event(&SessionEvent::Error(format!("the abort was not delivered ({e})")))?;
                    }
                    continue;
                }
            };
            let Some(f) = f else { return Ok(TurnEnd::Lost("the daemon closed the connection".into())) };
            if !self.for_me(&f) {
                continue;
            }
            match f.event.as_str() {
                "idle" => {
                    let transition: Transition = serde_json::from_value(f.payload.get("transition").cloned().unwrap_or(Value::Null))
                        .unwrap_or(Transition::Completed);
                    if self.json {
                        println!("{}", serde_json::to_string(&f)?);
                    } else {
                        let turn = TurnId(f.payload.get("turn").and_then(Value::as_u64).unwrap_or(0));
                        self.printer.event(&SessionEvent::Idle { turn, transition: transition.clone() })?;
                        self.printer.finish()?;
                    }
                    return Ok(TurnEnd::Idle(transition));
                }
                "owner.lost" => {
                    let by = f.payload.get("by").and_then(Value::as_str).unwrap_or("another client").to_string();
                    return Ok(TurnEnd::Lost(format!("`{by}` took session {} over; this client stopped driving it", hex(self.sid))));
                }
                "approval.request" => self.answer(&f.payload).await?,
                _ => self.frame(&f)?,
            }
        }
    }

    async fn answer(&mut self, p: &Value) -> Result<(), Fail> {
        let Some(id) = p.get("id").and_then(Value::as_u64) else { return Ok(()) };
        if self.json || !stdin_tty() {
            // No one at a terminal: the card is rejected, never silently allowed (04).
            if self.json {
                println!("{}", serde_json::to_string(&json!({"v": 1, "event": "approval.request", "payload": p}))?);
            }
            return self.reply(id, false, None).await;
        }
        self.printer.finish()?;
        print!("{}", render_card(p));
        std::io::stdout().flush()?;
        let answer = tokio::task::spawn_blocking(|| crate::approval::read_answer(false))
            .await
            .unwrap_or(Answer::Decided(aho_approval::Decided::cancelled()));
        let (allow, scope) = match answer {
            Answer::Decided(d) => match (d.outcome, d.scope) {
                (ApprovalOutcome::AllowedOnce, GrantScope::Once) => (true, Some("once")),
                (ApprovalOutcome::AllowedOnce, _) => (true, Some("session")),
                _ => (false, None),
            },
            Answer::Preset => (false, None),
        };
        self.reply(id, allow, scope).await
    }

    async fn reply(&mut self, id: u64, allow: bool, scope: Option<&str>) -> Result<(), Fail> {
        if let Err(e) = self.c.request("approval.answer", json!({"id": id, "allow": allow, "scope": scope})).await {
            self.printer.event(&SessionEvent::Error(format!("the answer was not delivered ({e})")))?;
        }
        Ok(())
    }

    async fn prompt(&mut self, text: String) -> Result<TurnEnd, Fail> {
        self.c.request("session.prompt", json!({"session": self.sid, "text": text})).await.map_err(err)?;
        self.until_idle().await
    }
}

fn read_line_blocking() -> Option<String> {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s),
    }
}

/// Opens (or resumes a crashed) session in the daemon; `which` is a hex id or prefix, `None` = new.
pub(crate) async fn open_in(c: &Conn, which: Option<&str>) -> Result<(SessionId, bool), Fail> {
    let Some(prefix) = which else {
        let f = c.request("session.open", json!({})).await.map_err(err)?;
        let sid: SessionId = serde_json::from_value(f.payload.get("session").cloned().unwrap_or(Value::Null))
            .map_err(|e| err(format!("the daemon answered session.open without an id ({e})")))?;
        return Ok((sid, false));
    };
    let rows = list(c).await.map_err(err)?;
    let hits: Vec<&Row> = rows.iter().filter(|r| hex(r.id).starts_with(prefix)).collect();
    let row = match hits.as_slice() {
        [one] => *one,
        [] => return Err(err(format!("the daemon has no session `{prefix}`; `aho session list` shows them"))),
        many => return Err(err(format!("`{prefix}` matches {} sessions; give more of the id", many.len()))),
    };
    if row.status == "crashed" {
        c.request("session.resume", json!({"session": row.id})).await.map_err(err)?;
        eprintln!("session {} had crashed; it continues from its last completed turn", hex(row.id));
    } else {
        c.request("session.open", json!({"id": row.id})).await.map_err(err)?;
    }
    Ok((row.id, true))
}

pub async fn attach(ctx: &Ctx, a: &AttachArgs) -> Result<(), Fail> {
    let socket = ctx.socket(a.socket.as_deref());
    let c = Conn::connect(&socket).await.map_err(err)?;
    attach_with(ctx, &socket, c, a).await
}

pub async fn attach_with(ctx: &Ctx, socket: &Path, c: Conn, a: &AttachArgs) -> Result<(), Fail> {
    let (sid, resumed) = open_in(&c, a.session.as_deref()).await?;
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let path = ctx.session_dir(&p).join(format!("{}.jsonl", hex(sid)));
    // Text mode shows the catch-up brief and then only what is new; `--json` replays everything.
    let mut from: Option<EntryId> = None;
    if resumed && !a.json {
        let log = JsonlLog::open(&path)?;
        if let Some(h) = log.head() {
            let entries = log.read_branch(h)?;
            println!("{}", aho_inspect::render_catch_up(&aho_inspect::catch_up(&aho_log::fold(&entries, &Default::default())?)));
            from = Some(h);
        }
    }
    let attached = c
        .request("session.attach", json!({"session": sid, "client_instance": client_instance(), "has_local_history": false, "from": from}))
        .await
        .map_err(err)?;
    let owner = attached.payload.get("owner").and_then(Value::as_bool).unwrap_or(false);
    let mode = if a.json { Mode::Json } else { Mode::Text };
    let printer =
        Printer::new(mode, Style { color: !a.json && color(), hud: a.hud, echo_user: false }, std::io::stdout(), std::io::stderr())
            .with_manifests(&ctx.comp.manifests());
    let mut cl = Client { c, sid, json: a.json, printer };
    if !owner {
        let why = attached.payload.get("reason").map(Value::to_string).unwrap_or_default();
        if a.prompt.is_some() {
            return Err(err(format!("another client drives session {} ({why}), so the prompt was not sent. Detach it first", hex(sid))));
        }
        eprintln!("watching session {} read-only: another client drives it ({why}); ctrl-c leaves", hex(sid));
        loop {
            tokio::select! {
                f = cl.c.next_event() => match f {
                    Some(f) if cl.for_me(&f) => cl.frame(&f)?,
                    Some(_) => {}
                    None => break,
                },
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        cl.printer.finish()?;
        return Ok(());
    }

    let mut last = Transition::Completed;
    let mut ran = 0;
    let mut lost = None;
    let repl = a.prompt.is_none() && stdin_tty() && !a.json;
    let mut prompts: Vec<String> = Vec::new();
    if let Some(p) = &a.prompt {
        prompts.push(p.clone());
    } else if !repl {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        if !text.trim().is_empty() {
            prompts.push(text);
        }
    }
    for text in prompts {
        match cl.prompt(text).await? {
            TurnEnd::Idle(t) => last = t,
            TurnEnd::Lost(why) => lost = Some(why),
        }
        ran += 1;
    }
    if repl {
        loop {
            print!("› ");
            std::io::stdout().flush()?;
            let Some(line) = tokio::task::spawn_blocking(read_line_blocking).await.map_err(|e| err(e.to_string()))? else { break };
            if line.trim().is_empty() {
                continue;
            }
            ran += 1;
            match cl.prompt(line.trim_end().to_string()).await? {
                TurnEnd::Idle(t) => last = t,
                TurnEnd::Lost(why) => {
                    lost = Some(why);
                    break;
                }
            }
        }
    }
    if lost.is_none()
        && let Err(e) = cl.c.request("session.detach", json!({"session": sid})).await
    {
        eprintln!("aho: could not detach cleanly ({e}); the daemon releases the session when this connection closes");
    }
    if !a.json {
        let paint = |s: String| if color() { format!("\x1b[2m{s}\x1b[0m") } else { s };
        println!("{}", paint(format!("session {} · daemon {}  (resume: aho attach {})", hex(sid), socket.display(), short(sid))));
    }
    if let Some(why) = lost {
        return Err(err(why));
    }
    match last {
        _ if ran == 0 && !repl => Err(err("no prompt was given; pass -p <prompt> or pipe one on stdin")),
        Transition::Aborted(_) | Transition::PromptTooLong | Transition::MaxTurns => Err(Fail::Reported),
        _ => Ok(()),
    }
}
