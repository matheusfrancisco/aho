//! `aho plan --lock` / `aho run --lock`: the pinned plan and the inputs it came from.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use aho_types::Hash;
use serde::{Deserialize, Serialize};

use crate::inputs::{read_optional, write_atomic};
use crate::{ComposeError, Planned};

pub const LOCK_FILE: &str = "aho.lock";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LockFile {
    pub hash: Hash,
    pub fingerprint: Hash,
    pub schema_hash: Hash,
    pub loadout_hash: Hash,
    /// Digest of every config file that fed the loadout, by path.
    pub inputs: BTreeMap<String, Hash>,
}

impl LockFile {
    pub fn of(p: &Planned) -> LockFile {
        LockFile {
            hash: p.plan.hash,
            fingerprint: p.plan.fingerprint,
            schema_hash: p.loadout.schema_hash,
            loadout_hash: p.loadout.hash,
            inputs: p.loadout.inputs.iter().cloned().collect(),
        }
    }

    pub fn path(project: &Path) -> PathBuf {
        project.join(LOCK_FILE)
    }

    pub fn write(&self, project: &Path) -> Result<PathBuf, ComposeError> {
        let path = LockFile::path(project);
        let body = toml::to_string_pretty(self).map_err(|e| ComposeError::Lock { path: path.clone(), err: e.to_string() })?;
        let text = format!("# Written by `aho plan --lock`; `aho run --lock` refuses to start when the plan differs.\n{body}");
        write_atomic(&path, &text).map_err(|err| ComposeError::Lock { path: path.clone(), err })?;
        Ok(path)
    }

    pub fn read(project: &Path) -> Result<LockFile, ComposeError> {
        let path = LockFile::path(project);
        let text = read_optional(&path)?.ok_or_else(|| ComposeError::Lock { path: path.clone(), err: "no lock file".into() })?;
        toml::from_str(&text).map_err(|e| ComposeError::Lock { path, err: format!("unreadable lock: {e}") })
    }

    /// Refuses a plan that differs from the pinned one, naming what moved.
    pub fn verify(&self, p: &Planned) -> Result<(), ComposeError> {
        if p.plan.hash == self.hash {
            return Ok(());
        }
        let now = LockFile::of(p);
        let mut causes = Vec::new();
        if now.fingerprint != self.fingerprint {
            causes.push(format!(
                "the binary's capability set changed (fingerprint {} → {})",
                self.fingerprint.short(),
                now.fingerprint.short()
            ));
        }
        if now.schema_hash != self.schema_hash {
            causes.push("the config schema changed".into());
        }
        let paths: BTreeSet<&String> = self.inputs.keys().chain(now.inputs.keys()).collect();
        for path in paths {
            match (self.inputs.get(path), now.inputs.get(path)) {
                (Some(a), Some(b)) if a != b => causes.push(format!("{path} changed")),
                (Some(_), None) => causes.push(format!("{path} is gone")),
                (None, Some(_)) => causes.push(format!("{path} is new")),
                _ => {}
            }
        }
        if causes.is_empty() {
            causes.push(if now.loadout_hash != self.loadout_hash {
                "resolved settings changed (environment, flags, or defaults)".into()
            } else {
                "capability states changed (binaries, credentials, platform, or discovered capabilities)".into()
            });
        }
        Err(ComposeError::Drift { expected: self.hash, actual: p.plan.hash, causes })
    }
}
