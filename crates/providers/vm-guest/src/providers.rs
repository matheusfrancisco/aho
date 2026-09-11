use crate::{Guest, connection::lock, pipes, protocol as p};
use aho_fs::{AppliedDiff, FileVersion, Fs, FsError, GrepHit, ReadResult, ScratchDir, Stat};
use aho_log::ProcessIdentity;
use aho_proc::{Child, Exit, ProcError, SpawnSpec, Subprocess, TransportChild};
use aho_term::{Closed, OpenSpec, Size, Snapshot, TermError, Terminal, Terminals};
use aho_types::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub(crate) struct Remote(pub Arc<Guest>);
impl Fs for Remote {
    fn root(&self) -> String {
        self.0.workspace.clone()
    }
    fn read(&self, path: &str, range: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>> {
        let args = json!({"path":path,"range":range});
        Box::pin(async move { self.0.call("fs.read", args).await.and_then(p::read_result).map_err(p::WireError::fs) })
    }
    fn read_bytes(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        let args = json!({"path":path});
        Box::pin(async move { self.0.call("fs.read_bytes", args).await.and_then(p::decode).map_err(p::WireError::fs) })
    }
    fn stat(&self, path: &str) -> BoxFuture<'_, Result<Stat, FsError>> {
        let args = json!({"path":path});
        Box::pin(async move { self.0.call("fs.stat", args).await.and_then(p::stat).map_err(p::WireError::fs) })
    }
    fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let args = json!({"path":path,"text":text,"expect":expect.map(|v|v.0)});
        Box::pin(async move { self.0.call("fs.write", args).await.and_then(p::diff).map_err(p::WireError::fs) })
    }
    fn edit(
        &self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
        expect: FileVersion,
    ) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let args = json!({"path":path,"old":old,"new":new,"replace_all":replace_all,"expect":expect.0});
        Box::pin(async move { self.0.call("fs.edit", args).await.and_then(p::diff).map_err(p::WireError::fs) })
    }
    fn glob(&self, pattern: &str, base: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let args = json!({"pattern":pattern,"base":base});
        Box::pin(async move { self.0.call("fs.glob", args).await.and_then(p::decode).map_err(p::WireError::fs) })
    }
    fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>> {
        let args = json!({"pattern":pattern,"base":base,"include":include});
        Box::pin(async move {
            self.0
                .call("fs.grep", args)
                .await
                .and_then(|v| p::decode::<Vec<Value>>(v)?.into_iter().map(p::hit).collect())
                .map_err(p::WireError::fs)
        })
    }
    fn list(&self, dir: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>> {
        let args = json!({"dir":dir});
        Box::pin(async move {
            self.0
                .call("fs.list", args)
                .await
                .and_then(|v| p::decode::<Vec<Value>>(v)?.into_iter().map(p::stat).collect())
                .map_err(p::WireError::fs)
        })
    }
    fn remove(&self, path: &str) -> BoxFuture<'_, Result<(), FsError>> {
        let args = json!({"path":path});
        Box::pin(async move { self.0.call("fs.remove", args).await.map(|_| ()).map_err(p::WireError::fs) })
    }
    fn scratch_dir(&self, parent: &str, prefix: &str) -> BoxFuture<'_, Result<Box<dyn ScratchDir>, FsError>> {
        let args = json!({"parent":parent,"prefix":prefix});
        Box::pin(async move {
            let v = self.0.call("fs.scratch", args).await.map_err(p::WireError::fs)?;
            let id = p::field(&v, "id").map_err(p::WireError::fs)?;
            let path = p::field(&v, "path").map_err(p::WireError::fs)?;
            Ok(Box::new(RemoteScratch { guest: self.0.clone(), id, path, closed: false }) as Box<dyn ScratchDir>)
        })
    }
}
struct RemoteScratch {
    guest: Arc<Guest>,
    id: u64,
    path: String,
    closed: bool,
}
impl ScratchDir for RemoteScratch {
    fn path(&self) -> &str {
        &self.path
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async move {
            if !self.closed {
                self.guest.call("fs.scratch_close", json!({"id":self.id})).await.map_err(p::WireError::fs)?;
                self.closed = true;
            }
            Ok(())
        })
    }
}
impl Drop for RemoteScratch {
    fn drop(&mut self) {
        if !self.closed {
            self.guest.release("fs.scratch_close", self.id);
        }
    }
}

