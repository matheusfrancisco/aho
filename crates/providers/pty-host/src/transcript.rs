//! The terminal's output as plain text lines: what scrolled by, not what is on screen now.
//!
//! A small line discipline over `vte`: printable text lands at the cursor column, `\r` returns
//! to column 0 (so progress bars overwrite in place), `\b` and cursor-left/right move, erase-in-line
//! truncates, `\n` commits the line. Everything else (colors, cursor addressing) is dropped. While a
//! program is on the alternate screen (vim, less, top) nothing is recorded: a redraw is not output.

use aho_proc::OutputBuffer;
use aho_term::Delta;
use vte::{Params, Perform};

pub struct Transcript {
    parser: vte::Parser,
    lines: Lines,
}

struct Lines {
    committed: OutputBuffer,
    line: Vec<char>,
    col: usize,
    alternate: bool,
}

impl Transcript {
    pub fn new(cap: usize) -> Transcript {
        Transcript {
            parser: vte::Parser::new(),
            lines: Lines { committed: OutputBuffer::new(cap), line: Vec::new(), col: 0, alternate: false },
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.parser.advance(&mut self.lines, *b);
        }
    }

    /// Completed lines since `cursor` plus the current incomplete line. `next` stops before the
    /// incomplete line, so the next read shows it again once it is finished.
    pub fn read(&self, cursor: u64) -> Delta {
        let chunk = self.lines.committed.read(cursor);
        let mut text = String::from_utf8_lossy(&chunk.bytes).into_owned();
        let partial: String = self.lines.line.iter().collect();
        text.push_str(partial.trim_end());
        Delta { text, next: chunk.next, dropped: chunk.dropped }
    }

    /// Absolute offset of the end of the committed text (a cursor that skips everything so far).
    pub fn end(&self) -> u64 {
        self.lines.committed.end()
    }
}

impl Lines {
    fn commit(&mut self) {
        let text: String = self.line.iter().collect();
        self.committed.push(text.trim_end().as_bytes());
        self.committed.push(b"\n");
        self.line.clear();
        self.col = 0;
    }

    fn put(&mut self, c: char) {
        if self.line.len() < self.col {
            self.line.resize(self.col, ' ');
        }
        if self.col < self.line.len() {
            self.line[self.col] = c;
        } else {
            self.line.push(c);
        }
        self.col += 1;
    }
}

fn first(params: &Params, default: usize) -> usize {
    match params.iter().next().and_then(|p| p.first()) {
        Some(0) | None => default,
        Some(n) => *n as usize,
    }
}

impl Perform for Lines {
    fn print(&mut self, c: char) {
        if !self.alternate {
            self.put(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        if self.alternate {
            return;
        }
        match byte {
            b'\n' | 0x0b | 0x0c => self.commit(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                let to = (self.col / 8 + 1) * 8;
                while self.col < to {
                    self.put(' ');
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if intermediates.first() == Some(&b'?') {
            let alt = params.iter().any(|p| matches!(p.first(), Some(47 | 1047 | 1049)));
            match action {
                'h' if alt => {
                    if !self.line.is_empty() {
                        self.commit();
                    }
                    self.alternate = true;
                }
                'l' if alt => {
                    self.alternate = false;
                    self.line.clear();
                    self.col = 0;
                }
                _ => {}
            }
            return;
        }
        if self.alternate || !intermediates.is_empty() {
            return;
        }
        match action {
            // erase in line: 0 = to end, 1 = to start, 2 = whole line
            'K' => match params.iter().next().and_then(|p| p.first()).copied().unwrap_or(0) {
                0 => self.line.truncate(self.col),
                1 => {
                    let upto = (self.col + 1).min(self.line.len());
                    self.line[..upto].iter_mut().for_each(|c| *c = ' ');
                }
                _ => self.line.clear(),
            },
            'C' => self.col += first(params, 1),
            'D' => self.col = self.col.saturating_sub(first(params, 1)),
            'G' => self.col = first(params, 1) - 1,
            'P' => {
                let n = first(params, 1).min(self.line.len().saturating_sub(self.col));
                self.line.drain(self.col..self.col + n);
            }
            '@' => {
                let at = self.col.min(self.line.len());
                for _ in 0..first(params, 1) {
                    self.line.insert(at, ' ');
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> String {
        let mut t = Transcript::new(1 << 16);
        t.feed(bytes);
        t.read(0).text
    }

    #[test]
    fn colors_are_dropped_and_crlf_lines_commit() {
        assert_eq!(text(b"\x1b[1;32mok\x1b[0m\r\nnext\r\n"), "ok\nnext\n");
    }

    #[test]
    fn carriage_return_overwrites_and_erase_in_line_truncates() {
        assert_eq!(text(b"10%\r50%\r100%\r\n"), "100%\n");
        assert_eq!(text(b"hello world\r\x1b[Kbye\r\n"), "bye\n");
        assert_eq!(text(b"abc\x08\x08X\r\n"), "aXc\n");
    }

    #[test]
    fn the_incomplete_line_is_shown_but_not_consumed() {
        let mut t = Transcript::new(1 << 16);
        t.feed(b"one\r\n$ ");
        let d = t.read(0);
        assert_eq!(d.text, "one\n$");
        t.feed(b"echo\r\n");
        let d2 = t.read(d.next);
        assert_eq!(d2.text, "$ echo\n");
        assert_eq!(t.read(d2.next).text, "");
    }

    #[test]
    fn alternate_screen_output_is_not_recorded() {
        assert_eq!(text(b"before\r\n\x1b[?1049h\x1b[2J\x1b[Hfull screen\r\n\x1b[?1049lafter\r\n"), "before\nafter\n");
    }

    #[test]
    fn a_cursor_older_than_the_ring_reports_what_was_dropped() {
        let mut t = Transcript::new(8);
        t.feed(b"aaaa\nbbbb\ncccc\n");
        let d = t.read(0);
        assert_eq!(d.text, "bb\ncccc\n");
        assert_eq!(d.dropped, 7);
        assert_eq!(d.next, t.end());
    }
}
