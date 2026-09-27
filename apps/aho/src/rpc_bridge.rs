//! `aho rpc` while `aho serve` runs ("one daemon, thin surfaces"): this process speaks the rpc
//! surface's protocol (the one the TS and Python SDKs speak) on its stdio (or on `--socket`),
//! and the daemon that owns the session does the work. Clients see the same frames as from an
//! in-process `aho rpc`:
//!
//! * `hello` → the daemon opens the session and decides the takeover (a refusal ends the
//!   connection, as in-process); `hello_ok{version, capabilities, session, head}`.
//! * `session.prompt` → `session.prompt{wait}` (the reply is the turn's end); `steer`, `abort` 1:1;
//!   `session.subscribe{from}` → `session.subscribe` (the replay first, then the reply).
//! * `session.fold`, `session.rewind`, `caps.*` → the daemon's `session.call`; approval cards
//!   arrive as `approval` events and `approval.answer` (including `preset`) goes back through it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use aho_types::*;
use daemon::wire::ServerFrame as DaemonFrame;
use serde_json::json;
use surface_rpc::wire::{self, AnswerParams, ClientFrame, Hello, HelloOk, ServerFrame};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::ctx::{Fail, err, hex};
use crate::serve::{Conn, open_in};

type Cards = Arc<Mutex<BTreeMap<u64, Value>>>;

fn cards_of(c: &Cards) -> MutexGuard<'_, BTreeMap<u64, Value>> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, f: &ServerFrame) -> Result<(), String> {
    let mut line = serde_json::to_string(f).map_err(|e| format!("a frame could not be encoded ({e})"))?;
    line.push('\n');
    w.write_all(line.as_bytes()).await.map_err(|e| format!("the client went away ({e})"))?;
    w.flush().await.map_err(|e| format!("the client went away ({e})"))
}

/// Answers the hello with an error, as the in-process server does, and ends the connection.
async fn refuse<W: AsyncWrite + Unpin>(w: &mut W, id: Option<u64>, why: String) -> Result<(), String> {
    send(w, &ServerFrame::error(wire::VERSION_MAX, id, why.clone())).await?;
    Err(why)
}

