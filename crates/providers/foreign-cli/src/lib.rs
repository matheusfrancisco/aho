//! `08`/`10`/`12` — the shared engine for foreign-CLI subagent providers (`subagent-claude`,
//! `subagent-codex`). Doc 12, "From openharness": **observe, don't wrap**.
//!
//! * The other CLI runs untouched, with the user's own credentials, in a PTY: a session of a
//!   dedicated tmux server (`tmux -L aho-subagents`, one session `aho-<cli>-<child>` per child,
//!   working directory = the child's workspace). The task goes on its command line; follow-ups
//!   and answers to its dialogs are typed into its pane (bracketed paste, then Enter);
//!   interrupts press Escape.
//! * State comes from *its* transcript, tailed by byte offset ([`Tail`]): each CLI crate owns a
//!   normalizer (a copy per CLI, never a shared abstraction — 12) that maps every record kind a
//!   recorded session of the real binary has shown onto [`Event`]s, or ignores it with a reason.
//!   Unknown kinds are surfaced as `Subagent/Note`, never guessed at.
//! * What the transcript says is mirrored into the **parent** log as `Subagent/*` entries
//!   ([`mirror`]), each transcript-derived entry carrying the byte offset it came from.
//! * Crash-honest receipts (12): a `Subagent/Receipt` is written before launch; a child whose
//!   transcript never appeared is *launching*, and after a crash becomes `blocked + uncertain`;
//!   attached children resume their tail at the recorded offset; unconfirmed sends become
//!   delivery `unknown`; a tmux session must be missing on two scans before the child is gone.
//! * A run whose transcript is silent while the screen is frozen is returned to the parent as
//!   blocked on the CLI's own dialog (`StopReason::ToolUse` + the screen in the diagnostic), so
//!   the parent can answer through that dialog with `send_message`.
//!
//! Policy: the CLI runs on the host and enforces its own permissions. A spawn is refused when
//! the bound world confines tools (the CLI would escape it) or the child's policy lacks
//! `SpawnsProcess` or `NetworkEgress` (it runs tools and talks to its vendor's API); without
//! `WorkspaceWrite` the CLI is started in its read-only mode.
//!
//! Parent-log access: the session seeds `aho.log` (`Arc<dyn LogStore>`); the provider appends
//! its `Subagent/*` entries there (they are not tool output, so `tool_events` does not apply).

#![forbid(unsafe_code)]

mod drive;
mod event;
pub mod mirror;
mod provider;
mod recover;
mod settings;
mod tail;
mod tmux;

pub use drive::{CHECK_EVERY, SCREEN_LINES};
pub use event::{Event, Normalized, Record, clip, text_of};
pub use mirror::Mirror;
pub use provider::{Cli, ForeignProvider, Launch, LaunchCtx, LocateCtx, Status};
pub use recover::{Recovered, scan};
pub use settings::{
    DEFAULT_BLOCKED_AFTER_MS, DEFAULT_INTERRUPT_GRACE_MS, DEFAULT_MAX_DEPTH, DEFAULT_POLL_MS, DEFAULT_SOCKET, Settings, which,
};
pub use tail::{FileIdent, Line, READ_CHUNK, Reset, Tail, TailRead, file_ident};
pub use tmux::{COMMAND_TIMEOUT, SessionState, Tmux};

/// Registry service name of the seeded parent log (see `aho-session`).
pub const LOG_SERVICE: &str = "aho.log";
