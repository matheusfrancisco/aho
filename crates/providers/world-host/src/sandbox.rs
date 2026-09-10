//! `sandbox-none`: the host world confines nothing; it says so.

use std::sync::Arc;

use aho_cap::BoundWorld;
use aho_proc::{SpawnSpec, Subprocess};
use aho_sandbox::Sandbox;
use aho_types::BoxFuture;

use crate::proc::HostSubprocess;

pub struct HostSandbox {
    root: String,
    proc: Arc<HostSubprocess>,
    shell: String,
}

impl HostSandbox {
    /// `shell` is the operator's login shell (`$SHELL`, or `/bin/sh`).
    pub fn new(root: String, proc: Arc<HostSubprocess>, shell: String) -> HostSandbox {
        HostSandbox { root, proc, shell }
    }
}

impl Sandbox for HostSandbox {
    fn bound(&self) -> BoundWorld {
        BoundWorld::host()
    }

    fn writable_roots(&self) -> Vec<String> {
        vec![self.root.clone()]
    }

    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let child = self.proc.spawn(SpawnSpec::new(&["/bin/sh", "-c", "true"])).await.map_err(|e| e.to_string())?;
            match child.wait().await {
                Ok(exit) if exit.code == 0 => Ok(()),
                Ok(exit) => Err(format!("`/bin/sh -c true` exited {} in {}", exit.code, self.root)),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn operator_shell(&self) -> Vec<String> {
        vec![self.shell.clone(), "-l".into()]
    }
}
