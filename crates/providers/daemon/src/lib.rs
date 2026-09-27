//! The `aho serve` process host. One process owns every session; thin clients (tui, web, cli,
//! remote) attach over a unix socket speaking NDJSON frames. The app calls this library
//! directly: it owns processes and sockets and is neither a capability nor a tool.
//!
//! * [`ready`]: the readiness fd. One byte on an inherited descriptor tells the parent that a
//!   freshly exec'd binary is serving. The only module with `unsafe` (fcntl and a pre_exec hook: passing the fd across exec).
//! * [`wire`]: the frame envelope and the version handshake.
//! * [`takeover`]: who may drive a session when a second client attaches.
//! * [`crash`]: crash detection by `(pid, start_time)` with the two-scan rule, and recovery by
//!   `fork(at)`.
//! * [`recovery`]: reload intents persisted in the log and consumed exactly once.
//! * [`server`]: `serve()` and its [`client`].
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![deny(unsafe_code)]

pub mod crash;
#[allow(unsafe_code)]
pub mod ready;
pub mod recovery;
pub mod server;
pub mod takeover;
pub mod wire;

pub use server::{ServeExit, ServeOptions, SessionHost, client, serve};
