//! The two-scan rule's memory (12: "an identity must be missing on two consecutive scans before
//! anything is marked gone"): the owner identities the last scan found gone, in a small JSON file.

use std::path::Path;

use aho_log::ProcessIdentity;
use aho_types::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScanState {
    scanned_at: u64,
    missing: Vec<ProcessIdentity>,
}

/// Owners the previous scan found gone. No file yet means no previous scan.
#[allow(clippy::disallowed_methods, reason = "authority: gc-state-load")]
pub(crate) fn load(path: &Path) -> Result<Vec<ProcessIdentity>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("could not read the previous scan {} ({e}); this scan counts as the first", path.display())),
    };
    serde_json::from_str::<ScanState>(&text)
        .map(|s| s.missing)
        .map_err(|e| format!("the previous scan {} is unreadable ({e}); this scan counts as the first and overwrites it", path.display()))
}

/// Replaces the file atomically (write + rename), creating its directory.
#[allow(clippy::disallowed_methods, reason = "authority: gc-state-save")]
pub(crate) fn save(path: &Path, now: Timestamp, missing: &[ProcessIdentity]) -> Result<(), String> {
    let fail =
        |e: std::io::Error| format!("could not record this scan in {} ({e}); the next scan will count as a first scan", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let body = serde_json::to_string_pretty(&ScanState { scanned_at: now.0, missing: missing.to_vec() })
        .map_err(|e| format!("could not encode the scan state ({e})"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}
