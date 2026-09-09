//! Daemon behavior: takeover, crash detection and recovery, reload intents, the readiness
//! handshake, and the socket end to end against real sessions with a scripted model.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

mod crash;
mod mutation;
mod operator;
mod ready;
mod recovery;
mod rig;
mod serve;
mod takeover;
