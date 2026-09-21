//! `GET /api/mux[?from=<entry id>]`: one WebSocket per tab. On connect it replays every entry
//! after `from` (`subscribe_from`), sends `ready`, then the pending approval cards, then streams
//! `entry`, `stream`, `idle`, `error`, `approval` and `approval_resolved` frames. A lagging
//! subscriber is caught up from the log, never from memory, so no entry is skipped or repeated.

use aho_session::SessionEvent;
use aho_types::EntryId;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::server::App;
use crate::views::{self, frame};

#[derive(Deserialize)]
pub struct MuxQuery {
    from: Option<u64>,
}

pub async fn mux(ws: WebSocketUpgrade, Query(q): Query<MuxQuery>, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| run(socket, app, q.from.map(EntryId)))
}

async fn send(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// Sends every entry after `last`; returns the new last id, or `None` if the socket closed.
async fn catch_up(socket: &mut WebSocket, app: &App, last: Option<EntryId>) -> Option<Option<EntryId>> {
    let mut last = last;
    for e in app.session.subscribe_from(last) {
        if last.is_some_and(|l| e.id <= l) {
            continue;
        }
        last = Some(e.id);
        if !send(socket, frame("entry", views::entry_json(&e))).await {
            return None;
        }
    }
    Some(last)
}

async fn run(mut socket: WebSocket, app: App, from: Option<EntryId>) {
    // Subscribe before replaying so nothing appended during the replay is missed.
    let mut live = app.session.subscribe();
    let mut web = app.web_events.subscribe();
    let Some(mut last) = catch_up(&mut socket, &app, from).await else { return };
    let ready = json!({ "head": app.session.log().head(), "last": last, "protocol": views::PROTOCOL_VERSION });
    if !send(&mut socket, frame("ready", ready)).await {
        return;
    }
    for f in app.pending_frames() {
        if !send(&mut socket, f).await {
            return;
        }
    }
    loop {
        tokio::select! {
            ev = live.recv() => {
                let text = match ev {
                    Ok(SessionEvent::Entry(e)) => {
                        if last.is_some_and(|l| e.id <= l) {
                            continue;
                        }
                        last = Some(e.id);
                        frame("entry", views::entry_json(&e))
                    }
                    Ok(SessionEvent::Stream(s)) => frame("stream", views::stream_json(&s)),
                    Ok(SessionEvent::Idle { turn, transition }) => frame("idle", json!({ "turn": turn, "transition": transition })),
                    Ok(SessionEvent::Error(message)) => frame("error", json!({ "message": message })),
                    Err(RecvError::Lagged(_)) => match catch_up(&mut socket, &app, last).await {
                        Some(l) => {
                            last = l;
                            continue;
                        }
                        None => return,
                    },
                    Err(RecvError::Closed) => return,
                };
                if !send(&mut socket, text).await {
                    return;
                }
            }
            ev = web.recv() => match ev {
                Ok(text) => {
                    if !send(&mut socket, text).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    for f in app.pending_frames() {
                        if !send(&mut socket, f).await {
                            return;
                        }
                    }
                }
                Err(RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
