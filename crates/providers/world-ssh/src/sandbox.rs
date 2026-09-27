//! `sandbox-ssh`: the world is another machine. The agent's files, processes and network are the
//! remote host's, and that is what confines it: `enforced by: remote host`.

use std::sync::Arc;

use aho_cap::BoundWorld;
use aho_proc::{SpawnSpec, Subprocess};
use aho_sandbox::Sandbox;
use aho_types::BoxFuture;

use crate::ENFORCED_BY;
use crate::proc::SshSubprocess;
use crate::ssh::{Ssh, sh_command};

pub struct SshSandbox {
    root: String,
    ssh: Arc<Ssh>,
    proc: Arc<SshSubprocess>,
}

impl SshSandbox {
    pub fn new(root: String, ssh: Arc<Ssh>, proc: Arc<SshSubprocess>) -> SshSandbox {
        SshSandbox { root, ssh, proc }
    }
}

impl Sandbox for SshSandbox {
    fn bound(&self) -> BoundWorld {
        BoundWorld { name: "ssh".into(), confines: true, enforced_by: ENFORCED_BY.into() }
    }

    fn writable_roots(&self) -> Vec<String> {
        vec![self.root.clone()]
    }

    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let child = self.proc.spawn(SpawnSpec::new(&["/bin/sh", "-c", "true"])).await.map_err(|e| e.to_string())?;
            match child.wait().await {
                Ok(exit) if exit.code == 0 => Ok(()),
                Ok(exit) => Err(format!("`/bin/sh -c true` exited {} in {} on {}", exit.code, self.root, self.ssh.target().display())),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    /// A local `ssh -tt` that lands in the remote workspace and runs the remote login shell. The
    /// argv is self-contained: the remote root does not exist locally, so run it from any cwd.
    fn operator_shell(&self) -> Vec<String> {
        self.ssh.tty_argv(&sh_command(r#"cd -- "$1" && exec "${SHELL:-/bin/sh}" -l"#, &[&self.root]))
    }
}
