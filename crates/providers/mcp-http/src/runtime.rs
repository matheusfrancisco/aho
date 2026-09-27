//! The provider's I/O runtime and the per-capability tool table.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aho_mcp::*;
use aho_types::BoxFuture;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The provider's I/O runtime: one named thread driving a current-thread tokio runtime. HTTP
/// exchanges and GET listeners run here, whatever executor (if any) the caller is on. The thread
/// ends when the last handle drops, cancelling whatever still runs.
pub struct IoRuntime {
    pub(crate) handle: tokio::runtime::Handle,
    thread: std::thread::ThreadId,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl IoRuntime {
    pub fn start() -> Result<Arc<IoRuntime>, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the MCP-over-HTTP runtime ({e}); no HTTP server was contacted"))?;
        let handle = rt.handle().clone();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("aho-mcp-http".into())
            .spawn(move || rt.block_on(async move { stopped.await.unwrap_or_default() }))
            .map_err(|e| format!("could not start the MCP-over-HTTP thread ({e}); no HTTP server was contacted"))?;
        Ok(Arc::new(IoRuntime { handle, thread: thread.thread().id(), _stop: stop }))
    }

    pub(crate) fn on_runtime_thread(&self) -> bool {
        std::thread::current().id() == self.thread
    }

    /// Blocks this thread until `fut`, run on the runtime, finishes (build-time handshakes).
    pub fn block<T: Send + 'static>(&self, fut: impl Future<Output = T> + Send + 'static) -> Result<T, McpError> {
        if self.on_runtime_thread() {
            return Err(McpError::Transport(
                "an MCP-over-HTTP handshake was started from its own I/O thread; it was refused to avoid a deadlock".into(),
            ));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.handle.spawn(async move { tx.send(fut.await).is_ok() });
        rx.recv().map_err(|_| McpError::Transport("the MCP-over-HTTP runtime stopped before the handshake finished".into()))
    }

    /// Runs `fut` on the runtime; await the result on any executor. Dropping it aborts the work.
    pub fn run<T: Send + 'static>(
        &self,
        fut: impl Future<Output = Result<T, McpError>> + Send + 'static,
    ) -> BoxFuture<'static, Result<T, McpError>> {
        let task = self.handle.spawn(fut);
        let abort = AbortOnDrop(task.abort_handle());
        Box::pin(async move {
            let out = task.await;
            drop(abort);
            out.map_err(|e| McpError::Transport(format!("the MCP-over-HTTP request ended early ({e})")))?
        })
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Every HTTP server's current tool generation, plus the names other transports already
/// registered (`mcp-stdio`, as `name → "server `x`"`). One per capability; the first listing and
/// every re-list install through it, so conflicts are checked against every other server.
pub struct ToolTable {
    gens: Mutex<Generations>,
    taken: BTreeMap<String, String>,
}

impl ToolTable {
    pub fn new(taken: BTreeMap<String, String>) -> ToolTable {
        ToolTable { gens: Mutex::new(Generations::default()), taken }
    }

    /// Phase one against every other server and every name another transport holds, then phase
    /// two. A fetch failure keeps the previous generation; a conflict installs zero tools.
    pub fn install(&self, server: &str, fetched: Result<Vec<RemoteTool>, McpError>) -> Result<Generation, McpError> {
        let checked = fetched.and_then(|tools| {
            let clash = tools.iter().map(|t| normalize(server, &t.raw_name)).find_map(|n| self.taken.get(&n).map(|w| (n, w.clone())));
            match clash {
                Some((name, with)) => Err(McpError::NameConflict { server: server.to_string(), name, with }),
                None => Ok(tools),
            }
        });
        let mut gens = lock(&self.gens);
        match checked {
            Err(e @ McpError::NameConflict { .. }) => {
                gens.swap(server, Ok(Vec::new()))?;
                Err(e)
            }
            other => gens.swap(server, other).cloned(),
        }
    }

    pub fn current(&self, server: &str) -> Option<Generation> {
        lock(&self.gens).current(server).cloned()
    }

    /// The raw name behind a registered (normalized) name in `server`'s current generation.
    pub fn raw_for(&self, server: &str, normalized: &str) -> Option<String> {
        lock(&self.gens).current(server)?.tools.iter().find(|(n, _)| n == normalized).map(|(_, t)| t.raw_name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_types::RootSchema;
    use serde_json::json;

    fn tool(raw: &str) -> RemoteTool {
        RemoteTool { raw_name: raw.into(), title: None, description: String::new(), schema: RootSchema(json!({"type": "object"})) }
    }

    #[test]
    fn install_checks_other_transports_and_keeps_generations_on_fetch_failure() {
        // stdio server `a` registered `b__c`; HTTP server `a__b` offering `c` normalizes to the same name.
        let taken = BTreeMap::from([(normalize("a", "b__c"), "server `a`".to_string())]);
        let table = ToolTable::new(taken);
        assert_eq!(table.install("h", Ok(vec![tool("y")])).unwrap().number, 1);
        assert!(table.install("h", Err(McpError::Transport("down".into()))).is_err());
        assert_eq!(table.current("h").unwrap().tools.len(), 1, "a failed re-list keeps the previous generation");
        assert_eq!(table.raw_for("h", &normalize("h", "y")).as_deref(), Some("y"));
        assert_eq!(table.install("h", Ok(vec![])).unwrap().number, 2, "a successful re-list swaps in the next generation");
        assert_eq!(table.raw_for("h", &normalize("h", "y")), None);
        let err = table.install("a__b", Ok(vec![tool("c")])).unwrap_err();
        assert!(matches!(&err, McpError::NameConflict { with, .. } if with == "server `a`"), "{err:?}");
        assert!(table.current("a__b").unwrap().tools.is_empty(), "a conflict installs zero tools");
    }
}
