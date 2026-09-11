use aho_fs::{FileVersion, Fs, ScratchDir};
use aho_proc::{Child, Subprocess};
use aho_term::{Terminal, Terminals};
use pty_host::HostTerminals;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use vm_guest::protocol::{self as p, Request, WireError, field};
use world_host::{HostFs, HostSubprocess};

const MAX_SESSIONS: usize = 128;
const MAX_HANDLES: usize = 1024;
const POLL: Duration = Duration::from_millis(200);
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

type ReadPipe = AsyncMutex<Option<Box<dyn AsyncRead + Unpin + Send>>>;
type WritePipe = AsyncMutex<Option<Box<dyn AsyncWrite + Unpin + Send>>>;
struct Pipes {
    stdin: WritePipe,
    stdout: ReadPipe,
    stderr: ReadPipe,
}
struct Process {
    child: Arc<dyn Child>,
    pipes: Option<Pipes>,
}
/// A scratch directory the host holds open; closing it removes it.
type SharedScratch = Arc<AsyncMutex<Box<dyn ScratchDir>>>;

#[derive(Default)]
struct Session {
    children: Mutex<HashMap<u64, Arc<Process>>>,
    terminals: Mutex<HashMap<u64, Arc<dyn Terminal>>>,
    scratch: Mutex<HashMap<u64, SharedScratch>>,
}
pub(crate) struct Server {
    workspace: String,
    fs: HostFs,
    subprocess: HostSubprocess,
    terminals: HostTerminals,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sequence: AtomicU64,
    boot: String,
}
impl Server {
    pub(crate) fn new(workspace: String) -> Result<Self, String> {
        let fs = HostFs::new(Path::new(&workspace))?;
        let subprocess = HostSubprocess::new(fs.root_path());
        let terminals = HostTerminals::new(fs.root_path());
        let boot = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos());
        Ok(Self { workspace, fs, subprocess, terminals, sessions: Mutex::new(HashMap::new()), sequence: AtomicU64::new(1), boot })
    }
    fn next(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }
    fn session(&self, r: &Request) -> Result<Arc<Session>, WireError> {
        if r.version != p::VERSION {
            return Err(WireError::protocol("guest protocol version mismatch; install matching vm-guest-agent"));
        }
        if r.workspace != self.workspace && r.workspace != self.fs.root() {
            return Err(WireError::denied(format!(
                "agent workspace is `{}`, not `{}`; configure --workspace explicitly to match the world",
                self.workspace, r.workspace
            )));
        }
        if r.session.is_empty() || r.session.len() > 128 {
            return Err(WireError::protocol("invalid guest session identifier"));
        }
        let mut sessions = lock(&self.sessions);
        if let Some(session) = sessions.get(&r.session) {
            return Ok(session.clone());
        }
        if r.op != "hello" {
            return Err(WireError::protocol("unknown guest session; reconnect before making requests"));
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err(WireError::protocol("guest session limit reached; close old sessions or restart the agent"));
        }
        let session = Arc::new(Session::default());
        sessions.insert(r.session.clone(), session.clone());
        Ok(session)
    }
    pub(crate) async fn dispatch(&self, r: &Request) -> Result<Value, WireError> {
        let s = self.session(r)?;
        let a = &r.args;
        match r.op.as_str() {
            "hello" | "probe" => Ok(json!({"version":p::VERSION,"boot":self.boot,"workspace":self.fs.root()})),
            "session.close" => {
                lock(&self.sessions).remove(&r.session);
                Ok(Value::Null)
            }
            "fs.read" => Ok(p::read_value(self.fs.read(&field::<String>(a, "path")?, field(a, "range")?).await?)),
            "fs.read_bytes" => Ok(json!(self.fs.read_bytes(&field::<String>(a, "path")?).await?)),
            "fs.stat" => Ok(p::stat_value(self.fs.stat(&field::<String>(a, "path")?).await?)),
            "fs.write" => Ok(p::diff_value(
                self.fs
                    .write(&field::<String>(a, "path")?, &field::<String>(a, "text")?, field::<Option<u64>>(a, "expect")?.map(FileVersion))
                    .await?,
            )),
            "fs.edit" => Ok(p::diff_value(
                self.fs
                    .edit(
                        &field::<String>(a, "path")?,
                        &field::<String>(a, "old")?,
                        &field::<String>(a, "new")?,
                        field(a, "replace_all")?,
                        FileVersion(field(a, "expect")?),
                    )
                    .await?,
            )),
            "fs.glob" => Ok(json!(self.fs.glob(&field::<String>(a, "pattern")?, field::<Option<String>>(a, "base")?.as_deref()).await?)),
            "fs.grep" => Ok(Value::Array(
                self.fs
                    .grep(
                        &field::<String>(a, "pattern")?,
                        field::<Option<String>>(a, "base")?.as_deref(),
                        field::<Option<String>>(a, "include")?.as_deref(),
                    )
                    .await?
                    .into_iter()
                    .map(p::hit_value)
                    .collect(),
            )),
            "fs.list" => Ok(Value::Array(self.fs.list(&field::<String>(a, "dir")?).await?.into_iter().map(p::stat_value).collect())),
            "fs.remove" => {
                self.fs.remove(&field::<String>(a, "path")?).await?;
                Ok(Value::Null)
            }
            "fs.scratch" => {
                let scratch = self.fs.scratch_dir(&field::<String>(a, "parent")?, &field::<String>(a, "prefix")?).await?;
                let path = scratch.path().to_string();
                let id = self.next();
                let mut entries = lock(&s.scratch);
                capacity(entries.len())?;
                entries.insert(id, Arc::new(AsyncMutex::new(scratch)));
                Ok(json!({"id":id,"path":path}))
            }
            "fs.scratch_close" => {
                let id = field(a, "id")?;
                let scratch = lock(&s.scratch).get(&id).cloned();
                if let Some(scratch) = scratch {
                    scratch.lock().await.close().await?;
                    lock(&s.scratch).remove(&id);
                }
                Ok(Value::Null)
            }
            "proc.spawn" | "proc.interactive" | "proc.transport" => {
                capacity(lock(&s.children).len())?;
                let mut spec = p::spawn(a)?;
                let process = match r.op.as_str() {
                    "proc.transport" => {
                        let transport = self.subprocess.spawn_transport(spec).await?;
                        Process {
                            child: transport.child,
                            pipes: Some(Pipes {
                                stdin: AsyncMutex::new(Some(transport.stdin)),
                                stdout: AsyncMutex::new(Some(transport.stdout)),
                                stderr: AsyncMutex::new(Some(transport.stderr)),
                            }),
                        }
                    }
                    "proc.interactive" => {
                        // Start supervision before sending potentially blocking initial stdin.
                        let initial = spec.stdin.take();
                        let child = self.subprocess.spawn_interactive(spec).await?;
                        if let Some(bytes) = initial {
                            bounded_write(child.write_stdin(bytes)).await?;
                        }
                        Process { child, pipes: None }
                    }
                    _ => Process { child: self.subprocess.spawn(spec).await?, pipes: None },
                };
                let mut v = p::identity_value(process.child.identity());
                let id = self.next();
                v["id"] = json!(id);
                let mut entries = lock(&s.children);
                capacity(entries.len())?;
                entries.insert(id, Arc::new(process));
                Ok(v)
            }
            "proc.release" => {
                lock(&s.children).remove(&field::<u64>(a, "id")?);
                Ok(Value::Null)
            }
            "proc.wait" => {
                let process = process(&s, a)?;
                match tokio::time::timeout(POLL, process.child.wait()).await {
                    Ok(exit) => Ok(p::exit_value(exit?)),
                    Err(_) => Ok(Value::Null),
                }
            }
            "proc.kill" => {
                process(&s, a)?.child.kill().await?;
                Ok(Value::Null)
            }
            "proc.output" => Ok(json!(process(&s, a)?.child.output(field(a, "cursor")?))),
            "proc.readable" => {
                let process = process(&s, a)?;
                let _ = tokio::time::timeout(POLL, process.child.readable(field(a, "cursor")?)).await;
                Ok(Value::Null)
            }
            "proc.stdin" => {
                bounded_write(process(&s, a)?.child.write_stdin(field(a, "bytes")?)).await?;
                Ok(Value::Null)
            }
            "pipe.read" => {
                let process = process(&s, a)?;
                let pipes = pipes(&process)?;
                let stream = field::<String>(a, "stream")?;
                let pipe = match stream.as_str() {
                    "stdout" => &pipes.stdout,
                    "stderr" => &pipes.stderr,
                    _ => return Err(WireError::protocol("unknown guest transport stream")),
                };
                let mut slot = pipe.lock().await;
                let Some(reader) = slot.as_mut() else {
                    return Ok(json!({"bytes":[],"eof":true}));
                };
                let mut bytes = vec![0; p::PIPE_CHUNK];
                match tokio::time::timeout(POLL, reader.read(&mut bytes)).await {
                    Ok(Ok(n)) => {
                        bytes.truncate(n);
                        if n == 0 {
                            *slot = None;
                        }
                        Ok(json!({"bytes":bytes,"eof":n==0}))
                    }
                    Ok(Err(e)) => Err(e.into()),
                    Err(_) => Ok(json!({"bytes":[],"eof":false})),
                }
            }
            "pipe.write" => {
                let bytes: Vec<u8> = field(a, "bytes")?;
                if bytes.len() > p::PIPE_CHUNK {
                    return Err(WireError::protocol("guest transport write exceeds 16 KiB chunk limit"));
                }
                let process = process(&s, a)?;
                let pipes = pipes(&process)?;
                let mut slot = pipes.stdin.lock().await;
                let pipe = slot.as_mut().ok_or_else(|| WireError::protocol("guest transport stdin is closed"))?;
                tokio::time::timeout(Duration::from_secs(10), pipe.write_all(&bytes))
                    .await
                    .map_err(|_| WireError::protocol("guest transport stdin blocked for 10s; some bytes may have been written"))??;
                Ok(Value::Null)
            }
            "pipe.stdin_close" | "pipe.stdout_close" | "pipe.stderr_close" => {
                let id = field(a, "id")?;
                let process = lock(&s.children).get(&id).cloned();
                if let Some(process) = process {
                    let pipes = pipes(&process)?;
                    match r.op.as_str() {
                        "pipe.stdin_close" => {
                            pipes.stdin.lock().await.take();
                        }
                        "pipe.stdout_close" => {
                            pipes.stdout.lock().await.take();
                        }
                        _ => {
                            pipes.stderr.lock().await.take();
                        }
                    }
                }
                Ok(Value::Null)
            }
            "term.open" => {
                capacity(lock(&s.terminals).len())?;
                let term = self.terminals.open(p::open(a)?).await?;
                let mut v = p::identity_value(term.identity());
                let id = self.next();
                v["id"] = json!(id);
                v["snapshot"] = p::snapshot_value(term.snapshot(0));
                let mut entries = lock(&s.terminals);
                capacity(entries.len())?;
                entries.insert(id, term);
                Ok(v)
            }
            "term.snapshot" => Ok(p::snapshot_value(terminal(&s, a)?.snapshot(field(a, "cursor")?))),
            "term.changed" => {
                let term = terminal(&s, a)?;
                let _ = tokio::time::timeout(POLL, term.changed(field(a, "seq")?)).await;
                Ok(Value::Null)
            }
            "term.write" => {
                terminal(&s, a)?.write(field(a, "bytes")?).await?;
                Ok(Value::Null)
            }
            "term.resize" => {
                terminal(&s, a)?.resize(p::size(a)?)?;
                Ok(Value::Null)
            }
            "term.close" => Ok(p::closed_value(terminal(&s, a)?.close().await?)),
            "term.release" => {
                lock(&s.terminals).remove(&field::<u64>(a, "id")?);
                Ok(Value::Null)
            }
            _ => Err(WireError::protocol(format!("unknown guest operation `{}`; install matching vm-guest-agent", r.op))),
        }
    }
}
fn capacity(n: usize) -> Result<(), WireError> {
    if n >= MAX_HANDLES { Err(WireError::protocol("guest session handle limit reached; release unused handles")) } else { Ok(()) }
}
fn process(s: &Session, a: &Value) -> Result<Arc<Process>, WireError> {
    let id = field(a, "id")?;
    lock(&s.children).get(&id).cloned().ok_or_else(|| WireError::protocol(format!("guest process handle {id} no longer exists")))
}
fn terminal(s: &Session, a: &Value) -> Result<Arc<dyn Terminal>, WireError> {
    let id = field(a, "id")?;
    lock(&s.terminals).get(&id).cloned().ok_or_else(|| WireError::protocol(format!("guest terminal handle {id} no longer exists")))
}
fn pipes(process: &Process) -> Result<&Pipes, WireError> {
    process.pipes.as_ref().ok_or_else(|| WireError::protocol("process was not started with exclusive transport pipes"))
}
async fn bounded_write(f: impl std::future::Future<Output = Result<(), aho_proc::ProcError>>) -> Result<(), WireError> {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .map_err(|_| WireError::protocol("guest stdin blocked for 10s; some bytes may have been written"))?
        .map_err(Into::into)
}
