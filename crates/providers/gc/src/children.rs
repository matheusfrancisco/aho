//! Children a session started (jobs, viewers, sandboxes) and the owner that started them (12:
//! "`aho gc` reaps children orphaned by a dead identity"; crash-honest receipts).
//!
//! A child is known from its receipt, `Custom{ns: "Child", name: "Spawned", body: {pid,
//! start_time, kind, label}}`, appended by the owner's operations table as `system:ops` when the
//! child starts (`aho_proc::spawned`, read back with `aho_proc::child_receipt`); receipts by any
//! other principal (a tool's `value.events`, say) are ignored. The owner is the identity of the
//! `Session/Meta` in force at that point of the log. Identities are `(pid, start_time)` — pids
//! recycle, the pair does not — probed with `ps`. An owner is alive when its pid runs and started
//! no later than recorded (the meta is written after boot); a child only when its pid started
//! within a few seconds of the recorded time, since the receipt is written as it starts.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use aho_log::ProcessIdentity;
use aho_types::Timestamp;

use crate::logs::Sessions;
use crate::{GcReport, Kept, Orphan, OrphanKind, Pending, Target};

/// `ps` reports elapsed time in whole seconds and the identity is recorded a moment after the
/// process started: a process that started later than this after the recorded time is another
/// process that reuses the pid.
const START_SLACK_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The pid runs and started when the identity says.
    Alive,
    /// No such pid, a zombie, or a process that started later (the pid was recycled).
    Gone,
    /// `ps` could not answer; nothing is judged on it.
    Unknown,
}

/// Probes a session owner `(pid, start_time)` with `ps -o stat=,etime= -p <pid>`: alive when the
/// pid runs and started no later than recorded (plus slack).
pub fn liveness(id: &ProcessIdentity) -> Liveness {
    probe(id, false)
}

/// Probes a child: alive only when the pid started within the slack of the recorded time, so a
/// receipt cannot point gc at an unrelated older process.
pub fn child_liveness(id: &ProcessIdentity) -> Liveness {
    probe(id, true)
}

