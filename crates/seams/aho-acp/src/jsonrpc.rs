//! JSON-RPC 2.0 as ACP uses it, plus the stdio framing: one UTF-8 JSON message per line,
//! delimited by `\n`, never containing an embedded newline. Batches are not part of ACP and are
//! refused as invalid requests.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const JSONRPC: &str = "2.0";

/// Largest frame accepted; a longer line is discarded and answered with a parse error.
pub const MAX_FRAME: usize = 16 << 20;

// ───────────────────────────── ids & errors ─────────────────────────────

/// `RequestId`: a number, a string, or `null`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Str(String),
    Null,
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::Str(s) => write!(f, "{s:?}"),
            RequestId::Null => write!(f, "null"),
        }
    }
}

/// The error codes the ACP schema names (`ErrorCode`).
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const REQUEST_CANCELLED: i64 = -32800;
    pub const AUTH_REQUIRED: i64 = -32000;
    pub const RESOURCE_NOT_FOUND: i64 = -32002;
}

/// `Error`: `{code, message, data?}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into(), data: None }
    }
    pub fn parse_error(detail: impl std::fmt::Display) -> RpcError {
        RpcError::new(code::PARSE_ERROR, format!("the message is not valid JSON ({detail}); send one JSON-RPC object per line"))
    }
    pub fn invalid_request(detail: impl Into<String>) -> RpcError {
        RpcError::new(code::INVALID_REQUEST, detail)
    }
    pub fn method_not_found(method: &str) -> RpcError {
        RpcError::new(code::METHOD_NOT_FOUND, format!("method `{method}` is not supported; the initialize response lists what is"))
            .with_data(json!({ "method": method }))
    }
    pub fn invalid_params(detail: impl Into<String>) -> RpcError {
        RpcError::new(code::INVALID_PARAMS, detail)
    }
    pub fn internal(detail: impl Into<String>) -> RpcError {
        RpcError::new(code::INTERNAL_ERROR, detail)
    }
    pub fn request_cancelled() -> RpcError {
        RpcError::new(code::REQUEST_CANCELLED, "the request was cancelled by `$/cancel_request` before it finished")
    }
    pub fn resource_not_found(detail: impl Into<String>) -> RpcError {
        RpcError::new(code::RESOURCE_NOT_FOUND, detail)
    }
    /// Local error for an outgoing request whose connection closed before the answer came.
    pub fn closed() -> RpcError {
        RpcError::internal("the ACP connection closed before the peer answered")
    }
    pub fn with_data(mut self, data: Value) -> RpcError {
        self.data = Some(data);
        self
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (JSON-RPC {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

// ───────────────────────────── messages ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Request { id: RequestId, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: RequestId, result: Result<Value, RpcError> },
}

/// Why an incoming frame was not a message, and whether (and to which id) to answer it.
/// Something shaped like a response is never answered, so two peers cannot ping-pong errors.
#[derive(Clone, Debug, PartialEq)]
pub struct Rejected {
    pub reply_to: Option<RequestId>,
    pub error: RpcError,
}

impl Message {
    pub fn request(id: RequestId, method: impl Into<String>, params: Value) -> Message {
        Message::Request { id, method: method.into(), params }
    }
    pub fn notification(method: impl Into<String>, params: Value) -> Message {
        Message::Notification { method: method.into(), params }
    }
    pub fn response(id: RequestId, result: Result<Value, RpcError>) -> Message {
        Message::Response { id, result }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("jsonrpc".into(), Value::String(JSONRPC.into()));
        match self {
            Message::Request { id, method, params } => {
                m.insert("id".into(), id_value(id));
                m.insert("method".into(), Value::String(method.clone()));
                if !params.is_null() {
                    m.insert("params".into(), params.clone());
                }
            }
            Message::Notification { method, params } => {
                m.insert("method".into(), Value::String(method.clone()));
                if !params.is_null() {
                    m.insert("params".into(), params.clone());
                }
            }
            Message::Response { id, result } => {
                m.insert("id".into(), id_value(id));
                match result {
                    Ok(v) => m.insert("result".into(), v.clone()),
                    Err(e) => m.insert("error".into(), serde_json::to_value(e).unwrap_or(Value::Null)),
                };
            }
        }
        Value::Object(m)
    }

    /// One frame without the trailing newline. JSON escapes newlines inside strings, so the
    /// result never contains `\n`.
    pub fn encode(&self) -> String {
        self.to_value().to_string()
    }

    /// Classifies a parsed JSON value (JSON-RPC 2.0 §4–5).
    pub fn from_value(v: Value) -> Result<Message, Rejected> {
        let invalid = |reply_to: Option<RequestId>, why: &str| Rejected {
            reply_to,
            error: RpcError::invalid_request(format!("{why}; every ACP message is one JSON-RPC 2.0 object per line")),
        };
        let mut obj = match v {
            Value::Object(o) => o,
            Value::Array(_) => return Err(invalid(Some(RequestId::Null), "batches are not part of ACP")),
            _ => return Err(invalid(Some(RequestId::Null), "the message is not a JSON object")),
        };
        let id = match obj.remove("id") {
            None => None,
            Some(raw) => match parse_id(&raw) {
                Some(id) => Some(id),
                None => return Err(invalid(Some(RequestId::Null), "`id` must be an integer, a string, or null")),
            },
        };
        let is_response = !obj.contains_key("method") && (obj.contains_key("result") || obj.contains_key("error"));
        if obj.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC) {
            let reply = if is_response { None } else { Some(id.clone().unwrap_or(RequestId::Null)) };
            return Err(invalid(reply, "`jsonrpc` must be \"2.0\""));
        }
        if let Some(method) = obj.remove("method") {
            let Value::String(method) = method else {
                return Err(invalid(Some(id.unwrap_or(RequestId::Null)), "`method` must be a string"));
            };
            let params = obj.remove("params").unwrap_or(Value::Null);
            if !matches!(params, Value::Null | Value::Object(_) | Value::Array(_)) {
                return Err(invalid(Some(id.unwrap_or(RequestId::Null)), "`params` must be an object or an array"));
            }
            return Ok(match id {
                Some(id) => Message::Request { id, method, params },
                None => Message::Notification { method, params },
            });
        }
        if is_response {
            let Some(id) = id else { return Err(invalid(None, "a response must carry the `id` it answers")) };
            let result = match (obj.remove("result"), obj.remove("error")) {
                (Some(r), None) => Ok(r),
                (None, Some(e)) => match serde_json::from_value::<RpcError>(e) {
                    Ok(e) => Err(e),
                    Err(_) => return Err(invalid(None, "`error` must be an object with an integer `code` and a string `message`")),
                },
                _ => return Err(invalid(None, "a response carries exactly one of `result` and `error`")),
            };
            return Ok(Message::Response { id, result });
        }
        Err(invalid(Some(id.unwrap_or(RequestId::Null)), "the object has neither `method` nor `result`/`error`"))
    }
}

fn id_value(id: &RequestId) -> Value {
    serde_json::to_value(id).unwrap_or(Value::Null)
}

fn parse_id(v: &Value) -> Option<RequestId> {
    match v {
        Value::Null => Some(RequestId::Null),
        Value::String(s) => Some(RequestId::Str(s.clone())),
        Value::Number(n) => n.as_i64().map(RequestId::Number),
        _ => None,
    }
}

/// Decodes one frame's text.
pub fn decode(line: &str) -> Result<Message, Rejected> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => Message::from_value(v),
        Err(e) => Err(Rejected { reply_to: Some(RequestId::Null), error: RpcError::parse_error(e) }),
    }
}

