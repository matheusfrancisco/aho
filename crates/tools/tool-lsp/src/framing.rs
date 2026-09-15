//! LSP base protocol: `Content-Length: N\r\n\r\n` + N bytes of JSON, over one byte stream.
//!
//! The decoder is fed whatever a read returned — half a header, three messages at once — and
//! hands back complete messages in order. A malformed header is reported and its header block
//! discarded, so the caller decides whether the stream is still worth reading.

use std::fmt;

use serde_json::Value;

/// Largest body accepted; a server announcing more is treated as broken, not buffered.
pub const MAX_BODY: usize = 64 << 20;
/// A header block (all header lines) longer than this without its blank line is malformed.
pub const MAX_HEADER: usize = 8 * 1024;
/// Consumed bytes kept at the front of the buffer before it is compacted.
const COMPACT_AT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// A header line that is not `Name: value`, a non-numeric length, or an unterminated block.
    BadHeader(String),
    /// A header block without `Content-Length`.
    MissingLength,
    TooLarge(usize),
    /// The body was framed correctly but is not JSON; the stream stays in sync.
    BadJson(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::BadHeader(h) => write!(f, "the language server sent a malformed message header ({h})"),
            FrameError::MissingLength => f.write_str("the language server sent a message header without Content-Length"),
            FrameError::TooLarge(n) => write!(f, "the language server announced a {n}-byte message, over the {MAX_BODY}-byte limit"),
            FrameError::BadJson(e) => write!(f, "the language server sent a message that is not JSON ({e})"),
        }
    }
}

impl std::error::Error for FrameError {}

