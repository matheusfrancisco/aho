//! Stores: in-memory and JSONL. Both are append-only; a fork is a head move (06).

use super::*;

// ───────────────────────────────────── store ─────────────────────────────────────

#[derive(Debug)]
pub enum LogError {
    ChainBroken {
        at: EntryId,
    },
    UnknownParent(EntryId),
    Io(String),
    /// A fold rule failed (approval outside a turn, a second result for one call, …). Never repaired.
    Invariant {
        rule: &'static str,
        at: EntryId,
        detail: String,
    },
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::ChainBroken { at } => write!(f, "hash chain broken at entry {}", at.0),
            LogError::UnknownParent(p) => write!(f, "unknown parent entry {}", p.0),
            LogError::Io(e) => write!(f, "log io: {e}"),
            LogError::Invariant { rule, at, detail } => write!(f, "invariant `{rule}` violated at entry {}: {detail}", at.0),
        }
    }
}

impl std::error::Error for LogError {}

/// The only mutation path into a session.
pub trait LogStore: Send + Sync {
    /// Appends a child of `parent` (or of the current head when `None`) and moves the head to it.
    fn append(&self, parent: Option<EntryId>, by: Principal, kind: EntryKind) -> Result<Entry, LogError>;
    fn head(&self) -> Option<EntryId>;
    fn heads(&self) -> Vec<EntryId>;
    /// Entries on the branch ending at `head`, oldest first.
    fn read_branch(&self, head: EntryId) -> Result<Vec<Entry>, LogError>;
    /// Entries appended after `from` (any branch), in append order. For `subscribe_from`.
    fn read_from(&self, from: Option<EntryId>) -> Vec<Entry>;
    /// Moves the head to `at` so the next append branches from it. Nothing copied.
    fn fork(&self, at: EntryId) -> Result<(), LogError>;
    fn get(&self, id: EntryId) -> Option<Entry>;
    /// Last `SessionMeta` on the file, for cheap session listing.
    fn tail_meta(&self) -> Option<Entry>;
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    index: BTreeMap<EntryId, usize>,
    head: Option<EntryId>,
}

impl Inner {
    fn replay_line(&mut self, line: &str) -> Result<(), LogError> {
        if line.trim().is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<Line>(line).map_err(|e| LogError::Io(e.to_string()))? {
            Line::Entry(e) => {
                if !e.verify() {
                    return Err(LogError::ChainBroken { at: e.id });
                }
                let id = e.id;
                self.index.insert(id, self.entries.len());
                self.entries.push(*e);
                self.head = Some(id);
            }
            Line::Head { head } => self.head = Some(head),
        }
        Ok(())
    }

    fn push(&mut self, parent: Option<EntryId>, by: Principal, kind: EntryKind, at: Timestamp) -> Result<Entry, LogError> {
        let parent = parent.or(self.head);
        let prev_hash = match parent {
            Some(p) => self.index.get(&p).map(|&i| self.entries[i].hash).ok_or(LogError::UnknownParent(p))?,
            None => Hash::ZERO,
        };
        let id = EntryId(self.entries.last().map(|e| e.id.0 + 1).unwrap_or(1));
        let hash = Entry::body_hash(&prev_hash, id, parent, at, &by, &kind);
        let e = Entry { id, parent, prev_hash, hash, at, by, kind };
        self.index.insert(id, self.entries.len());
        self.entries.push(e.clone());
        self.head = Some(id);
        Ok(e)
    }
    fn branch(&self, head: EntryId) -> Result<Vec<Entry>, LogError> {
        let mut out = Vec::new();
        let mut cur = Some(head);
        while let Some(id) = cur {
            let e = self.index.get(&id).map(|&i| &self.entries[i]).ok_or(LogError::UnknownParent(id))?;
            if !e.verify() {
                return Err(LogError::ChainBroken { at: id });
            }
            if let Some(p) = e.parent {
                let parent_hash = self.index.get(&p).map(|&i| self.entries[i].hash).ok_or(LogError::UnknownParent(p))?;
                if parent_hash != e.prev_hash {
                    return Err(LogError::ChainBroken { at: id });
                }
            }
            out.push(e.clone());
            cur = e.parent;
        }
        out.reverse();
        Ok(out)
    }
    fn heads(&self) -> Vec<EntryId> {
        let parents: BTreeSet<EntryId> = self.entries.iter().filter_map(|e| e.parent).collect();
        self.entries.iter().map(|e| e.id).filter(|id| !parents.contains(id)).collect()
    }
}