/// `params` as a typed request body; a mismatch is `Invalid params` naming the method.
pub fn params<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { Value::Object(Map::new()) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(format!("`{method}` params do not match the ACP v1 schema: {e}")))
}

/// A typed value as a JSON-RPC `result`.
pub fn result<T: Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(format!("the result did not serialize: {e}")))
}

// ───────────────────────────── framing ─────────────────────────────

/// One line from the peer.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Line(String),
    /// The line exceeded [`MAX_FRAME`] bytes and was discarded up to its newline.
    TooLong(usize),
    /// The line was not UTF-8.
    NotUtf8,
}

/// Splits a byte stream into frames without ever buffering more than [`MAX_FRAME`] bytes.
/// Blank lines are skipped; a trailing `\r` is dropped.
pub struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> FrameReader<R> {
        FrameReader::with_limit(inner, MAX_FRAME)
    }

    pub fn with_limit(inner: R, max: usize) -> FrameReader<R> {
        FrameReader { inner, buf: Vec::new(), max }
    }

    /// The next frame, or `None` at end of stream.
    pub async fn next(&mut self) -> std::io::Result<Option<Frame>> {
        loop {
            let mut dropped = 0usize;
            let ended = loop {
                let avail = self.inner.fill_buf().await?;
                if avail.is_empty() {
                    break true;
                }
                let (chunk, used, done) = match avail.iter().position(|b| *b == b'\n') {
                    Some(i) => (&avail[..i], i + 1, true),
                    None => (avail, avail.len(), false),
                };
                if dropped > 0 || self.buf.len() + chunk.len() > self.max {
                    dropped += self.buf.len() + chunk.len();
                    self.buf.clear();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
                self.inner.consume(used);
                if done {
                    break false;
                }
            };
            if dropped > 0 {
                return Ok(Some(Frame::TooLong(dropped)));
            }
            if ended && self.buf.is_empty() {
                return Ok(None);
            }
            let mut line = std::mem::take(&mut self.buf);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                if ended {
                    return Ok(None);
                }
                continue;
            }
            return Ok(Some(match String::from_utf8(line) {
                Ok(s) => Frame::Line(s),
                Err(_) => Frame::NotUtf8,
            }));
        }
    }
}

impl Frame {
    /// The frame as a message, or what to answer.
    pub fn decode(self) -> Result<Message, Rejected> {
        match self {
            Frame::Line(l) => decode(&l),
            Frame::TooLong(n) => Err(Rejected {
                reply_to: Some(RequestId::Null),
                error: RpcError::parse_error(format!("the line is {n} bytes, over the {MAX_FRAME}-byte limit; it was discarded")),
            }),
            Frame::NotUtf8 => Err(Rejected { reply_to: Some(RequestId::Null), error: RpcError::parse_error("the line is not UTF-8") }),
        }
    }
}

/// Writes one frame and its newline, then flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &str) -> std::io::Result<()> {
    w.write_all(frame.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}
