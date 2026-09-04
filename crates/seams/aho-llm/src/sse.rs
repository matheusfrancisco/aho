//! Server-sent events framing (WHATWG `text/event-stream`), shared by every streaming adapter.
//! Pure: bytes in, events out; the adapter owns the transport.

/// One dispatched event. `event` is `None` when the stream sent no `event:` field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental decoder: feed arbitrary byte chunks (a UTF-8 sequence or a CRLF may straddle two).
#[derive(Debug, Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
    after_cr: bool,
}

impl SseDecoder {
    pub fn new() -> SseDecoder {
        SseDecoder::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in chunk {
            if std::mem::take(&mut self.after_cr) && b == b'\n' {
                continue;
            }
            match b {
                b'\n' => self.end_line(&mut out),
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut out);
                }
                _ => self.line.push(b),
            }
        }
        out
    }

    /// End of stream. A final event that was never terminated by a blank line is still
    /// dispatched: providers that close the connection right after the last `data:` line
    /// would otherwise lose it.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        self.end_line(&mut out);
        out.pop()
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) {
        let raw = std::mem::take(&mut self.line);
        if raw.is_empty() {
            let event = self.event.take();
            if let Some(data) = self.data.take() {
                out.push(SseEvent { event, data });
            }
            return;
        }
        let line = String::from_utf8_lossy(&raw);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = "event: message_start\ndata: {\"a\":1}\n\n: keep-alive comment\n\ndata: first\ndata: second\n\nevent: ping\ndata: {\"type\": \"ping\"}\n\n";

    fn all_at_once(bytes: &[u8]) -> Vec<SseEvent> {
        let mut d = SseDecoder::new();
        let mut out = d.push(bytes);
        out.extend(d.finish());
        out
    }

    #[test]
    fn decodes_named_events_multiline_data_and_skips_comments() {
        let events = all_at_once(STREAM.as_bytes());
        assert_eq!(
            events,
            vec![
                SseEvent { event: Some("message_start".into()), data: "{\"a\":1}".into() },
                SseEvent { event: None, data: "first\nsecond".into() },
                SseEvent { event: Some("ping".into()), data: "{\"type\": \"ping\"}".into() },
            ]
        );
    }

    #[test]
    fn byte_at_a_time_equals_whole_buffer_including_crlf_and_utf8() {
        let text = "data: héllo → wörld\r\n\r\nevent: x\rdata: y\r\r";
        let whole = all_at_once(text.as_bytes());
        assert_eq!(whole.len(), 2);
        assert_eq!(whole[0].data, "héllo → wörld");
        assert_eq!(whole[1], SseEvent { event: Some("x".into()), data: "y".into() });
        let mut d = SseDecoder::new();
        let mut split = Vec::new();
        for b in text.as_bytes() {
            split.extend(d.push(std::slice::from_ref(b)));
        }
        split.extend(d.finish());
        assert_eq!(split, whole);
    }

    #[test]
    fn unterminated_final_event_is_dispatched_at_finish() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: [DONE]").is_empty());
        assert_eq!(d.finish(), Some(SseEvent { event: None, data: "[DONE]".into() }));
        assert_eq!(d.finish(), None);
    }

    #[test]
    fn event_name_without_data_is_not_dispatched() {
        assert!(all_at_once(b"event: lonely\n\n").is_empty());
        assert_eq!(all_at_once(b"data\n\n"), vec![SseEvent { event: None, data: String::new() }]);
    }
}
