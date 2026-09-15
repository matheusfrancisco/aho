//! Splits a program's merged output into its own text and the helper's tool-call frames
//! (see `prelude` for the wire format). Text before, between and after frames is kept in
//! order; a frame never appears in the output shown to the model.

use std::collections::BTreeMap;

use aho_types::Value;

use crate::prelude::MARKER;

/// A marker without its line end after this many bytes is not a frame (a piece is < 512 bytes).
const MAX_PIECE_LINE: usize = 4096;

#[derive(Debug, PartialEq)]
pub enum Event {
    Text(Vec<u8>),
    Request {
        id: u64,
        tool: String,
        args: Value,
    },
    /// A frame that could not be read; the program is waiting for an answer all the same.
    Bad(String),
}

pub struct Frames {
    marker: Vec<u8>,
    buf: Vec<u8>,
    pieces: BTreeMap<u64, String>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Frames {
    pub fn new(nonce: &str) -> Frames {
        Frames { marker: format!("{MARKER}{nonce}:").into_bytes(), buf: Vec::new(), pieces: BTreeMap::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(pos) = find(&self.buf, &self.marker) else {
                // Hold back a tail that could be the start of a marker split across reads.
                let keep =
                    (1..self.marker.len().min(self.buf.len() + 1)).rev().find(|k| self.buf.ends_with(&self.marker[..*k])).unwrap_or(0);
                let text: Vec<u8> = self.buf.drain(..self.buf.len() - keep).collect();
                if !text.is_empty() {
                    out.push(Event::Text(text));
                }
                return out;
            };
            if pos > 0 {
                out.push(Event::Text(self.buf.drain(..pos).collect()));
            }
            let body_at = self.marker.len();
            let Some(nl) = self.buf[body_at..].iter().position(|b| *b == b'\n') else {
                if self.buf.len() - body_at > MAX_PIECE_LINE {
                    out.push(Event::Bad("a tool-call frame had no line end; it was shown as output".into()));
                    out.push(Event::Text(std::mem::take(&mut self.buf)));
                }
                return out;
            };
            let line: Vec<u8> = self.buf.drain(..body_at + nl + 1).skip(body_at).collect();
            if let Some(ev) = self.piece(&line[..line.len() - 1]) {
                out.push(ev);
            }
        }
    }

    /// End of output: whatever is left is text (a torn frame included).
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            out.push(Event::Text(std::mem::take(&mut self.buf)));
        }
        out
    }

    /// `<id>:<more>:<chunk>`; the last piece of an id completes its request.
    fn piece(&mut self, line: &[u8]) -> Option<Event> {
        let text = String::from_utf8_lossy(line);
        let mut parts = text.splitn(3, ':');
        let (Some(id), Some(more), Some(chunk)) = (parts.next(), parts.next(), parts.next()) else {
            return Some(Event::Bad("a tool-call frame was malformed".into()));
        };
        let Ok(id) = id.parse::<u64>() else { return Some(Event::Bad(format!("a tool-call frame had a bad id `{id}`"))) };
        self.pieces.entry(id).or_default().push_str(chunk);
        if more == "1" {
            return None;
        }
        let body = self.pieces.remove(&id).unwrap_or_default();
        let v: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return Some(Event::Bad(format!("tool call {id} was not valid JSON ({e})"))),
        };
        let Some(tool) = v.get("tool").and_then(Value::as_str) else {
            return Some(Event::Bad(format!("tool call {id} named no tool")));
        };
        let args = match v.get("args") {
            None | Some(Value::Null) => Value::Object(Default::default()),
            Some(a) => a.clone(),
        };
        Some(Event::Request { id, tool: tool.to_string(), args })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(nonce: &str, id: u64, more: u8, chunk: &str) -> String {
        format!("{MARKER}{nonce}:{id}:{more}:{chunk}\n")
    }

    #[test]
    fn text_around_frames_is_kept_in_order_and_pieces_reassemble() {
        let mut f = Frames::new("n1");
        let body = json!({"id": 1, "tool": "read", "args": {"path": "a.txt"}}).to_string();
        let (a, b) = body.split_at(10);
        let stream = format!("before{}{}after\n", frame("n1", 1, 1, a), frame("n1", 1, 0, b));
        let mut events = Vec::new();
        // byte-at-a-time: markers and lines split across reads
        for byte in stream.as_bytes() {
            events.extend(f.feed(&[*byte]));
        }
        events.extend(f.finish());
        let text: Vec<u8> = events.iter().filter_map(|e| if let Event::Text(t) = e { Some(t.clone()) } else { None }).flatten().collect();
        assert_eq!(String::from_utf8(text).unwrap(), "beforeafter\n");
        let reqs: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::Request { .. })).collect();
        assert_eq!(reqs, vec![&Event::Request { id: 1, tool: "read".into(), args: json!({"path": "a.txt"}) }]);
    }

    #[test]
    fn another_nonce_is_plain_output_and_bad_frames_are_reported() {
        let mut f = Frames::new("real");
        let forged = frame("other", 1, 0, r#"{"id":1,"tool":"bash"}"#);
        let ev = f.feed(forged.as_bytes());
        assert_eq!(ev, vec![Event::Text(forged.into_bytes())]);
        let ev = f.feed(frame("real", 2, 0, "{not json").as_bytes());
        assert!(matches!(&ev[..], [Event::Bad(m)] if m.starts_with("tool call 2 was not valid JSON")), "{ev:?}");
        let ev = f.feed(frame("real", 3, 0, r#"{"id":3}"#).as_bytes());
        assert_eq!(ev, vec![Event::Bad("tool call 3 named no tool".into())]);
    }
}
