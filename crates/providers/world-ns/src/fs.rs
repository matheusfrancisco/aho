//! The host never resolves an agent-supplied filesystem path. The existing Fs
//! implementation and shadow-git store run in a short-lived confined helper.
use crate::{Core, WORKSPACE};
use aho_fs::*;
use aho_proc::{SpawnSpec, Subprocess};
use aho_types::{BoxFuture, Hash};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Read(String, Option<(u32, u32)>),
    Bytes(String),
    Stat(String),
    List(String),
    Write(String, String, Option<u64>),
    Edit(String, String, String, bool, u64),
    Glob(String, Option<String>),
    Grep(String, Option<String>, Option<String>),
    Remove(String),
    Tree,
    Restore(String),
    Diff(String, String),
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    value: Value,
    error: Option<WireError>,
}
#[derive(Serialize, Deserialize)]
struct WireError {
    kind: String,
    path: String,
    expected: u64,
    actual: u64,
    count: usize,
}
impl From<FsError> for WireError {
    fn from(e: FsError) -> Self {
        let mut w = Self { kind: e.code().into(), path: String::new(), expected: 0, actual: 0, count: 0 };
        w.path = match e {
            FsError::NotFound(p)
            | FsError::NotObserved(p)
            | FsError::OutsideWorkspace(p)
            | FsError::DanglingSymlink(p)
            | FsError::NoMatch(p)
            | FsError::Invalid(p)
            | FsError::Denied(p)
            | FsError::Io(p) => p,
            FsError::StaleVersion { path, expected, actual } => {
                w.expected = expected.0;
                w.actual = actual.0;
                path
            }
            FsError::NotUnique { path, count } => {
                w.count = count;
                path
            }
        };
        w
    }
}
impl From<WireError> for FsError {
    fn from(w: WireError) -> Self {
        match w.kind.as_str() {
            "FS_NOT_FOUND" => Self::NotFound(w.path),
            "FS_NOT_OBSERVED" => Self::NotObserved(w.path),
            "FS_OUTSIDE_WORKSPACE" => Self::OutsideWorkspace(w.path),
            "FS_DANGLING_SYMLINK" => Self::DanglingSymlink(w.path),
            "FS_STALE_VERSION" => Self::StaleVersion { path: w.path, expected: FileVersion(w.expected), actual: FileVersion(w.actual) },
            "FS_NOT_UNIQUE" => Self::NotUnique { path: w.path, count: w.count },
            "FS_NO_MATCH" => Self::NoMatch(w.path),
            "FS_INVALID" => Self::Invalid(w.path),
            "FS_DENIED" => Self::Denied(w.path),
            _ => Self::Io(w.path),
        }
    }
}

