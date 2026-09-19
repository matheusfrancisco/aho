//! `12` visual debug: every drawn frame becomes a [`Frame`] (layout rects, text per area, render
//! order, timing, anomalies). The [`FrameCapture`] ring buffer drops a frame identical to the
//! previous one unless it is anomalous, and flags `layout_oscillation` when consecutive frames
//! alternate between two screens of different layout (flicker). Frames stay in memory; they never
//! enter the log's hash chain.

use std::collections::VecDeque;

use aho_types::Hash;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};

/// Collects what one draw pass laid out. Render code calls [`Probe::area`] for each region it
/// fills and [`Probe::check`] for runtime assertions.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub rects: Vec<(String, Rect)>,
    pub anomalies: Vec<String>,
}

impl Probe {
    pub fn area(&mut self, name: &str, rect: Rect) {
        self.rects.push((name.to_string(), rect));
    }

    /// A render-time assertion: a false `cond` records `msg` as an anomaly of this frame; the
    /// frame is then kept even if it looks identical to the previous one.
    pub fn check(&mut self, cond: bool, msg: impl Into<String>) {
        if !cond {
            self.anomalies.push(msg.into());
        }
    }
}

/// One captured frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub seq: u64,
    pub width: u16,
    pub height: u16,
    /// `(area, x, y, w, h)` in render order.
    pub rects: Vec<(String, u16, u16, u16, u16)>,
    /// `(area, text)`: the rendered cells of each area, one line per row, trailing blanks trimmed.
    pub text: Vec<(String, String)>,
    /// Render time.
    pub ms: u64,
    /// When the frame was drawn (ms since the epoch).
    pub at: u64,
    pub anomalies: Vec<String>,
    /// Digest of every cell symbol and the rects; equal digests mean an identical frame.
    pub digest: Hash,
    /// Digest of the rects alone (layout shape), for oscillation detection.
    pub layout: Hash,
}

impl Frame {
    pub fn from_buffer(buf: &Buffer, probe: Probe, ms: u64) -> Frame {
        let screen = buffer_lines(buf, buf.area);
        let rects: Vec<(String, u16, u16, u16, u16)> = probe.rects.iter().map(|(n, r)| (n.clone(), r.x, r.y, r.width, r.height)).collect();
        let text = probe.rects.iter().map(|(n, r)| (n.clone(), buffer_lines(buf, *r).join("\n"))).collect();
        let layout_json = serde_json::to_vec(&rects).unwrap_or_default();
        let mut all = layout_json.clone();
        all.extend_from_slice(screen.join("\n").as_bytes());
        Frame {
            seq: 0,
            width: buf.area.width,
            height: buf.area.height,
            rects,
            text,
            ms,
            at: aho_types::Timestamp::now().0,
            anomalies: probe.anomalies,
            digest: Hash::of(&all),
            layout: Hash::of(&layout_json),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "width": self.width,
            "height": self.height,
            "ms": self.ms,
            "at": self.at,
            "rects": self.rects.iter().map(|(n, x, y, w, h)| json!({"area": n, "x": x, "y": y, "w": w, "h": h})).collect::<Vec<_>>(),
            "text": self.text.iter().map(|(n, t)| json!({"area": n, "text": t})).collect::<Vec<_>>(),
            "anomalies": self.anomalies,
        })
    }
}

/// The rows of `area` as text, each trimmed of trailing blanks.
pub fn buffer_lines(buf: &Buffer, area: Rect) -> Vec<String> {
    let area = area.intersection(buf.area);
    let mut out = Vec::with_capacity(area.height as usize);
    for y in area.top()..area.bottom() {
        let mut line = String::new();
        let mut skip = 0usize;
        for x in area.left()..area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            skip = unicode_width::UnicodeWidthStr::width(sym).saturating_sub(1);
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Ring buffer of recent frames.
#[derive(Debug)]
pub struct FrameCapture {
    cap: usize,
    frames: VecDeque<Frame>,
    seq: u64,
    dropped: u64,
    /// `(digest, layout, at)` of the last distinct frames.
    recent: VecDeque<(Hash, Hash, u64)>,
}

pub const LAYOUT_OSCILLATION: &str = "layout_oscillation";

/// Four frames alternating between the same two screens within this window count as flicker.
pub const OSCILLATION_WINDOW_MS: u64 = 1000;

impl FrameCapture {
    pub fn new(cap: usize) -> FrameCapture {
        FrameCapture { cap: cap.max(1), frames: VecDeque::new(), seq: 0, dropped: 0, recent: VecDeque::new() }
    }

    /// Keeps `frame` unless it is identical to the last kept one and carries no anomaly.
    /// Returns the kept frame (with its sequence number and any detected oscillation).
    ///
    /// Oscillation: the last four distinct frames alternate between exactly two screens whose
    /// layouts differ (X Y X Y) within [`OSCILLATION_WINDOW_MS`]. A layout that changes because
    /// the content changed (a modal growing when its plan arrives) never repeats a screen.
    pub fn record(&mut self, mut frame: Frame) -> Option<&Frame> {
        self.seq += 1;
        frame.seq = self.seq;
        let identical = self.frames.back().is_some_and(|f| f.digest == frame.digest);
        if !identical {
            self.recent.push_back((frame.digest, frame.layout, frame.at));
            while self.recent.len() > 4 {
                self.recent.pop_front();
            }
            if let [a, b, c, d] = self.recent.iter().collect::<Vec<_>>()[..]
                && a.0 == c.0
                && b.0 == d.0
                && a.0 != b.0
                && a.1 != b.1
                && d.2.saturating_sub(a.2) <= OSCILLATION_WINDOW_MS
            {
                frame.anomalies.push(format!(
                    "{LAYOUT_OSCILLATION}: frames {}..{} alternate between two layouts",
                    self.seq.saturating_sub(3),
                    self.seq
                ));
            }
        }
        if identical && frame.anomalies.is_empty() {
            self.dropped += 1;
            return None;
        }
        self.frames.push_back(frame);
        while self.frames.len() > self.cap {
            self.frames.pop_front();
        }
        self.frames.back()
    }

    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter()
    }

    pub fn last(&self) -> Option<&Frame> {
        self.frames.back()
    }

    /// Frames dropped because they matched the previous one.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn to_json(&self) -> Value {
        json!({ "dropped_identical": self.dropped, "frames": self.frames.iter().map(Frame::to_json).collect::<Vec<_>>() })
    }
}
