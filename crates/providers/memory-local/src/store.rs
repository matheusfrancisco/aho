//! One JSON file per scope: `<home>/.aho/memory/<blake3(project) short>.json` and
//! `<home>/.aho/memory/global.json`. Writes go to a sibling temp file and are renamed into place.

use std::path::{Path, PathBuf};

use aho_memory::{Entry, MemoryError, Scope};
use aho_types::Hash;
use serde::{Deserialize, Serialize};

/// Global-store ids start here so ids never collide across the two files.
pub const GLOBAL_BASE: u64 = 1 << 40;
/// Synthetic (skill) ids live above both stores and are never persisted.
pub const SKILL_BASE: u64 = 1 << 41;
const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePaths {
    pub project: PathBuf,
    pub global: PathBuf,
}

impl StorePaths {
    pub fn new(home: &Path, project: &str) -> StorePaths {
        let dir = home.join(".aho").join("memory");
        StorePaths { project: dir.join(format!("{}.json", Hash::of(project.as_bytes()).short())), global: dir.join("global.json") }
    }
}

pub fn scope_of(id: u64) -> Option<Scope> {
    match id {
        0 => None,
        id if id < GLOBAL_BASE => Some(Scope::Project),
        id if id < SKILL_BASE => Some(Scope::Global),
        _ => None,
    }
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    #[serde(default)]
    project: Option<String>,
    next_id: u64,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    project: Option<&'a str>,
    next_id: u64,
    entries: &'a [Entry],
}

pub struct Store {
    path: PathBuf,
    scope: Scope,
    project: Option<String>,
    next_id: u64,
    pub entries: Vec<Entry>,
    pub dirty: bool,
}

impl Store {
    #[allow(clippy::disallowed_methods, reason = "authority: memory-local-store-open")]
    pub fn open(path: PathBuf, scope: Scope, project: Option<String>) -> Result<Store, MemoryError> {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let f: FileIn = serde_json::from_slice(&bytes).map_err(|e| {
                    MemoryError::Store(format!(
                        "`{}` is not a valid memory store ({e}); aho left it untouched. Repair it or move it aside to start an empty store",
                        path.display()
                    ))
                })?;
                if f.version != VERSION {
                    return Err(MemoryError::Store(format!(
                        "`{}` has store version {} and this aho reads version {VERSION}; aho left it untouched. Upgrade aho or move the file aside",
                        path.display(),
                        f.version
                    )));
                }
                let mut entries = f.entries;
                for e in &mut entries {
                    e.scope = scope;
                    e.skill = None;
                }
                Ok(Store { path, scope, project: f.project.or(project), next_id: f.next_id, entries, dirty: false })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Store { path, scope, project, next_id: 0, entries: Vec::new(), dirty: false })
            }
            Err(e) => {
                Err(MemoryError::Store(format!("cannot read `{}` ({e}); check its permissions or set `memory.home`", path.display())))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn alloc(&mut self) -> u64 {
        self.next_id += 1;
        match self.scope {
            Scope::Project => self.next_id,
            Scope::Global => GLOBAL_BASE + self.next_id,
        }
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Writes the file if anything changed since the last save.
    #[allow(clippy::disallowed_methods, reason = "authority: memory-local-store-flush")]
    pub fn flush(&mut self) -> Result<(), MemoryError> {
        if !self.dirty {
            return Ok(());
        }
        let fail = |what: &str, e: std::io::Error| {
            MemoryError::Store(format!("cannot {what} `{}` ({e}); the change is kept in memory for this session only", self.path.display()))
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| fail("create the directory of", e))?;
        }
        let out = FileOut { version: VERSION, project: self.project.as_deref(), next_id: self.next_id, entries: &self.entries };
        let bytes = serde_json::to_vec(&out).map_err(|e| MemoryError::Store(format!("cannot encode the memory store: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| fail("write", e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| fail("replace", e))?;
        self.dirty = false;
        Ok(())
    }
}