impl Remote {
    async fn start(&self, op: &'static str, spec: SpawnSpec) -> Result<Arc<RemoteChild>, ProcError> {
        let v = self.0.call(op, p::spawn_value(spec)).await.map_err(p::WireError::proc)?;
        Ok(Arc::new(RemoteChild {
            guest: self.0.clone(),
            id: p::field(&v, "id").map_err(p::WireError::proc)?,
            identity: p::identity(&v).map_err(p::WireError::proc)?,
            failure: Mutex::new(None),
        }))
    }
}
impl Subprocess for Remote {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.start("proc.spawn", spec).await.map(|c| c as Arc<dyn Child>) })
    }
    fn spawn_interactive(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async move { self.start("proc.interactive", spec).await.map(|c| c as Arc<dyn Child>) })
    }
    fn spawn_transport(&self, spec: SpawnSpec) -> BoxFuture<'_, Result<TransportChild, ProcError>> {
        Box::pin(async move {
            let child = self.start("proc.transport", spec).await?;
            let stdin = Box::new(pipes::Writer::new(self.0.clone(), child.id));
            let stdout = Box::new(pipes::Reader::new(self.0.clone(), child.id, "stdout"));
            let stderr = Box::new(pipes::Reader::new(self.0.clone(), child.id, "stderr"));
            Ok(TransportChild { child, stdin, stdout, stderr })
        })
    }
}
struct RemoteChild {
    guest: Arc<Guest>,
    id: u64,
    identity: ProcessIdentity,
    failure: Mutex<Option<ProcError>>,
}
impl Child for RemoteChild {
    fn identity(&self) -> ProcessIdentity {
        self.identity
    }
    fn wait(&self) -> BoxFuture<'_, Result<Exit, ProcError>> {
        Box::pin(async move {
            loop {
                if let Some(e) = lock(&self.failure).clone() {
                    return Err(e);
                }
                let v = self.guest.call("proc.wait", json!({"id":self.id})).await.map_err(p::WireError::proc)?;
                if !v.is_null() {
                    return p::exit(v).map_err(p::WireError::proc);
                }
            }
        })
    }
    fn kill(&self) -> BoxFuture<'_, Result<(), ProcError>> {
        Box::pin(async move { self.guest.call("proc.kill", json!({"id":self.id})).await.map(|_| ()).map_err(p::WireError::proc) })
    }
    fn output(&self, cursor: u64) -> (Vec<u8>, u64, bool) {
        match self.guest.call_sync("proc.output", json!({"id":self.id,"cursor":cursor})).and_then(p::decode) {
            Ok(value) => value,
            Err(e) => {
                *lock(&self.failure) = Some(e.proc());
                (Vec::new(), cursor, false)
            }
        }
    }
    fn readable(&self, cursor: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = self.guest.call("proc.readable", json!({"id":self.id,"cursor":cursor})).await {
                *lock(&self.failure) = Some(e.proc());
            }
        })
    }
    fn write_stdin(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), ProcError>> {
        Box::pin(
            async move { self.guest.call("proc.stdin", json!({"id":self.id,"bytes":bytes})).await.map(|_| ()).map_err(p::WireError::proc) },
        )
    }
}
impl Drop for RemoteChild {
    fn drop(&mut self) {
        self.guest.release("proc.release", self.id);
    }
}

impl Terminals for Remote {
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, Result<Arc<dyn Terminal>, TermError>> {
        Box::pin(async move {
            let v = self.0.call("term.open", p::open_value(spec)).await.map_err(p::WireError::term)?;
            let snapshot = p::snapshot(p::field(&v, "snapshot").map_err(p::WireError::term)?).map_err(p::WireError::term)?;
            Ok(Arc::new(RemoteTerminal {
                guest: self.0.clone(),
                id: p::field(&v, "id").map_err(p::WireError::term)?,
                identity: p::identity(&v).map_err(p::WireError::term)?,
                last: Mutex::new(snapshot),
                failure: Mutex::new(None),
            }) as Arc<dyn Terminal>)
        })
    }
}
struct RemoteTerminal {
    guest: Arc<Guest>,
    id: u64,
    identity: ProcessIdentity,
    last: Mutex<Snapshot>,
    failure: Mutex<Option<TermError>>,
}
impl Terminal for RemoteTerminal {
    fn identity(&self) -> ProcessIdentity {
        self.identity
    }
    fn snapshot(&self, cursor: u64) -> Snapshot {
        match self.guest.call_sync("term.snapshot", json!({"id":self.id,"cursor":cursor})).and_then(p::snapshot) {
            Ok(snapshot) => {
                *lock(&self.last) = snapshot.clone();
                snapshot
            }
            // The seam has no snapshot error channel. Preserve the last observed state, never
            // invent an exit or screen; operations with an error channel report this failure.
            Err(e) => {
                *lock(&self.failure) = Some(e.term());
                lock(&self.last).clone()
            }
        }
    }
    fn changed(&self, seq: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = self.guest.call("term.changed", json!({"id":self.id,"seq":seq})).await {
                *lock(&self.failure) = Some(e.term());
            }
        })
    }
    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TermError>> {
        Box::pin(async move {
            if let Some(e) = lock(&self.failure).clone() {
                return Err(e);
            }
            self.guest.call("term.write", json!({"id":self.id,"bytes":bytes})).await.map(|_| ()).map_err(p::WireError::term)
        })
    }
    fn resize(&self, size: Size) -> Result<(), TermError> {
        if let Some(e) = lock(&self.failure).clone() {
            return Err(e);
        }
        self.guest.call_sync("term.resize", json!({"id":self.id,"cols":size.cols,"rows":size.rows})).map(|_| ()).map_err(p::WireError::term)
    }
    fn close(&self) -> BoxFuture<'_, Result<Closed, TermError>> {
        Box::pin(async move { self.guest.call("term.close", json!({"id":self.id})).await.and_then(p::closed).map_err(p::WireError::term) })
    }
}
impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        self.guest.release("term.release", self.id);
    }
}
