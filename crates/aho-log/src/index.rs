//! Rebuildable, root-bound cross-session projection. JSONL remains the source of truth.
//! Opening an existing index and querying it never enumerate session files. Initial creation,
//! explicit rebuild, and opening an individual writer are the reconciliation boundaries.

mod ingest;
mod query;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use aho_types::{EntryId, Principal, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Entry, EntryKind, JsonlLog, LogError, LogStore};
pub use query::{IndexHit, IndexPage, IndexQuery};

#[derive(Debug)]
pub struct IndexError(pub String);

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for IndexError {}
impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<serde_json::Error> for IndexError {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<LogError> for IndexError {
    fn from(e: LogError) -> Self {
        Self(e.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexIssue {
    /// Canonical hexadecimal session id, never a local entry id.
    pub session_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexReport {
    pub indexed_sessions: usize,
    pub indexed_entries: usize,
    pub issues: Vec<IndexIssue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexStatus {
    pub sessions_dir: PathBuf,
    pub indexed_sessions: usize,
    pub indexed_entries: usize,
    /// Unreadable, missing, or interrupted writers are excluded, not silently served stale.
    pub issues: Vec<IndexIssue>,
}

pub struct SessionIndex {
    root: PathBuf,
    db: Mutex<Connection>,
}

impl SessionIndex {
    /// Binds one database to exactly one canonical sessions directory. A new database is
    /// populated once; SQLite's immediate transaction serializes concurrent initializers.
    /// A damaged SQLite database is an error: rebuild into a new database from the JSONL logs.
    #[allow(clippy::disallowed_methods, reason = "authority: session-index-open")]
    pub fn open(database: impl AsRef<Path>, sessions_dir: impl AsRef<Path>) -> Result<Self, IndexError> {
        std::fs::create_dir_all(sessions_dir.as_ref())?;
        let root = std::fs::canonicalize(sessions_dir.as_ref())?;
        let mut db = Connection::open(database)?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        let bound: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='root'", [], |r| r.get(0)).optional()?;
        let root_text = root.to_str().ok_or_else(|| IndexError("session root must be UTF-8".into()))?;
        match bound {
            Some(bound) if bound != root_text => return Err(IndexError("index belongs to a different sessions directory".into())),
            None => {
                tx.execute("INSERT INTO metadata VALUES ('root', ?1)", [root_text])?;
            }
            _ => {}
        }
        let version: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='version'", [], |r| r.get(0)).optional()?;
        match version.as_deref() {
            None => {
                ingest::rebuild(&tx, &root)?;
                tx.execute("INSERT INTO metadata VALUES ('version', '1')", [])?;
            }
            Some("1") => {}
            Some(other) => return Err(IndexError(format!("unsupported session index version {other}; rebuild into a new database"))),
        }
        tx.commit()?;
        Ok(Self { root, db: Mutex::new(db) })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, IndexError> {
        self.db.lock().map_err(|_| IndexError("session index lock poisoned".into()))
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.root
    }

    /// The only whole-directory scan after first creation. Removed files disappear and corrupt
    /// sessions are quarantined independently; healthy sessions remain searchable.
    pub fn rebuild(&self) -> Result<IndexReport, IndexError> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let report = ingest::rebuild(&tx, &self.root)?;
        tx.commit()?;
        Ok(report)
    }

    /// Reconciles one source, including raw/external appends and fork-pointer moves. Missing or
    /// corrupt sources evict their old search rows and appear in the report instead.
    pub fn refresh_session(&self, id: SessionId) -> Result<IndexReport, IndexError> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let report = ingest::refresh(&tx, &self.root, id)?;
        tx.commit()?;
        Ok(report)
    }

    pub fn status(&self) -> Result<IndexStatus, IndexError> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let indexed_sessions = tx.query_row("SELECT count(*) FROM sessions WHERE error IS NULL", [], |r| r.get(0))?;
        let indexed_entries =
            tx.query_row("SELECT count(*) FROM entries e JOIN sessions s ON s.id=e.session_id WHERE s.error IS NULL", [], |r| r.get(0))?;
        let issues = issues(&tx)?;
        tx.commit()?;
        Ok(IndexStatus { sessions_dir: self.root.clone(), indexed_sessions, indexed_entries, issues })
    }

    /// Exact composite identity lookup, with no filesystem read. Unavailable sessions never
    /// return cached entries. The entry is raw operator data; model-facing callers must redact.
    pub fn get(&self, session: SessionId, entry: EntryId) -> Result<Option<Entry>, IndexError> {
        let db = self.lock()?;
        let json: Option<String> = db
            .query_row(
                "SELECT e.json FROM entries e JOIN sessions s ON s.id=e.session_id WHERE e.session_id=?1 AND e.id=?2 AND s.error IS NULL",
                params![hex(session), number(entry.0)?],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into)).transpose()
    }

    /// Opens a normal JSONL writer and synchronizes its index before returning it. Keep a
    /// single writer per session, as required by JsonlLog itself. Durable append failures after
    /// the log write explicitly say the log persisted; do not retry the append blindly.
    #[allow(clippy::disallowed_methods, reason = "authority: session-index-writer")]
    pub fn open_log(self: &Arc<Self>, id: SessionId) -> Result<IndexedLog, IndexError> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let path = self.root.join(format!("{}.jsonl", hex(id)));
        // Materialize a new empty source so it has the same refresh semantics as every other log.
        let _file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let report = ingest::refresh(&tx, &self.root, id)?;
        tx.commit()?;
        if let Some(issue) = report.issues.first() {
            return Err(IndexError(issue.reason.clone()));
        }
        let log = JsonlLog::open(path)?;
        Ok(IndexedLog { log, index: Arc::clone(self), id })
    }
}

/// Mutation-through adapter; reads preserve JsonlLog semantics exactly. A persistent dirty
/// marker precedes each source mutation so a crash cannot make stale results look current.
pub struct IndexedLog {
    log: JsonlLog,
    index: Arc<SessionIndex>,
    id: SessionId,
}

impl IndexedLog {
    #[allow(clippy::disallowed_methods, reason = "authority: session-index-sync")]
    fn sync_source(&self) -> Result<(), IndexError> {
        std::fs::File::open(self.index.root.join(format!("{}.jsonl", hex(self.id))))?.sync_all()?;
        Ok(())
    }

    fn mutate<T>(
        &self,
        write: impl FnOnce() -> Result<T, LogError>,
        update: impl FnOnce(&Connection, &T) -> Result<(), IndexError>,
    ) -> Result<T, LogError> {
        let run = || -> Result<T, IndexError> {
            let mut db = self.index.lock()?;
            db.execute("UPDATE sessions SET error='writer interrupted; refresh this session' WHERE id=?1", [hex(self.id)])?;
            let value = write()?;
            self.sync_source()
                .map_err(|e| IndexError(format!("log mutation persisted but source sync failed: {e}; refresh session {}", hex(self.id))))?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            update(&tx, &value).map_err(|e| {
                IndexError(format!("log mutation persisted but index update failed: {e}; refresh session {} before retrying", hex(self.id)))
            })?;
            tx.execute("UPDATE sessions SET error=NULL WHERE id=?1", [hex(self.id)])?;
            tx.commit().map_err(|e| {
                IndexError(format!("log mutation persisted but index commit failed: {e}; refresh session {}", hex(self.id)))
            })?;
            Ok(value)
        };
        run().map_err(|e| LogError::Io(e.to_string()))
    }
}

impl LogStore for IndexedLog {
    fn append(&self, parent: Option<EntryId>, by: Principal, kind: EntryKind) -> Result<Entry, LogError> {
        self.mutate(
            || self.log.append(parent, by, kind),
            |db, entry| {
                let old_head: Option<i64> = db.query_row("SELECT head FROM sessions WHERE id=?1", [hex(self.id)], |r| r.get(0))?;
                ingest::insert(db, &hex(self.id), entry)?;
                if entry.parent.map(|id| number(id.0)).transpose()? == old_head {
                    db.execute("UPDATE entries SET current=1 WHERE session_id=?1 AND id=?2", params![hex(self.id), number(entry.id.0)?])?;
                    db.execute("UPDATE sessions SET head=?2 WHERE id=?1", params![hex(self.id), number(entry.id.0)?])?;
                } else {
                    ingest::set_head(db, &hex(self.id), Some(entry.id))?;
                }
                Ok(())
            },
        )
    }
    fn fork(&self, at: EntryId) -> Result<(), LogError> {
        self.mutate(|| self.log.fork(at), |db, ()| ingest::set_head(db, &hex(self.id), Some(at)))
    }
    fn head(&self) -> Option<EntryId> {
        self.log.head()
    }
    fn heads(&self) -> Vec<EntryId> {
        self.log.heads()
    }
    fn read_branch(&self, head: EntryId) -> Result<Vec<Entry>, LogError> {
        self.log.read_branch(head)
    }
    fn read_from(&self, from: Option<EntryId>) -> Vec<Entry> {
        self.log.read_from(from)
    }
    fn get(&self, id: EntryId) -> Option<Entry> {
        self.log.get(id)
    }
    fn tail_meta(&self) -> Option<Entry> {
        self.log.tail_meta()
    }
}

fn hex(id: SessionId) -> String {
    format!("{:x}", id.0)
}
fn number(n: u64) -> Result<i64, IndexError> {
    i64::try_from(n).map_err(|_| IndexError("entry id or timestamp exceeds SQLite's signed integer range".into()))
}
fn issues(db: &Connection) -> Result<Vec<IndexIssue>, IndexError> {
    let mut stmt = db.prepare("SELECT id,error FROM sessions WHERE error IS NOT NULL ORDER BY id")?;
    Ok(stmt.query_map([], |r| Ok(IndexIssue { session_id: r.get(0)?, reason: r.get(1)? }))?.collect::<Result<_, _>>()?)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, head INTEGER, error TEXT);
CREATE TABLE IF NOT EXISTS entries (
    row_id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id INTEGER NOT NULL, parent INTEGER, hash TEXT NOT NULL, at INTEGER NOT NULL,
    kind TEXT NOT NULL, tool TEXT, call_id TEXT, current INTEGER NOT NULL DEFAULT 0,
    text TEXT NOT NULL, json TEXT NOT NULL, UNIQUE(session_id,id)
);
CREATE INDEX IF NOT EXISTS entry_kind ON entries(kind,at);
CREATE INDEX IF NOT EXISTS entry_tool ON entries(tool,at);
CREATE INDEX IF NOT EXISTS entry_call ON entries(session_id,call_id);
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(text, content='entries', content_rowid='row_id', tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS search_insert AFTER INSERT ON entries BEGIN
    INSERT INTO search(rowid,text) VALUES (new.row_id,new.text);
END;
CREATE TRIGGER IF NOT EXISTS search_delete AFTER DELETE ON entries BEGIN
    INSERT INTO search(search,rowid,text) VALUES ('delete',old.row_id,old.text);
END;
";