/// Serves one client over `reader`/`writer`, bridged to the daemon on `conn`. `which` names the
/// session (hex id or prefix); `None` opens a new one. Returns when the client closes its side and
/// every request it made has been answered.
pub async fn bridge<R, W>(conn: Conn, which: Option<String>, reader: R, mut writer: W) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let first = loop {
        match lines.next_line().await.map_err(|e| format!("the client's input could not be read ({e})"))? {
            None => return Ok(()),
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
        }
    };
    let (id, hello) = match serde_json::from_str::<ClientFrame>(&first) {
        Ok(f) if f.method == "hello" => match serde_json::from_value::<Hello>(f.params) {
            Ok(h) => (f.id, h),
            Err(e) => return refuse(&mut writer, f.id, format!("bad hello params: {e}")).await,
        },
        Ok(f) => return refuse(&mut writer, f.id, format!("the first frame must be `hello`, got `{}`", f.method)).await,
        Err(e) => return refuse(&mut writer, None, format!("the first line is not a frame: {e}")).await,
    };
    let v = match wire::negotiate(&hello) {
        Ok(v) => v,
        Err(m) => return refuse(&mut writer, id, m).await,
    };
    let sid = match open_in(&conn, which.as_deref()).await {
        Ok((sid, _)) => sid,
        Err(e) => return refuse(&mut writer, id, e.to_string()).await,
    };
    let attached = conn
        .request(
            "session.attach",
            json!({ "session": sid, "client_instance": hello.client_instance, "has_local_history": hello.has_local_history, "watch": false }),
        )
        .await;
    let attached = match attached {
        Ok(f) => f,
        Err(e) => return refuse(&mut writer, id, e).await,
    };
    if attached.payload["takeover"] != json!("allowed") {
        let why = attached.payload["reason"].as_str().unwrap_or("another client drives this session").to_string();
        return refuse(&mut writer, id, format!("rpc attach refused: {why}")).await;
    }
    let ok = HelloOk {
        version: v,
        capabilities: wire::METHODS.iter().map(|m| m.to_string()).collect(),
        session: hex(sid),
        head: serde_json::from_value(attached.payload["head"].clone()).unwrap_or(None),
    };
    let ok = serde_json::to_value(&ok).map_err(|e| format!("hello_ok could not be encoded ({e})"))?;
    send(&mut writer, &ServerFrame::new(v, id, "hello_ok", ok)).await?;

    let conn = Arc::new(conn);
    let cards: Cards = Arc::default();
    let mut tasks: JoinSet<Option<ServerFrame>> = JoinSet::new();
    let mut reading = true;
    let outcome = loop {
        if !reading && tasks.is_empty() {
            break Ok(());
        }
        tokio::select! {
            biased;
            ev = conn.next_event() => match ev {
                Some(f) => if let Some(out) = event(v, &f, &cards) { send(&mut writer, &out).await? },
                None => {
                    let gone = "the daemon closed the connection (it stopped or is reloading), so this session is no longer served here. Reconnect";
                    send(&mut writer, &ServerFrame::error(v, None, gone)).await?;
                    break Err(gone.to_string());
                }
            },
            Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                // Whatever the daemon sent before this reply (a subscribe's replay, a turn's
                // entries) goes out first, as the in-process server orders it.
                while let Some(f) = conn.try_event() {
                    if let Some(out) = event(v, &f, &cards) {
                        send(&mut writer, &out).await?;
                    }
                }
                match done {
                    Ok(Some(f)) => send(&mut writer, &f).await?,
                    Ok(None) => {}
                    Err(e) => send(&mut writer, &ServerFrame::error(v, None, format!("a request failed inside the bridge: {e}"))).await?,
                }
            },
            line = lines.next_line(), if reading => match line {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => match serde_json::from_str::<ClientFrame>(&l) {
                    Ok(f) if f.v != v => {
                        let msg = format!("this frame says v{} but the connection negotiated v{v}", f.v);
                        send(&mut writer, &ServerFrame::error(v, f.id, msg)).await?;
                    }
                    Ok(f) => {
                        let (conn, cards) = (conn.clone(), cards.clone());
                        tasks.spawn(async move {
                            let r = call(&conn, sid, &cards, &f.method, f.params).await;
                            match (f.id, r) {
                                (None, _) => None,
                                (id, Ok(p)) => Some(ServerFrame::result(v, id, p)),
                                (id, Err(m)) => Some(ServerFrame::error(v, id, m)),
                            }
                        });
                    }
                    Err(e) => send(&mut writer, &ServerFrame::error(v, None, format!("not a frame ({e}); send one JSON object per line"))).await?,
                },
                Ok(None) | Err(_) => reading = false,
            },
        }
    };
    while let Some(f) = conn.try_event() {
        if let Some(out) = event(v, &f, &cards) {
            send(&mut writer, &out).await?;
        }
    }
    if outcome.is_ok() {
        conn.request("session.detach", json!({ "session": sid })).await?;
    }
    outcome
}

/// One client request, answered through the daemon.
async fn call(conn: &Conn, sid: SessionId, cards: &Cards, method: &str, params: Value) -> Result<Value, String> {
    let params = if params.is_null() { json!({}) } else { params };
    let host = |m: &str, p: Value| json!({ "session": sid, "method": m, "params": p });
    match method {
        "session.prompt" => {
            let p: wire::PromptParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
            let f =
                conn.request("session.prompt", json!({ "session": sid, "text": p.text, "client_key": p.client_key, "wait": true })).await?;
            Ok(json!({ "transition": f.payload["transition"] }))
        }
        "session.steer" => {
            let p: wire::SteerParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
            conn.request("session.steer", json!({ "session": sid, "text": p.text, "urgent": p.urgent })).await.map(|_| json!({}))
        }
        "session.abort" => conn.request("session.abort", json!({ "session": sid })).await.map(|_| json!({})),
        "session.subscribe" => {
            let p: wire::SubscribeParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
            let f = conn.request("session.subscribe", json!({ "session": sid, "from": p.from })).await?;
            Ok(json!({ "replayed": f.payload["replayed"], "head": f.payload["head"] }))
        }
        "session.fold" | "session.rewind" => conn.request("session.call", host(method, params)).await.map(|f| f.payload),
        "approval.answer" => {
            let p: AnswerParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
            if p.outcome == "preset" {
                return answer_with_preset(conn, sid, cards, p.id).await;
            }
            wire::decided(&p.outcome, p.scope.as_deref())?;
            let (outcome, scope) = match p.outcome.as_str() {
                "allow" | "allowed_once" | "allow_once" => ("allow", p.scope),
                "allow_session" => ("allow", Some("session".to_string())),
                "reject" | "rejected" => ("reject", None),
                _ => ("cancel", None),
            };
            conn.request("approval.answer", json!({ "id": p.id, "outcome": outcome, "scope": scope })).await?;
            cards_of(cards).remove(&p.id);
            Ok(json!({}))
        }
        "hello" => Err("hello was already negotiated on this connection".into()),
        m if m.starts_with("caps.") => conn.request("session.call", host(m, params)).await.map(|f| f.payload),
        other => Err(format!("unknown method `{other}`; hello_ok.capabilities lists what this server speaks")),
    }
}

