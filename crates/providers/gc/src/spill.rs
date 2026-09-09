//! Spill files (10: "spill, don't truncate"): `<session.dir>/<id>.spill/<blake3>.txt`, named by
//! the `Tool/Result.spill_ref` that points at them. Files are content-addressed and a child
//! session writes into its parent's directory, so a file is referenced when any log names its
//! hash. The directory of a session whose owner still runs is skipped whole: the session writes
//! the file before it appends the result that names it.

use std::collections::HashSet;

use aho_types::Hash;

use crate::children::{Liveness, Prober};
use crate::logs::Sessions;
use crate::{GcReport, Kept, Orphan, OrphanKind, ScanInputs, Target, age_of};

#[allow(clippy::disallowed_methods, reason = "authority: gc-spill-scan")]
pub(crate) fn judge(inputs: &ScanInputs, sessions: &Sessions, prober: &mut Prober, report: &mut GcReport) {
    let Ok(entries) = std::fs::read_dir(&inputs.session_dir) else { return };
    let mut dirs: Vec<_> =
        entries.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "spill")).collect();
    dirs.sort();
    if dirs.is_empty() {
        return;
    }
    if !sessions.unreadable.is_empty() {
        report.notes.push(format!(
            "spill files were not judged: {} session log(s) could not be read, and any of them may name a spill",
            sessions.unreadable.len()
        ));
        return;
    }
    let referenced: HashSet<Hash> = sessions.logs.iter().flat_map(|l| l.spills.iter().copied()).collect();
    for dir in dirs {
        let label = dir.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(id) = sessions.by_label(&label).and_then(|l| l.identity)
            && prober.owner(id) != Liveness::Gone
        {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<_> = files.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let is_spill = path.extension().is_some_and(|x| x == "txt") && path.is_file();
            let Some(hash) = path.file_stem().and_then(|s| Hash::from_hex(&s.to_string_lossy())).filter(|_| is_spill) else {
                continue;
            };
            if referenced.contains(&hash) {
                continue;
            }
            let Some(age) = age_of(&path, inputs.now) else { continue };
            if age < inputs.grace_ms {
                report.kept.push(Kept {
                    kind: OrphanKind::Spill,
                    path: path.display().to_string(),
                    why: "written within the grace period (its result may be on its way into a log)".into(),
                });
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).ok();
            report.items.push(Orphan {
                owner: label.clone(),
                kind: OrphanKind::Spill,
                age_ms: Some(age),
                size,
                path: path.display().to_string(),
                target: Target::Spill { path: path.clone() },
            });
        }
    }
}