/// One framed message, ready to write to the server's stdin.
pub fn encode(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Incremental decoder over the server's stdout.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Start of the unconsumed bytes in `buf`.
    pos: usize,
    /// Body length of the message whose header was already consumed.
    body: Option<usize>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= COMPACT_AT {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received but not yet returned as part of a message.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The next complete message; `Ok(None)` means more bytes are needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Value>, FrameError> {
        let len = match self.body {
            Some(n) => n,
            None => match self.header()? {
                Some(n) => n,
                None => return Ok(None),
            },
        };
        let rest = &self.buf[self.pos..];
        if rest.len() < len {
            return Ok(None);
        }
        let parsed = serde_json::from_slice::<Value>(&rest[..len]);
        self.pos += len;
        self.body = None;
        parsed.map(Some).map_err(|e| FrameError::BadJson(e.to_string()))
    }

    /// Parses one header block if it is complete; on success the block is consumed.
    fn header(&mut self) -> Result<Option<usize>, FrameError> {
        let rest = &self.buf[self.pos..];
        let Some(end) = find(rest, b"\r\n\r\n") else {
            if rest.len() > MAX_HEADER {
                // No blank line in sight: drop what we have so a later header can still be found.
                self.pos = self.buf.len();
                return Err(FrameError::BadHeader(format!("no end of header within {MAX_HEADER} bytes")));
            }
            return Ok(None);
        };
        let block = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += end + 4;
        let mut length = None;
        for line in block.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                return Err(FrameError::BadHeader(format!("`{}`", clip(line))));
            };
            if name.trim().eq_ignore_ascii_case("content-length") {
                let n = value.trim().parse::<usize>().map_err(|_| FrameError::BadHeader(format!("`{}`", clip(line))))?;
                length = Some(n);
            }
        }
        let n = length.ok_or(FrameError::MissingLength)?;
        if n > MAX_BODY {
            return Err(FrameError::TooLarge(n));
        }
        self.body = Some(n);
        Ok(Some(n))
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn clip(s: &str) -> String {
    s.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all(d: &mut Decoder) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(v) = d.next().unwrap() {
            out.push(v);
        }
        out
    }

    #[test]
    fn encode_then_decode_round_trips_with_a_byte_exact_length() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"text": "é ✓"}});
        let bytes = encode(&msg);
        let body_len = msg.to_string().len();
        assert!(bytes.starts_with(format!("Content-Length: {body_len}\r\n\r\n").as_bytes()));
        let mut d = Decoder::new();
        d.push(&bytes);
        assert_eq!(all(&mut d), vec![msg]);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn a_message_split_at_every_byte_boundary_decodes_once_complete() {
        let msg = json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"message": "ünïcode"}});
        let bytes = encode(&msg);
        for cut in 1..bytes.len() {
            let mut d = Decoder::new();
            d.push(&bytes[..cut]);
            assert_eq!(d.next().unwrap(), None, "cut at {cut}");
            d.push(&bytes[cut..]);
            assert_eq!(d.next().unwrap(), Some(msg.clone()), "cut at {cut}");
            assert_eq!(d.next().unwrap(), None);
        }
    }

    #[test]
    fn byte_by_byte_feeding_yields_every_message_in_order() {
        let msgs: Vec<Value> = (0..5).map(|i| json!({"id": i, "result": "x".repeat(i * 7)})).collect();
        let stream: Vec<u8> = msgs.iter().flat_map(encode).collect();
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for b in stream {
            d.push(&[b]);
            got.extend(all(&mut d));
        }
        assert_eq!(got, msgs);
    }

    #[test]
    fn several_messages_in_one_read_come_out_in_order_and_a_trailing_partial_waits() {
        let a = json!({"id": 1, "result": null});
        let b = json!({"id": 2, "result": [1, 2, 3]});
        let c = json!({"method": "$/progress", "params": {"token": "t"}});
        let mut chunk = encode(&a);
        chunk.extend(encode(&b));
        let tail = encode(&c);
        chunk.extend(&tail[..10]);
        let mut d = Decoder::new();
        d.push(&chunk);
        assert_eq!(all(&mut d), vec![a, b]);
        d.push(&tail[10..]);
        assert_eq!(all(&mut d), vec![c]);
    }

    #[test]
    fn header_names_are_case_insensitive_and_other_headers_are_ignored() {
        let body = br#"{"id":7}"#;
        let mut bytes =
            format!("content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH:  {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        let mut d = Decoder::new();
        d.push(&bytes);
        assert_eq!(all(&mut d), vec![json!({"id": 7})]);
    }

    #[test]
    fn a_header_line_without_a_colon_is_rejected_and_the_next_message_still_decodes() {
        let good = json!({"id": 3});
        let mut bytes = b"garbage line\r\n\r\n".to_vec();
        bytes.extend(encode(&good));
        let mut d = Decoder::new();
        d.push(&bytes);
        assert!(matches!(d.next(), Err(FrameError::BadHeader(h)) if h.contains("garbage line")));
        assert_eq!(all(&mut d), vec![good]);
    }

    #[test]
    fn a_non_numeric_or_missing_length_is_rejected() {
        let mut d = Decoder::new();
        d.push(b"Content-Length: twelve\r\n\r\n");
        assert!(matches!(d.next(), Err(FrameError::BadHeader(_))));
        let mut d = Decoder::new();
        d.push(b"Content-Type: text/plain\r\n\r\n{}");
        assert_eq!(d.next(), Err(FrameError::MissingLength));
    }

    #[test]
    fn an_oversized_body_is_refused_before_buffering_it() {
        let mut d = Decoder::new();
        d.push(format!("Content-Length: {}\r\n\r\n", MAX_BODY + 1).as_bytes());
        assert_eq!(d.next(), Err(FrameError::TooLarge(MAX_BODY + 1)));
    }

    #[test]
    fn a_header_that_never_ends_is_rejected_once_it_passes_the_limit() {
        let mut d = Decoder::new();
        d.push(&vec![b'x'; MAX_HEADER]);
        assert_eq!(d.next(), Ok(None));
        d.push(b"xx");
        assert!(matches!(d.next(), Err(FrameError::BadHeader(_))));
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn a_framed_body_that_is_not_json_is_reported_and_the_stream_stays_in_sync() {
        let mut bytes = b"Content-Length: 5\r\n\r\nnope!".to_vec();
        bytes.extend(encode(&json!({"id": 9})));
        let mut d = Decoder::new();
        d.push(&bytes);
        assert!(matches!(d.next(), Err(FrameError::BadJson(_))));
        assert_eq!(all(&mut d), vec![json!({"id": 9})]);
    }

    #[test]
    fn consumed_bytes_are_compacted_so_a_long_session_does_not_grow_the_buffer() {
        let msg = json!({"id": 1, "result": "y".repeat(1000)});
        let one = encode(&msg);
        let mut d = Decoder::new();
        for _ in 0..500 {
            // Leave one byte behind each round so the buffer is never simply cleared.
            d.push(&one);
            d.push(&one[..1]);
            assert_eq!(d.next().unwrap(), Some(msg.clone()));
            let rest = &one[1..];
            d.push(rest);
            assert_eq!(d.next().unwrap(), Some(msg.clone()));
        }
        assert!(d.buf.len() < COMPACT_AT + 4 * one.len(), "buffer grew to {}", d.buf.len());
    }
}
