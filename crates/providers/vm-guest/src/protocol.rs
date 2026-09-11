//! Versioned, bounded JSON frames shared with the executable endpoint.
use aho_fs::{AppliedDiff, FileVersion, FsError, GrepHit, ReadResult, Stat};
use aho_log::ProcessIdentity;
use aho_proc::{Exit, ProcError, SpawnSpec};
use aho_term::{Closed, Delta, OpenSpec, Screen, Size, Snapshot, TermError};
use aho_types::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 << 20;
pub const PIPE_CHUNK: usize = 16 << 10;
pub const MAX_INFLIGHT: usize = 128;
pub const REQUEST_SECONDS: u64 = 30;
pub const DEVICE: &str = "/dev/virtio-ports/org.aho.agent";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub version: u32,
    pub session: String,
    pub workspace: String,
    pub op: String,
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub result: Result<Value, WireError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireError {
    pub kind: String,
    pub message: String,
    pub detail: Value,
}
impl WireError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self { kind: "PROTOCOL".into(), message: message.into(), detail: Value::Null }
    }
    pub fn denied(message: String) -> Self {
        Self { kind: "DENIED".into(), message, detail: Value::Null }
    }
    pub fn fs(self) -> FsError {
        let p = || self.detail.as_str().unwrap_or(&self.message).to_string();
        match self.kind.as_str() {
            "FS_NOT_FOUND" => FsError::NotFound(p()),
            "FS_NOT_OBSERVED" => FsError::NotObserved(p()),
            "FS_OUTSIDE_WORKSPACE" => FsError::OutsideWorkspace(p()),
            "FS_DANGLING_SYMLINK" => FsError::DanglingSymlink(p()),
            "FS_NO_MATCH" => FsError::NoMatch(p()),
            "FS_INVALID" => FsError::Invalid(self.message),
            "FS_DENIED" | "DENIED" => FsError::Denied(self.message),
            "FS_STALE_VERSION" => match (field(&self.detail, "path"), field(&self.detail, "expected"), field(&self.detail, "actual")) {
                (Ok(path), Ok(expected), Ok(actual)) => {
                    FsError::StaleVersion { path, expected: FileVersion(expected), actual: FileVersion(actual) }
                }
                _ => FsError::Io("malformed guest stale-version error".into()),
            },
            "FS_NOT_UNIQUE" => match (field(&self.detail, "path"), field(&self.detail, "count")) {
                (Ok(path), Ok(count)) => FsError::NotUnique { path, count },
                _ => FsError::Io("malformed guest uniqueness error".into()),
            },
            _ => FsError::Io(self.message),
        }
    }
    pub fn proc(self) -> ProcError {
        match self.kind.as_str() {
            "PROC_NOT_FOUND" => ProcError::NotFound(self.detail.as_str().unwrap_or(&self.message).into()),
            "PROC_DENIED" | "DENIED" => ProcError::Denied(self.message),
            "PROC_TIMEOUT" => ProcError::Timeout,
            _ => ProcError::Io(self.message),
        }
    }
    pub fn term(self) -> TermError {
        match self.kind.as_str() {
            "TERM_NOT_FOUND" => TermError::NotFound(self.detail.as_str().unwrap_or(&self.message).into()),
            "TERM_DENIED" | "DENIED" => TermError::Denied(self.message),
            "TOOL_BAD_ARGS" => TermError::BadArgs(self.message),
            "TERM_CLOSED" => TermError::Closed(self.message),
            _ => TermError::Io(self.message),
        }
    }
}
impl From<FsError> for WireError {
    fn from(e: FsError) -> Self {
        let detail = match &e {
            FsError::NotFound(p)
            | FsError::NotObserved(p)
            | FsError::OutsideWorkspace(p)
            | FsError::DanglingSymlink(p)
            | FsError::NoMatch(p) => json!(p),
            FsError::StaleVersion { path, expected, actual } => json!({"path":path,"expected":expected.0,"actual":actual.0}),
            FsError::NotUnique { path, count } => json!({"path":path,"count":count}),
            _ => Value::Null,
        };
        Self { kind: e.code().into(), message: e.to_string(), detail }
    }
}
impl From<ProcError> for WireError {
    fn from(e: ProcError) -> Self {
        let detail = match &e {
            ProcError::NotFound(p) => json!(p),
            _ => Value::Null,
        };
        Self { kind: e.code().into(), message: e.to_string(), detail }
    }
}
impl From<TermError> for WireError {
    fn from(e: TermError) -> Self {
        let detail = match &e {
            TermError::NotFound(p) => json!(p),
            _ => Value::Null,
        };
        Self { kind: e.code().into(), message: e.to_string(), detail }
    }
}
impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        Self::protocol(format!(
            "guest channel I/O failed: {e}; check the guest agent and reconnect (an in-flight effect may have completed)"
        ))
    }
}