#[allow(clippy::disallowed_methods, reason = "authority: gc-process-probe")]
fn probe(id: &ProcessIdentity, exact: bool) -> Liveness {
    let out = match Command::new("ps").args(["-o", "stat=,etime=", "-p", &id.pid.to_string()]).output() {
        Ok(o) => o,
        Err(_) => return Liveness::Unknown,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut words = text.split_whitespace();
    let (Some(stat), Some(etime)) = (words.next(), words.next()) else {
        return if out.status.success() { Liveness::Unknown } else { Liveness::Gone };
    };
    if stat.starts_with('Z') {
        return Liveness::Gone;
    }
    let Some(elapsed_s) = parse_etime(etime) else { return Liveness::Unknown };
    let started = Timestamp::now().0.saturating_sub(elapsed_s.saturating_mul(1000));
    let late = started > id.start_time.0.saturating_add(START_SLACK_MS);
    let early = exact && started.saturating_add(START_SLACK_MS) < id.start_time.0;
    if late || early { Liveness::Gone } else { Liveness::Alive }
}

/// `[[dd-]hh:]mm:ss` → seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut secs = 0u64;
    for part in rest.split(':') {
        secs = secs.checked_mul(60)?.checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(days * 86_400 + secs)
}

/// One scan's liveness answers, probed once per identity; remembers which owners were gone.
#[derive(Default)]
pub(crate) struct Prober {
    owners: HashMap<ProcessIdentity, Liveness>,
    children: HashMap<ProcessIdentity, Liveness>,
    owners_gone: Vec<ProcessIdentity>,
}

impl Prober {
    pub(crate) fn child(&mut self, id: ProcessIdentity) -> Liveness {
        *self.children.entry(id).or_insert_with(|| child_liveness(&id))
    }

    /// Probes a session owner; gone owners are what the next scan confirms against.
    pub(crate) fn owner(&mut self, id: ProcessIdentity) -> Liveness {
        let l = *self.owners.entry(id).or_insert_with(|| liveness(&id));
        if l == Liveness::Gone && !self.owners_gone.contains(&id) {
            self.owners_gone.push(id);
        }
        l
    }

    pub(crate) fn owners_gone(&self) -> Vec<ProcessIdentity> {
        let mut v = self.owners_gone.clone();
        v.sort_by_key(|i| (i.pid, i.start_time.0));
        v
    }
}

/// Children still running whose owner was gone on this scan and on the previous one.
pub(crate) fn judge(sessions: &Sessions, previous: &[ProcessIdentity], now: Timestamp, prober: &mut Prober, report: &mut GcReport) {
    for log in &sessions.logs {
        for rc in &log.receipts {
            let shown = format!("pid {} {} {}", rc.child.pid, rc.kind, rc.label).trim_end().to_string();
            let Some(owner) = rc.owner else {
                report.kept.push(Kept {
                    kind: OrphanKind::Child,
                    path: shown,
                    why: "its receipt precedes every Session/Meta of the log, so its owner is unknown".into(),
                });
                continue;
            };
            if prober.owner(owner) != Liveness::Gone {
                continue;
            }
            match prober.child(rc.child) {
                Liveness::Gone => continue,
                Liveness::Unknown => {
                    report.kept.push(Kept {
                        kind: OrphanKind::Child,
                        path: shown,
                        why: "`ps` could not tell whether it still runs".into(),
                    });
                    continue;
                }
                Liveness::Alive => {}
            }
            if !previous.contains(&owner) {
                match report.pending.iter_mut().find(|p| p.identity == owner && p.owner == log.label) {
                    Some(p) => p.children += 1,
                    None => report.pending.push(Pending { owner: log.label.clone(), identity: owner, children: 1 }),
                }
                continue;
            }
            report.items.push(Orphan {
                owner: log.label.clone(),
                kind: OrphanKind::Child,
                age_ms: Some(now.0.saturating_sub(rc.child.start_time.0)),
                size: None,
                path: shown,
                target: Target::Child { child: rc.child, owner },
            });
        }
    }
}

/// Stops a child that is still the recorded process: SIGTERM to its process group (world-host
/// starts every child as a group leader), SIGKILL after `grace_ms`.
pub(crate) fn reap(child: &ProcessIdentity, grace_ms: u64) -> Result<(), String> {
    match child_liveness(child) {
        Liveness::Alive => {}
        Liveness::Gone => return Err(format!("pid {} exited before it was stopped; nothing to do", child.pid)),
        Liveness::Unknown => {
            return Err(format!(
                "could not tell whether pid {} is still the recorded process (`ps` did not answer); left it running",
                child.pid
            ));
        }
    }
    signal(child.pid, "TERM")?;
    if wait_gone(child, grace_ms) {
        return Ok(());
    }
    signal(child.pid, "KILL")?;
    if wait_gone(child, 1_000) {
        return Ok(());
    }
    Err(format!("pid {} still runs after SIGTERM and SIGKILL; stop it by hand (`kill -9 {}`)", child.pid, child.pid))
}

fn wait_gone(child: &ProcessIdentity, ms: u64) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        if child_liveness(child) == Liveness::Gone {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `kill -s <sig> -- -<pid>` (the group), falling back to the pid alone when it leads no group.
#[allow(clippy::disallowed_methods, reason = "authority: gc-process-signal")]
fn signal(pid: u32, sig: &str) -> Result<(), String> {
    let group = Command::new("kill").args(["-s", sig, "--", &format!("-{pid}")]).output();
    if matches!(&group, Ok(o) if o.status.success()) {
        return Ok(());
    }
    let one = Command::new("kill").args(["-s", sig, &pid.to_string()]).output().map_err(|e| format!("could not run kill ({e})"))?;
    if one.status.success() {
        return Ok(());
    }
    Err(format!("kill -{sig} {pid} failed: {}", String::from_utf8_lossy(&one.stderr).trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_formats_parse() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-00:00:01"), Some(172_801));
        assert_eq!(parse_etime("x"), None);
    }
}
