use crate::{Core, WORKSPACE};
use aho_proc::{Child, ProcError, SpawnSpec, Subprocess, TransportChild};
use aho_term::{OpenSpec, TermError, Terminal, Terminals};
use aho_types::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub(crate) struct Command {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
}

pub struct NamespaceProcesses {
    core: Arc<Core>,
    host: world_host::HostSubprocess,
    pty: pty_host::HostTerminals,
}

impl NamespaceProcesses {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self { host: world_host::HostSubprocess::new(core.dir()), pty: pty_host::HostTerminals::new(core.dir()), core }
    }
    fn wrap(&self, spec: SpawnSpec) -> Result<SpawnSpec, ProcError> {
        self.core.alive().map_err(ProcError::Denied)?;
        if spec.argv.is_empty() {
            return Err(ProcError::Denied("empty command".into()));
        }
        let command = Command { argv: spec.argv, cwd: cwd(spec.cwd.as_deref())?, env: spec.env };
        let payload = serde_json::to_string(&command).map_err(|e| ProcError::Io(e.to_string()))?;
        Ok(SpawnSpec { argv: self.core.argv("exec", &payload), cwd: None, env: vec![], stdin: spec.stdin, timeout_ms: spec.timeout_ms })
    }
}
fn cwd(path: Option<&str>) -> Result<String, ProcError> {
    let p = path.unwrap_or(WORKSPACE);
    let rel = aho_fs::lexical_rel(WORKSPACE, p).map_err(|e| ProcError::Denied(e.to_string()))?;
    Ok(if rel == "." { WORKSPACE.into() } else { format!("{WORKSPACE}/{rel}") })
}
impl Subprocess for NamespaceProcesses {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.host.spawn(self.wrap(spec)?).await })
    }
    fn spawn_interactive(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.host.spawn_interactive(self.wrap(spec)?).await })
    }
    fn spawn_transport(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<TransportChild, ProcError>> {
        Box::pin(async move { self.host.spawn_transport(self.wrap(spec)?).await })
    }
}
impl Terminals for NamespaceProcesses {
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, Result<Arc<dyn Terminal>, TermError>> {
        Box::pin(async move {
            let wrapped = self
                .wrap(SpawnSpec { argv: spec.argv, cwd: spec.cwd, env: spec.env, ..SpawnSpec::default() })
                .map_err(|e| TermError::Denied(e.to_string()))?;
            self.pty.open(OpenSpec { argv: wrapped.argv, cwd: None, env: vec![], size: spec.size }).await
        })
    }
}
