//! Inline rendering: the finished transcript lives in the terminal's own scrollback (scroll it,
//! search it, copy from it like any output), and only the live bottom — the running part of the
//! turn, the prompt, the menu and the hint row — is a viewport ratatui redraws.
//!
//! A block is written to the scrollback once, when it is settled ([`Transcript::settled`]: every
//! block before the first tool card still waiting for its result). The viewport grows and
//! shrinks with what it shows; ratatui's inline viewports have a fixed height, so a new height
//! re-creates the terminal at the old viewport's top row.
//!
//! Resizing: the emulator rewraps the screen before aho hears about it, so the old viewport's
//! rows (full-width borders) may now take more rows, and nothing on screen says where the
//! viewport starts. Terminals keep the cursor on its logical line through a rewrap, so the
//! hardware cursor is hidden and parked at the viewport's top-left between frames (the prompt
//! shows a drawn cursor instead): after a resize the cursor *is* the viewport's new top, and
//! everything from it down is erased and redrawn. Without the parking, each resize would leave
//! the rewrapped part of the old border above the new viewport.
//!
//! [`Transcript::settled`]: crate::Transcript::settled

use std::io;

use aho_types::EntryId;
use ratatui::backend::ClearType;
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::frames::Probe;
use crate::model::UiState;
use crate::render::{block_lines, draw_inline, inline_height};

/// A backend a new terminal can be opened on after the old one is dropped, and that can write
/// rows into the scrollback above an inline viewport.
pub trait Reopen: Backend + Sized {
    fn reopen(&self) -> Self;

    /// Writes `lines` above the viewport. `Ok(true)`: the viewport was cleared and the caller
    /// re-opens it below the new rows; `Ok(false)`: the viewport is still in place.
    fn history(term: &mut Terminal<Self>, lines: &[Line<'static>]) -> io::Result<bool> {
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        term.insert_before(rows, |buf| {
            for (y, line) in lines.iter().enumerate().take(rows as usize) {
                let row = Rect { y: buf.area.y + y as u16, height: 1, ..buf.area };
                if let Some(bg) = line.style.bg {
                    buf.set_style(row, Style::default().bg(bg));
                }
                buf.set_line(row.x, row.y, line, row.width);
            }
        })?;
        Ok(false)
    }
}

impl Reopen for ratatui::backend::CrosstermBackend<std::io::Stdout> {
    fn reopen(&self) -> Self {
        ratatui::backend::CrosstermBackend::new(std::io::stdout())
    }

