//! Terminals on the host: the `Terminals` seam (`aho-term`) for `world = host`.
//!
//! Each terminal is a `portable-pty` pseudo-terminal whose program is its own session leader in
//! the workspace root. `vt100` renders output into the screen and, in parallel, into a
//! plain-text transcript (`Transcript`) read with cursors. `close` kills the whole tree: every
//! process in the leader's session plus descendants that left it (`tree`), because a hung-up
//! shell does not take its background jobs with it.
//!
//! Like everything under `host`, nothing is confined: the terminal runs as your user.
//! `aho plan` says `enforced by: policy only`.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

mod transcript;

#[cfg(unix)]
mod pty;
#[cfg(unix)]
mod tree;

use std::path::PathBuf;
use std::sync::Arc;

use aho_cap::*;
use aho_sandbox::SandboxHandle;
use aho_term::{Terminals, TerminalsHandle};

pub use transcript::Transcript;

#[cfg(unix)]
pub use pty::{HostTerminals, PtyTerminal};

pub const ENFORCED_BY: &str = "policy only";
/// Transcript bytes kept per terminal; older text is dropped and reported as `dropped`.
pub const TRANSCRIPT_CAP: usize = 1 << 20;

/// The bound world's `Sandbox`: its first writable root is the workspace terminals start in
/// (the worktree under `workspace.layout = worktree`), and it says whether the world confines.
pub struct PtyDeps {
    pub sandbox: SandboxHandle,
}

impl Deps for PtyDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Custom("sandbox")];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(PtyDeps { sandbox: r.handle()? })
    }
}

pub struct PtyHost {
    terminals: Arc<dyn Terminals>,
}

impl Capability for PtyHost {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = PtyDeps;
    type Config = ();

    fn build(deps: PtyDeps, _: ()) -> Result<Self, BuildError> {
        if deps.sandbox.0.bound().confines {
            return Err(BuildError::Unsupported(
                "pty-host opens terminals on the host, but the bound world confines its processes; bind that world's own terminals provider (`seams.terminals`)",
            ));
        }
        let Some(root) = deps.sandbox.0.writable_roots().into_iter().next() else {
            return Err(BuildError::Probe(
                "the bound world reported no workspace root, so terminals have no working directory; check `workspace.root`".into(),
            ));
        };
        Self::for_root(PathBuf::from(root))
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(TerminalsHandle(self.terminals.clone()));
    }
}

impl PtyHost {
    #[cfg(unix)]
    fn for_root(root: PathBuf) -> Result<Self, BuildError> {
        Ok(PtyHost { terminals: Arc::new(HostTerminals::new(root)) })
    }

    #[cfg(not(unix))]
    fn for_root(_: PathBuf) -> Result<Self, BuildError> {
        Err(BuildError::Unsupported("pty-host needs a POSIX host (Linux or macOS)"))
    }
}

declare_capability!(
    PtyHost,
    "pty-host",
    kind = Kind::Provider,
    provides = [aho_term::SEAM],
    requires = [
        Requirement::Setting { key: "seams.world", pred: Pred::Eq("host") },
        Requirement::AnyOf(&[Requirement::Platform(Os::Linux), Requirement::Platform(Os::MacOs)]),
    ],
    tools = &[],
    provides_handles = [HandleKind::Terminals],
    unlock = Fix::cli("aho set seams.world host"),
    enforced_by = ENFORCED_BY,
    doc = "PTY terminals on the host (world = host): portable-pty sessions in the workspace root, rendered by a vt100 emulator into a screen plus a plain-text transcript read with cursors. Closing a terminal kills its whole process tree, background jobs included. Nothing is confined: terminals run as your user."
);

#[cfg(test)]
mod tests;
