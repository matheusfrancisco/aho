//! A thin client for the daemon socket: request/reply over [`wire`] frames, pushed events kept
//! in order, and the reload handshake.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aho_types::SessionId;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::ready::READY_BYTE;
use crate::wire::{self, ClientFrame, PROTOCOL_VERSION, ServerFrame};

pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    /// Pushed frames read while waiting for a reply.
    events: VecDeque<ServerFrame>,
}

#[allow(clippy::disallowed_methods, reason = "authority: daemon-client-connect")]
pub async fn connect(socket: &Path) -> Result<Client, String> {
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        format!("could not connect to the daemon at {} ({e}). Start it with `aho serve`, or check the socket path", socket.display())
    })?;
    let (reader, writer) = stream.into_split();
    Ok(Client { reader: BufReader::new(reader), writer, next_id: 0, events: VecDeque::new() })
}

impl Client {
    /// Sends one request and returns the frame that replies to it (`reply_to` = its id), which
    /// may be an `error` frame. Pushed events that arrive first are kept for [`Client::next_event`].
    pub async fn request(&mut self, method: &str, params: Value) -> Result<ServerFrame, String> {
        self.next_id += 1;
        let id = self.next_id;
        wire::write_frame(&mut self.writer, &ClientFrame::new(id, method, params))
            .await
            .map_err(|e| format!("could not send `{method}` to the daemon ({e}); the connection is gone. Reconnect and retry"))?;
        loop {
            let frame = self
                .read()
                .await?
                .ok_or_else(|| format!("the daemon closed the connection before answering `{method}`. Reconnect and retry"))?;
            if frame.reply_to == Some(id) {
                return Ok(frame);
            }
            self.events.push_back(frame);
        }
    }

    /// The version handshake; `Err` carries the daemon's reason when it refuses.
    pub async fn hello(&mut self, client_instance: &str) -> Result<ServerFrame, String> {
        let f =
            self.request("hello", json!({ "min": PROTOCOL_VERSION, "max": PROTOCOL_VERSION, "client_instance": client_instance })).await?;
        if f.event == "hello_ok" { Ok(f) } else { Err(f.message().unwrap_or("the daemon refused the handshake").to_string()) }
    }

    /// The next frame that is not a reply to a request: buffered ones first. `None` at end of
    /// stream.
    pub async fn next_event(&mut self) -> Result<Option<ServerFrame>, String> {
        if let Some(f) = self.events.pop_front() {
            return Ok(Some(f));
        }
        self.read().await
    }

    async fn read(&mut self) -> Result<Option<ServerFrame>, String> {
        wire::read_frame(&mut self.reader).await.map_err(|e| {
            format!("the daemon sent something that is not a frame ({e}). Reconnect, and upgrade the older side if it keeps happening")
        })
    }

    /// After `reload.started`: raw bytes until [`READY_BYTE`]. A `reload.failed` line means the
    /// exec failed and the old image kept serving. `READY_BYTE` is a control character, which
    /// JSON always escapes, so it never appears inside a frame.
    async fn await_ready(&mut self) -> Result<(), String> {
        let mut line = Vec::new();
        loop {
            match self.reader.read_u8().await {
                Ok(READY_BYTE) => return Ok(()),
                Ok(b'\n') => {
                    if let Ok(f) = serde_json::from_slice::<ServerFrame>(&line)
                        && f.event == "reload.failed"
                    {
                        return Err(f.message().unwrap_or("the reload failed and the old binary kept serving").to_string());
                    }
                    line.clear();
                }
                Ok(b) => line.push(b),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(
                        "the new binary exited before it was ready; the old sessions are closed and will resume when a daemon starts"
                            .to_string(),
                    );
                }
                Err(e) => {
                    return Err(format!(
                        "lost the connection while waiting for the new binary ({e}); the old sessions are closed and will resume when a daemon starts"
                    ));
                }
            }
        }
    }
}

/// Asks the daemon at `socket` to reload into `binary` (default: the binary it runs) on behalf of
/// `session`, and waits until the new image signals readiness on this connection.
pub async fn reload_and_wait(socket: &Path, binary: Option<PathBuf>, session: Option<SessionId>, timeout: Duration) -> Result<(), String> {
    let binary = match binary {
        Some(b) => Some(
            b.to_str()
                .ok_or_else(|| {
                    format!(
                        "the binary path {} is not valid UTF-8, so it cannot be sent to the daemon. Move the binary to a UTF-8 path",
                        b.display()
                    )
                })?
                .to_string(),
        ),
        None => None,
    };
    let run = async {
        let mut c = connect(socket).await?;
        c.hello(&format!("reload-{}", std::process::id())).await?;
        let r = c.request("daemon.reload", json!({ "binary": binary, "session": session })).await?;
        if r.event != "reload.started" {
            return Err(r
                .message()
                .map(str::to_string)
                .unwrap_or_else(|| format!("the daemon answered `{}` instead of starting the reload", r.event)));
        }
        c.await_ready().await
    };
    tokio::time::timeout(timeout, run).await.map_err(|_| {
        format!(
            "the reload did not finish within {:.1} s, so we stopped waiting. The new daemon may still come up: check with `session.list`, or reload again",
            timeout.as_secs_f64()
        )
    })?
}