    /// Prints the rows as text (no trailing padding, so a narrower window rewraps them cleanly;
    /// a tinted row fills with erase-to-end-of-line), from the viewport's top row down, letting
    /// the terminal scroll them into its history.
    fn history(term: &mut Terminal<Self>, lines: &[Line<'static>]) -> io::Result<bool> {
        use ratatui::crossterm::queue;
        use ratatui::crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor};
        use ratatui::crossterm::terminal::{Clear, ClearType};
        use ratatui::style::Modifier;
        let top = term.get_frame().area().y;
        let bottom = term.size()?.height;
        let out = term.backend_mut();
        erase_rows(out, top, bottom)?;
        let attrs = |out: &mut Self, m: Modifier| -> io::Result<()> {
            for (bit, a) in [
                (Modifier::BOLD, Attribute::Bold),
                (Modifier::DIM, Attribute::Dim),
                (Modifier::ITALIC, Attribute::Italic),
                (Modifier::UNDERLINED, Attribute::Underlined),
                (Modifier::REVERSED, Attribute::Reverse),
            ] {
                if m.contains(bit) {
                    queue!(out, SetAttribute(a))?;
                }
            }
            Ok(())
        };
        for line in lines {
            for span in &line.spans {
                let style = line.style.patch(span.style);
                queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
                if let Some(fg) = style.fg {
                    queue!(out, SetForegroundColor(fg.into()))?;
                }
                if let Some(bg) = style.bg {
                    queue!(out, SetBackgroundColor(bg.into()))?;
                }
                attrs(out, style.add_modifier)?;
                queue!(out, Print(span.content.as_ref()))?;
            }
            queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
            if let Some(bg) = line.style.bg {
                queue!(out, SetBackgroundColor(bg.into()), Clear(ClearType::UntilNewLine), ResetColor)?;
            }
            queue!(out, Print("\r\n"))?;
        }
        std::io::Write::flush(out)?;
        Ok(true)
    }
}

impl Reopen for TestBackend {
    fn reopen(&self) -> Self {
        self.clone()
    }
}

/// Viewport heights change in steps of this many rows while a turn runs (fewer re-creations
/// while an answer streams in).
const GROW_STEP: u16 = 4;

pub struct InlineScreen<B: Reopen> {
    term: Terminal<B>,
    height: u16,
    /// Blocks already written to the scrollback.
    printed: usize,
    /// The entry of the last written block: after a refold (fork, rewind) the blocks up to it are
    /// still on screen and are not written again.
    printed_upto: Option<EntryId>,
    /// The terminal size the viewport was laid out for.
    size: Size,
}

impl<B: Reopen> InlineScreen<B> {
    /// Opens the viewport at the cursor; nothing of `ui` is written yet.
    pub fn new(backend: B, height: u16) -> io::Result<Self> {
        let height = height.max(1);
        let term = Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(height) })?;
        let size = term.size()?;
        Ok(InlineScreen { term, height, printed: 0, printed_upto: None, size })
    }

    pub fn backend(&self) -> &B {
        self.term.backend()
    }

    #[cfg(test)]
    pub(crate) fn backend_mut(&mut self) -> &mut B {
        self.term.backend_mut()
    }

    /// Blocks written to the scrollback.
    pub fn printed(&self) -> usize {
        self.printed
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// The transcript was refolded: blocks at or before the last written entry count as written.
    pub fn reanchor(&mut self, ui: &UiState) {
        self.printed = match self.printed_upto {
            None => 0,
            Some(upto) => ui.transcript.blocks.iter().position(|b| b.entry > upto).unwrap_or(ui.transcript.blocks.len()),
        };
    }

    /// The screen under an overlay was replaced (alternate screen), and may have been resized
    /// and rewrapped meanwhile: repaint the viewport from the parked cursor.
    pub fn invalidate(&mut self) -> io::Result<()> {
        self.reflowed()
    }

    /// The viewport starts at the parked cursor (wherever the rewrap moved it): erase from there
    /// down and open the viewport again at that row.
    fn reflowed(&mut self) -> io::Result<()> {
        let top = self.term.backend_mut().get_cursor_position()?.y;
        let bottom = self.term.size()?.height;
        erase_rows(self.term.backend_mut(), top, bottom)?;
        let backend = self.term.backend().reopen();
        self.term = Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(self.height) })?;
        self.size = self.term.size()?;
        Ok(())
    }

    /// Writes newly settled blocks to the scrollback, sizes the viewport, draws the live bottom;
    /// returns the viewport as drawn.
    pub fn draw(&mut self, ui: &UiState, probe: &mut Probe) -> io::Result<Buffer> {
        if self.term.size()? != self.size {
            self.reflowed()?;
        }
        let size = self.size;
        let settled = ui.transcript.settled().max(self.printed.min(ui.transcript.blocks.len()));
        if settled > self.printed {
            let lines = block_lines(ui, self.printed..settled, size.width as usize);
            if !lines.is_empty() && B::history(&mut self.term, &lines)? {
                // The viewport was cleared under the new rows: open it again below them.
                let backend = self.term.backend().reopen();
                self.term = Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(self.height) })?;
            }
            self.printed = settled;
            self.printed_upto = ui.transcript.blocks.get(settled - 1).map(|b| b.entry);
        }
        let want = inline_height(ui, self.printed, size.width, size.height.max(1));
        let next = if want > self.height {
            // Grow now, in steps while a turn runs.
            if ui.is_busy() { want.div_ceil(GROW_STEP).saturating_mul(GROW_STEP).min(size.height.max(1)) } else { want }
        } else if want < self.height && !ui.is_busy() {
            want
        } else {
            self.height
        };
        if next != self.height {
            self.resize(next)?;
        }
        let printed = self.printed;
        let done = self.term.draw(|f| {
            let area = f.area();
            if let Some(pos) = draw_inline(ui, printed, area, f.buffer_mut(), probe)
                && let Some(cell) = f.buffer_mut().cell_mut(pos)
            {
                cell.modifier.insert(Modifier::REVERSED);
            }
        })?;
        let buffer = done.buffer.clone();
        let top = self.term.get_frame().area().y;
        // No frame cursor: ratatui hid it. Park it where a rewrap keeps the viewport's top.
        self.term.set_cursor_position(Position { x: 0, y: top })?;
        self.term.backend_mut().flush()?;
        Ok(buffer)
    }

    /// Blanks the viewport's rows; the cursor is left at its top-left.
    fn erase_viewport(&mut self) -> io::Result<()> {
        let area = self.term.get_frame().area();
        erase_rows(self.term.backend_mut(), area.y, area.bottom())
    }

    /// A new viewport height: clear the old viewport and open the new one at its top row.
    fn resize(&mut self, height: u16) -> io::Result<()> {
        self.erase_viewport()?;
        let backend = self.term.backend().reopen();
        self.term = Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(height) })?;
        self.height = height;
        self.size = self.term.size()?;
        Ok(())
    }

    /// Leaves the transcript in the scrollback and the cursor below it: the viewport is cleared.
    pub fn finish(&mut self) -> io::Result<()> {
        self.erase_viewport()?;
        self.term.show_cursor()
    }
}

/// Blanks rows `from..to` one line at a time and leaves the cursor at `(0, from)`. Never an
/// erase-below: from the top-left corner that is "clear the screen", which tmux
/// (`scroll-on-clear`, on by default) first copies into the scrollback, leaving a stale copy of
/// the viewport in the history.
fn erase_rows<B: Backend>(b: &mut B, from: u16, to: u16) -> io::Result<()> {
    for y in from..to {
        b.set_cursor_position(Position { x: 0, y })?;
        b.clear_region(ClearType::CurrentLine)?;
    }
    b.set_cursor_position(Position { x: 0, y: from })?;
    b.flush()
}