/// `[p]` on the card: the card's `preset_cut` becomes `policy.preset` through the same plan →
/// apply as `aho preset`, and the call is allowed for the session.
async fn answer_with_preset(conn: &Conn, _sid: SessionId, cards: &Cards, id: u64) -> Result<Value, String> {
    if !cards_of(cards).contains_key(&id) {
        return Err(format!("no approval is waiting with id {id}; it was answered or never asked"));
    }
    let wrote = conn.request("approval.answer", json!({ "id": id, "outcome": "preset" })).await?;
    cards_of(cards).remove(&id);
    Ok(wrote.payload)
}

/// A daemon event as the rpc surface sends it; `None` for what it has no frame for.
fn event(v: u16, f: &DaemonFrame, cards: &Cards) -> Option<ServerFrame> {
    if f.reply_to.is_some() {
        return None;
    }
    let p = &f.payload;
    Some(match f.event.as_str() {
        "entry" => ServerFrame::new(v, None, "entry", p["entry"].clone()),
        "stream" => ServerFrame::new(v, None, "stream", p["event"].clone()),
        "idle" => ServerFrame::new(v, None, "idle", json!({ "turn": p["turn"], "transition": p["transition"] })),
        "error" => ServerFrame::error(v, None, p["message"].as_str().unwrap_or("the daemon reported an error without a message")),
        "approval.request" => {
            let id = p["id"].as_u64()?;
            let card = json!({
                "id": id,
                "tool": p["tool"],
                "cap": p["cap"],
                "class": p["class"],
                "args": p["args"],
                "reason": p["reason"],
                "why": p["why"],
                "world": p["world"],
                "enforced_by": p["enforced_by"],
                "suggested_cut": p["suggested_cut"],
                "preset_cut": p["preset_cut"],
                "epoch_next": p["epoch_next"],
            });
            cards_of(cards).insert(id, card.clone());
            ServerFrame::new(v, None, "approval", card)
        }
        "owner.lost" => ServerFrame::error(
            v,
            None,
            format!(
                "client `{}` took this session over, so this connection no longer drives it. Reconnect with local history to take it back",
                p["by"].as_str().unwrap_or("another client")
            ),
        ),
        _ => return None,
    })
}

/// `aho rpc --socket PATH` through the daemon: one bridge per connection to `path` (mode `0600`),
/// each on its own daemon connection, all on session `which` (opened once here).
pub async fn serve_unix(socket: &Path, conn: Conn, which: Option<&str>, path: &Path) -> Result<(), Fail> {
    use std::os::unix::fs::PermissionsExt;
    let (sid, _) = open_in(&conn, which).await?;
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(err(format!("another server is listening on {}; stop it or pick another --socket", path.display())));
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("serving session {} on {} through the daemon on {} (ctrl-c stops)", hex(sid), path.display(), socket.display());
    drop(conn);
    loop {
        let (stream, _) = listener.accept().await?;
        let socket = socket.to_path_buf();
        tokio::spawn(async move {
            let (r, w) = stream.into_split();
            let served = match Conn::connect(&socket).await {
                Ok(c) => bridge(c, Some(hex(sid)), r, w).await,
                Err(e) => Err(e),
            };
            if let Err(e) = served {
                eprintln!("aho rpc: {e}");
            }
        });
    }
}