pub struct NamespaceFs {
    core: Arc<Core>,
    lock: tokio::sync::Mutex<()>,
}
impl NamespaceFs {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self { core, lock: tokio::sync::Mutex::new(()) }
    }
    async fn call(&self, request: Request) -> Result<Value, FsError> {
        let _guard = self.lock.lock().await;
        self.core.alive().map_err(FsError::Denied)?;
        let p = world_host::HostSubprocess::new(self.core.dir());
        let aho_proc::TransportChild { child, mut stdin, mut stdout, mut stderr } = p
            .spawn_transport(SpawnSpec { argv: self.core.argv("fs", ""), timeout_ms: Some(120_000), ..SpawnSpec::default() })
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        let data = serde_json::to_vec(&request).map_err(|e| FsError::Io(e.to_string()))?;
        // Read both pipes while feeding stdin: large files must never deadlock. The helper reads
        // its request to EOF, and only dropping the pipe closes it (`shutdown` merely flushes).
        let input = async move {
            stdin.write_all(&data).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<(), std::io::Error>(())
        };
        let mut output = Vec::new();
        let mut error = Vec::new();
        let (a, b, c) = tokio::join!(input, stdout.read_to_end(&mut output), stderr.read_to_end(&mut error));
        a.and(b.map(|_| ())).and(c.map(|_| ())).map_err(|e| FsError::Io(e.to_string()))?;
        let exit = child.wait().await.map_err(|e| FsError::Io(e.to_string()))?;
        if exit.code != 0 {
            return Err(FsError::Io(format!("namespace filesystem helper exited {}: {}", exit.code, String::from_utf8_lossy(&error))));
        }
        let result: Response = serde_json::from_slice(&output).map_err(|e| FsError::Io(format!("invalid namespace fs response: {e}")))?;
        match result.error {
            Some(e) => Err(e.into()),
            None => Ok(result.value),
        }
    }
}
fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, FsError> {
    serde_json::from_value(v).map_err(|e| FsError::Io(e.to_string()))
}
fn stat(v: Value) -> Result<Stat, FsError> {
    let (path, is_dir, bytes, version): (String, bool, u64, Option<u64>) = decode(v)?;
    Ok(Stat { path, is_dir, bytes, version: version.map(FileVersion) })
}
fn diff(v: Value) -> Result<AppliedDiff, FsError> {
    let (path, created, hunks, new_version): (String, bool, Vec<String>, u64) = decode(v)?;
    Ok(AppliedDiff { path, created, hunks, new_version: FileVersion(new_version) })
}
fn delta(v: Value) -> Result<RestoreDelta, FsError> {
    let (changed, deleted) = decode(v)?;
    Ok(RestoreDelta { changed, deleted })
}
impl Fs for NamespaceFs {
    fn root(&self) -> String {
        WORKSPACE.into()
    }
    fn read(&self, p: &str, r: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>> {
        let q = Request::Read(p.into(), r);
        Box::pin(async move {
            let (text, version, total_lines, partial): (String, u64, u32, bool) = decode(self.call(q).await?)?;
            Ok(ReadResult { text, version: FileVersion(version), total_lines, partial })
        })
    }
    fn read_bytes(&self, p: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        let q = Request::Bytes(p.into());
        Box::pin(async move { decode(self.call(q).await?) })
    }
    fn stat(&self, p: &str) -> BoxFuture<'_, Result<Stat, FsError>> {
        let q = Request::Stat(p.into());
        Box::pin(async move { stat(self.call(q).await?) })
    }
    fn list(&self, p: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>> {
        let q = Request::List(p.into());
        Box::pin(async move {
            let v: Vec<Value> = decode(self.call(q).await?)?;
            v.into_iter().map(stat).collect()
        })
    }
    fn write(&self, p: &str, t: &str, e: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let q = Request::Write(p.into(), t.into(), e.map(|v| v.0));
        Box::pin(async move { diff(self.call(q).await?) })
    }
    fn edit(&self, p: &str, o: &str, n: &str, a: bool, e: FileVersion) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let q = Request::Edit(p.into(), o.into(), n.into(), a, e.0);
        Box::pin(async move { diff(self.call(q).await?) })
    }
    fn glob(&self, p: &str, b: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let q = Request::Glob(p.into(), b.map(str::to_string));
        Box::pin(async move { decode(self.call(q).await?) })
    }
    fn grep(&self, p: &str, b: Option<&str>, i: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>> {
        let q = Request::Grep(p.into(), b.map(str::to_string), i.map(str::to_string));
        Box::pin(async move {
            let rows: Vec<(String, u32, String)> = decode(self.call(q).await?)?;
            Ok(rows.into_iter().map(|(path, line, text)| GrepHit { path, line, text }).collect())
        })
    }
    fn remove(&self, p: &str) -> BoxFuture<'_, Result<(), FsError>> {
        let q = Request::Remove(p.into());
        Box::pin(async move { self.call(q).await.map(|_| ()) })
    }
}
impl Snapshot for NamespaceFs {
    fn tree(&self) -> BoxFuture<'_, Result<Hash, FsError>> {
        Box::pin(async move {
            let s: String = decode(self.call(Request::Tree).await?)?;
            Hash::from_hex(&s).ok_or_else(|| FsError::Io("invalid checkpoint hash".into()))
        })
    }
    fn restore(&self, h: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        Box::pin(async move { delta(self.call(Request::Restore(h.to_hex())).await?) })
    }
    fn diff(&self, a: Hash, b: Hash) -> BoxFuture<'_, Result<RestoreDelta, FsError>> {
        Box::pin(async move { delta(self.call(Request::Diff(a.to_hex(), b.to_hex())).await?) })
    }
}

#[cfg(target_os = "linux")]
fn stat_value(s: Stat) -> Value {
    json!([s.path, s.is_dir, s.bytes, s.version.map(|v| v.0)])
}
#[cfg(target_os = "linux")]
fn diff_value(d: AppliedDiff) -> Value {
    json!([d.path, d.created, d.hunks, d.new_version.0])
}
#[cfg(target_os = "linux")]
fn delta_value(d: RestoreDelta) -> Value {
    json!([d.changed, d.deleted])
}
#[cfg(target_os = "linux")]
/// Called only after pivot_root, private /proc and capability removal.
pub(crate) async fn serve(request: Request) -> Response {
    let result = async {
        let fs = world_host::HostFs::new(std::path::Path::new(WORKSPACE)).map_err(FsError::Io)?;
        let snap = world_host::ShadowGit::new(std::path::Path::new(WORKSPACE), std::path::Path::new("/checkpoint"));
        let hash = |s: &str| Hash::from_hex(s).ok_or_else(|| FsError::Invalid("invalid checkpoint hash".into()));
        Ok::<Value, FsError>(match request {
            Request::Read(p, r) => {
                let v = fs.read(&p, r).await?;
                json!([v.text, v.version.0, v.total_lines, v.partial])
            }
            Request::Bytes(p) => json!(fs.read_bytes(&p).await?),
            Request::Stat(p) => stat_value(fs.stat(&p).await?),
            Request::List(p) => json!(fs.list(&p).await?.into_iter().map(stat_value).collect::<Vec<_>>()),
            Request::Write(p, t, e) => diff_value(fs.write(&p, &t, e.map(FileVersion)).await?),
            Request::Edit(p, o, n, a, e) => diff_value(fs.edit(&p, &o, &n, a, FileVersion(e)).await?),
            Request::Glob(p, b) => json!(fs.glob(&p, b.as_deref()).await?),
            Request::Grep(p, b, i) => {
                json!(fs.grep(&p, b.as_deref(), i.as_deref()).await?.into_iter().map(|h| (h.path, h.line, h.text)).collect::<Vec<_>>())
            }
            Request::Remove(p) => {
                fs.remove(&p).await?;
                Value::Null
            }
            Request::Tree => json!(snap.tree().await?.to_hex()),
            Request::Restore(h) => delta_value(snap.restore(hash(&h)?).await?),
            Request::Diff(a, b) => delta_value(snap.diff(hash(&a)?, hash(&b)?).await?),
        })
    }
    .await;
    match result {
        Ok(value) => Response { value, error: None },
        Err(e) => Response { value: Value::Null, error: Some(e.into()) },
    }
}
