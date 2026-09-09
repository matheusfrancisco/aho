//! `08` / `12` "Wire envelope": one JSON object per line. Every frame carries `v`; unknown fields
//! are skipped; `hello{min, max, client_instance}` → `hello_ok{version, capabilities}`, or an
//! `error` frame when the version ranges do not overlap. Frame and field names follow the
//! `surface-rpc` envelope (`ClientFrame{v, id, method, params}`,
//! `ServerFrame{v, reply_to, event, payload}`).

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 1;

/// Every method the daemon answers, advertised in `hello_ok.capabilities`.
pub const METHODS: &[&str] = &[
    "hello",
    "session.list",
    "session.open",
    "session.attach",
    "session.subscribe",
    "session.detach",
    "session.prompt",
    "session.steer",
    "session.abort",
    "session.resume",
    "session.mutation",
    "session.command",
    "session.call",
    "session.reopen",
    "approval.answer",
    "daemon.reload",
    "daemon.shutdown",
];

/// Client → daemon. A missing `params` reads as `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    pub v: u16,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl ClientFrame {
    pub fn new(id: u64, method: &str, params: Value) -> ClientFrame {
        ClientFrame { v: PROTOCOL_VERSION, id, method: method.to_string(), params }
    }
}

/// Daemon → client. `reply_to` names the request a reply answers; pushed events carry `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame {
    pub v: u16,
    #[serde(default)]
    pub reply_to: Option<u64>,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

impl ServerFrame {
    pub fn reply(id: u64, event: &str, payload: Value) -> ServerFrame {
        ServerFrame { v: PROTOCOL_VERSION, reply_to: Some(id), event: event.to_string(), payload }
    }
    pub fn event(event: &str, payload: Value) -> ServerFrame {
        ServerFrame { v: PROTOCOL_VERSION, reply_to: None, event: event.to_string(), payload }
    }
    /// `error{message}`.
    pub fn error(reply_to: Option<u64>, message: impl Into<String>) -> ServerFrame {
        ServerFrame { v: PROTOCOL_VERSION, reply_to, event: "error".to_string(), payload: json!({ "message": message.into() }) }
    }
    pub fn is_error(&self) -> bool {
        self.event == "error"
    }
    /// `payload.message`, or `payload.reason`, when present.
    pub fn message(&self) -> Option<&str> {
        self.payload.get("message").or_else(|| self.payload.get("reason")).and_then(Value::as_str)
    }
}

/// `hello` params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub min: u16,
    pub max: u16,
    #[serde(default)]
    pub client_instance: String,
}

/// The highest version both sides speak.
pub fn negotiate(min: u16, max: u16) -> Result<u16, String> {
    if min > max {
        return Err(format!(
            "hello asked for versions {min}..={max}, an empty range; send the lowest and highest version the client speaks"
        ));
    }
    let (lo, hi) = (min.max(PROTOCOL_VERSION), max.min(PROTOCOL_VERSION));
    if lo > hi {
        return Err(format!(
            "no common protocol version: the client speaks {min}..={max}, this daemon speaks {PROTOCOL_VERSION}..={PROTOCOL_VERSION}, so we did not open the connection. Upgrade the older side"
        ));
    }
    Ok(hi)
}

/// `hello_ok` payload.
pub fn hello_ok(version: u16) -> Value {
    json!({ "version": version, "capabilities": METHODS })
}

/// Reads the next frame. `Ok(None)` at end of stream; blank lines are skipped; a line that is not
/// valid UTF-8 or not the expected frame is an `InvalidData` error (the line is consumed, so the
/// caller may continue with the next one).
pub async fn read_frame<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        return serde_json::from_str(text).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
    }
}

/// Writes one frame as one line and flushes.
pub async fn write_frame<W, T>(w: &mut W, frame: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut bytes = serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await
}