pub fn field<T: DeserializeOwned>(v: &Value, name: &str) -> Result<T, WireError> {
    let value = v.get(name).ok_or_else(|| WireError::protocol(format!("guest protocol field `{name}` is missing")))?;
    serde_json::from_value(value.clone()).map_err(|e| WireError::protocol(format!("invalid guest protocol field `{name}`: {e}")))
}
pub fn decode<T: DeserializeOwned>(v: Value) -> Result<T, WireError> {
    serde_json::from_value(v).map_err(|e| WireError::protocol(format!("invalid guest protocol result: {e}")))
}
pub fn identity(v: &Value) -> Result<ProcessIdentity, WireError> {
    Ok(ProcessIdentity { pid: field(v, "pid")?, start_time: Timestamp(field(v, "start_time")?) })
}
pub fn identity_value(id: ProcessIdentity) -> Value {
    json!({"pid":id.pid,"start_time":id.start_time.0})
}
pub fn exit(v: Value) -> Result<Exit, WireError> {
    Ok(Exit { code: field(&v, "code")?, signal: field(&v, "signal")? })
}
pub fn exit_value(e: Exit) -> Value {
    json!({"code":e.code,"signal":e.signal})
}
pub fn spawn_value(s: SpawnSpec) -> Value {
    json!({"argv":s.argv,"cwd":s.cwd,"env":s.env,"stdin":s.stdin,"timeout_ms":s.timeout_ms})
}
pub fn spawn(v: &Value) -> Result<SpawnSpec, WireError> {
    Ok(SpawnSpec {
        argv: field(v, "argv")?,
        cwd: field(v, "cwd")?,
        env: field(v, "env")?,
        stdin: field(v, "stdin")?,
        timeout_ms: field(v, "timeout_ms")?,
    })
}
pub fn open_value(s: OpenSpec) -> Value {
    json!({"argv":s.argv,"cwd":s.cwd,"env":s.env,"cols":s.size.cols,"rows":s.size.rows})
}
pub fn open(v: &Value) -> Result<OpenSpec, WireError> {
    Ok(OpenSpec { argv: field(v, "argv")?, cwd: field(v, "cwd")?, env: field(v, "env")?, size: size(v)? })
}
pub fn size(v: &Value) -> Result<Size, WireError> {
    Ok(Size::new(field(v, "cols")?, field(v, "rows")?)?)
}
pub fn read_value(r: ReadResult) -> Value {
    json!({"text":r.text,"version":r.version.0,"total_lines":r.total_lines,"partial":r.partial})
}
pub fn read_result(v: Value) -> Result<ReadResult, WireError> {
    Ok(ReadResult {
        text: field(&v, "text")?,
        version: FileVersion(field(&v, "version")?),
        total_lines: field(&v, "total_lines")?,
        partial: field(&v, "partial")?,
    })
}
pub fn stat_value(s: Stat) -> Value {
    json!({"path":s.path,"is_dir":s.is_dir,"bytes":s.bytes,"version":s.version.map(|v|v.0)})
}
pub fn stat(v: Value) -> Result<Stat, WireError> {
    Ok(Stat {
        path: field(&v, "path")?,
        is_dir: field(&v, "is_dir")?,
        bytes: field(&v, "bytes")?,
        version: field::<Option<u64>>(&v, "version")?.map(FileVersion),
    })
}
pub fn diff_value(d: AppliedDiff) -> Value {
    json!({"path":d.path,"created":d.created,"hunks":d.hunks,"new_version":d.new_version.0})
}
pub fn diff(v: Value) -> Result<AppliedDiff, WireError> {
    Ok(AppliedDiff {
        path: field(&v, "path")?,
        created: field(&v, "created")?,
        hunks: field(&v, "hunks")?,
        new_version: FileVersion(field(&v, "new_version")?),
    })
}
pub fn hit_value(h: GrepHit) -> Value {
    json!({"path":h.path,"line":h.line,"text":h.text})
}
pub fn hit(v: Value) -> Result<GrepHit, WireError> {
    Ok(GrepHit { path: field(&v, "path")?, line: field(&v, "line")?, text: field(&v, "text")? })
}
pub fn snapshot_value(s: Snapshot) -> Value {
    json!({"screen":{"text":s.screen.text,"cursor_row":s.screen.cursor_row,"cursor_col":s.screen.cursor_col,"cols":s.screen.size.cols,"rows":s.screen.size.rows,"alternate":s.screen.alternate,"application_cursor":s.screen.application_cursor,"title":s.screen.title},"delta":{"text":s.delta.text,"next":s.delta.next,"dropped":s.delta.dropped},"seq":s.seq,"exit":s.exit.map(exit_value)})
}
pub fn snapshot(v: Value) -> Result<Snapshot, WireError> {
    let s: Value = field(&v, "screen")?;
    let d: Value = field(&v, "delta")?;
    Ok(Snapshot {
        screen: Screen {
            text: field(&s, "text")?,
            cursor_row: field(&s, "cursor_row")?,
            cursor_col: field(&s, "cursor_col")?,
            size: size(&s)?,
            alternate: field(&s, "alternate")?,
            application_cursor: field(&s, "application_cursor")?,
            title: field(&s, "title")?,
        },
        delta: Delta { text: field(&d, "text")?, next: field(&d, "next")?, dropped: field(&d, "dropped")? },
        seq: field(&v, "seq")?,
        exit: field::<Option<Value>>(&v, "exit")?.map(exit).transpose()?,
    })
}
pub fn closed_value(c: Closed) -> Value {
    json!({"killed":c.killed,"exit":c.exit.map(exit_value),"note":c.note})
}
pub fn closed(v: Value) -> Result<Closed, WireError> {
    Ok(Closed { killed: field(&v, "killed")?, exit: field::<Option<Value>>(&v, "exit")?.map(exit).transpose()?, note: field(&v, "note")? })
}