/// In-memory store (tests, subagents that do not persist).
#[derive(Default, Clone)]
pub struct MemoryLog {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryLog {
    pub fn new() -> Self {
        Self::default()
    }
    /// Decode a captured JSONL session without acquiring a host file. Shares entry verification
    /// and persisted fork-head semantics with `JsonlLog::open`; `read_branch` verifies linkage.
    pub fn from_jsonl(text: &str) -> Result<Self, LogError> {
        let mut inner = Inner::default();
        for line in text.lines() {
            inner.replay_line(line)?;
        }
        Ok(Self { inner: Arc::new(Mutex::new(inner)) })
    }
}

fn lock(m: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl LogStore for MemoryLog {
    fn append(&self, parent: Option<EntryId>, by: Principal, kind: EntryKind) -> Result<Entry, LogError> {
        lock(&self.inner).push(parent, by, kind, Timestamp::now())
    }
    fn head(&self) -> Option<EntryId> {
        lock(&self.inner).head
    }
    fn heads(&self) -> Vec<EntryId> {
        lock(&self.inner).heads()
    }
    fn read_branch(&self, head: EntryId) -> Result<Vec<Entry>, LogError> {
        lock(&self.inner).branch(head)
    }
    fn read_from(&self, from: Option<EntryId>) -> Vec<Entry> {
        let g = lock(&self.inner);
        g.entries.iter().filter(|e| from.map(|f| e.id > f).unwrap_or(true)).cloned().collect()
    }
    fn fork(&self, at: EntryId) -> Result<(), LogError> {
        let mut g = lock(&self.inner);
        if !g.index.contains_key(&at) {
            return Err(LogError::UnknownParent(at));
        }
        g.head = Some(at);
        Ok(())
    }
    fn get(&self, id: EntryId) -> Option<Entry> {
        let g = lock(&self.inner);
        g.index.get(&id).map(|&i| g.entries[i].clone())
    }
    fn tail_meta(&self) -> Option<Entry> {
        lock(&self.inner).entries.iter().rev().find(|e| matches!(e.kind, EntryKind::SessionMeta { .. })).cloned()
    }
}

/// One JSONL file per session, append-only. A `{"head": id}` line records fork pointer moves so a
/// reopened log resumes on the same branch; nothing is ever rewritten.
pub struct JsonlLog {
    path: PathBuf,
    mem: MemoryLog,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    Entry(Box<Entry>),
    Head { head: EntryId },
}

impl JsonlLog {
    #[allow(clippy::disallowed_methods, reason = "authority: jsonl-open")]
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, LogError> {
        let path = path.into();
        let mem = MemoryLog::new();
        if path.exists() {
            let f = std::fs::File::open(&path).map_err(|e| LogError::Io(e.to_string()))?;
            let mut g = lock(&mem.inner);
            for line in std::io::BufReader::new(f).lines() {
                let line = line.map_err(|e| LogError::Io(e.to_string()))?;
                g.replay_line(&line)?;
            }
        } else if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| LogError::Io(e.to_string()))?;
        }
        Ok(JsonlLog { path, mem })
    }
    #[allow(clippy::disallowed_methods, reason = "authority: jsonl-append")]
    fn write_line(&self, line: &Line) -> Result<(), LogError> {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).map_err(|e| LogError::Io(e.to_string()))?;
        let mut s = serde_json::to_string(line).map_err(|e| LogError::Io(e.to_string()))?;
        s.push('\n');
        f.write_all(s.as_bytes()).map_err(|e| LogError::Io(e.to_string()))
    }
}

impl LogStore for JsonlLog {
    fn append(&self, parent: Option<EntryId>, by: Principal, kind: EntryKind) -> Result<Entry, LogError> {
        let mut g = lock(&self.mem.inner);
        let e = g.push(parent, by, kind, Timestamp::now())?;
        self.write_line(&Line::Entry(Box::new(e.clone())))?;
        Ok(e)
    }
    fn head(&self) -> Option<EntryId> {
        self.mem.head()
    }
    fn heads(&self) -> Vec<EntryId> {
        self.mem.heads()
    }
    fn read_branch(&self, head: EntryId) -> Result<Vec<Entry>, LogError> {
        self.mem.read_branch(head)
    }
    fn read_from(&self, from: Option<EntryId>) -> Vec<Entry> {
        self.mem.read_from(from)
    }
    fn fork(&self, at: EntryId) -> Result<(), LogError> {
        self.mem.fork(at)?;
        self.write_line(&Line::Head { head: at })
    }
    fn get(&self, id: EntryId) -> Option<Entry> {
        self.mem.get(id)
    }
    fn tail_meta(&self) -> Option<Entry> {
        self.mem.tail_meta()
    }
}
