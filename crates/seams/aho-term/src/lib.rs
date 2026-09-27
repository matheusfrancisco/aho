//! PTY terminals, the `Terminals` seam a world provides next to `Subprocess`
//! (`pty-host` under `world = host`; a confining world ships its own provider).
//!
//! A terminal is one process tree on a pseudo-terminal. Its output is read two ways:
//! * the **screen**: what a VT emulator of `Size` shows right now (full-screen programs, prompts);
//! * the **transcript**: the output as plain text lines, addressed by absolute offsets and read with
//!   a cursor, so a reader gets exactly what scrolled by since its last read (the "scrollback delta").
//!
//! `close` kills the whole tree (the session leader, every process group in its session, and
//! descendants that left it) because a hung-up shell does not take its background jobs with it.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;

use aho_cap::{Handle, HandleKind, SeamId};
use aho_log::ProcessIdentity;
use aho_types::BoxFuture;

pub use aho_proc::Exit;

pub const SEAM: SeamId = SeamId("terminals");

// ───────────────────────────── geometry ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub const DEFAULT: Size = Size { cols: 120, rows: 32 };
    pub const MIN_COLS: u16 = 10;
    pub const MIN_ROWS: u16 = 2;
    pub const MAX_COLS: u16 = 500;
    pub const MAX_ROWS: u16 = 200;

    /// A size inside the bounds every provider accepts.
    pub fn new(cols: u16, rows: u16) -> Result<Size, TermError> {
        if !(Self::MIN_COLS..=Self::MAX_COLS).contains(&cols) || !(Self::MIN_ROWS..=Self::MAX_ROWS).contains(&rows) {
            return Err(TermError::BadArgs(format!(
                "a terminal of {cols}x{rows} is outside the supported range ({}..={} columns, {}..={} rows); nothing changed.",
                Self::MIN_COLS,
                Self::MAX_COLS,
                Self::MIN_ROWS,
                Self::MAX_ROWS
            )));
        }
        Ok(Size { cols, rows })
    }
}

impl Default for Size {
    fn default() -> Size {
        Size::DEFAULT
    }
}

// ───────────────────────────── data ─────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenSpec {
    pub argv: Vec<String>,
    /// Relative paths resolve against the world's workspace root; `None` = the root.
    pub cwd: Option<String>,
    /// Added on top of the world's environment (after `TERM`).
    pub env: Vec<(String, String)>,
    pub size: Size,
}

/// The emulator's current display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    /// Visible rows joined by `\n`, trailing blanks and blank bottom rows trimmed.
    pub text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub size: Size,
    /// A full-screen program switched to the alternate screen (vim, less, top).
    pub alternate: bool,
    /// DECCKM: arrow keys must be sent as `ESC O x` rather than `ESC [ x`.
    pub application_cursor: bool,
    /// Last window title the program set (OSC 0/2), empty if none.
    pub title: String,
}

/// One cursor read of the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    /// Completed lines since the cursor, followed by the current incomplete line (not yet
    /// consumed: the next read shows it again, finished).
    pub text: String,
    /// Cursor for the next read.
    pub next: u64,
    /// Bytes between the caller's cursor and the oldest transcript byte still held.
    pub dropped: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub screen: Screen,
    pub delta: Delta,
    /// Bumped on every output chunk and on exit; `Terminal::changed` waits past it.
    pub seq: u64,
    /// Set once the session leader has exited.
    pub exit: Option<Exit>,
}

/// The outcome of `close`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    /// Pids that were signalled (leader first).
    pub killed: Vec<u32>,
    pub exit: Option<Exit>,
    /// Anything the operator should know (e.g. a process that could not be enumerated).
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermError {
    NotFound(String),
    Denied(String),
    BadArgs(String),
    /// The terminal was closed or its process is gone.
    Closed(String),
    Io(String),
}

impl TermError {
    pub fn code(&self) -> &'static str {
        match self {
            TermError::NotFound(_) => "TERM_NOT_FOUND",
            TermError::Denied(_) => "TERM_DENIED",
            TermError::BadArgs(_) => "TOOL_BAD_ARGS",
            TermError::Closed(_) => "TERM_CLOSED",
            TermError::Io(_) => "TERM_IO",
        }
    }
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TermError::NotFound(p) => {
                write!(f, "`{p}` was not found in this world; no terminal was opened. Install it or give a full path.")
            }
            TermError::Denied(m) | TermError::BadArgs(m) | TermError::Closed(m) | TermError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for TermError {}

// ───────────────────────────── the seam ─────────────────────────────

pub trait Terminal: Send + Sync {
    fn identity(&self) -> ProcessIdentity;
    /// Screen now plus the transcript from `cursor`.
    fn snapshot(&self, cursor: u64) -> Snapshot;
    /// Resolves once `Snapshot::seq` exceeds `seq` (new output or exit).
    fn changed(&self, seq: u64) -> BoxFuture<'_, ()>;
    /// Bytes to the terminal's input, exactly as a keyboard would send them.
    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TermError>>;
    /// Resizes the pty (the program gets `SIGWINCH`) and the emulator.
    fn resize(&self, size: Size) -> Result<(), TermError>;
    /// Hangs up and kills the whole process tree. Idempotent.
    fn close(&self) -> BoxFuture<'_, Result<Closed, TermError>>;
}

pub trait Terminals: Send + Sync {
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, Result<Arc<dyn Terminal>, TermError>>;
}

#[derive(Clone)]
pub struct TerminalsHandle(pub Arc<dyn Terminals>);
impl Handle for TerminalsHandle {
    const KIND: HandleKind = HandleKind::Terminals;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_bounds_are_inclusive_and_errors_say_what_is_allowed() {
        assert_eq!(Size::new(10, 2), Ok(Size { cols: 10, rows: 2 }));
        assert_eq!(Size::new(500, 200), Ok(Size { cols: 500, rows: 200 }));
        for (c, r) in [(9, 24), (80, 1), (501, 24), (80, 201)] {
            let e = Size::new(c, r).unwrap_err();
            assert_eq!(e.code(), "TOOL_BAD_ARGS");
            assert!(e.to_string().contains("10..=500 columns, 2..=200 rows"), "{e}");
        }
    }
}
