//! Killing a terminal's whole process tree.
//!
//! The program is a session leader (setsid). An interactive shell puts every background job in
//! its own process group inside that session, and a double-forked process (`(cmd &)`) is
//! reparented away from the leader but stays in the session. So the tree is: every process whose
//! session id is the leader's pid, plus descendants (by parent pid) of any of them that moved to
//! another session. Hang-up first (SIGHUP + SIGCONT, so stopped jobs see it), then SIGKILL for
//! whatever is left, re-listing in between to catch processes forked during the kill.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
#[cfg(not(target_os = "linux"))]
use nix::unistd::getsid;
use nix::unistd::{Pid, getpgrp, getpid};

/// Grace between the hang-up and the SIGKILL round.
const HUP_GRACE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: Option<i32>,
}

#[derive(Debug, Default)]
pub struct Kill {
    pub killed: Vec<u32>,
    pub note: Option<String>,
}

/// Every process the tree walk may consider.
pub fn list() -> Result<Vec<Proc>, String> {
    #[cfg(target_os = "linux")]
    {
        list_proc()
    }
    #[cfg(not(target_os = "linux"))]
    {
        list_ps()
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods, reason = "authority: pty-host-procfs")]
fn list_proc() -> Result<Vec<Proc>, String> {
    let dir = std::fs::read_dir("/proc").map_err(|e| format!("/proc is not readable ({e})"))?;
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        // A process may exit between readdir and read; skip it.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        // `pid (comm) state ppid pgrp session …`; comm may contain spaces and parens.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if let (Some(ppid), Some(pgid), Some(sid)) = (f.get(1), f.get(2), f.get(3))
            && let (Ok(ppid), Ok(pgid), Ok(sid)) = (ppid.parse(), pgid.parse(), sid.parse())
        {
            out.push(Proc { pid, ppid, pgid, sid: Some(sid) });
        }
    }
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::disallowed_methods, reason = "authority: pty-host-ps")]
fn list_ps() -> Result<Vec<Proc>, String> {
    let ps = if std::path::Path::new("/bin/ps").exists() { "/bin/ps" } else { "ps" };
    let out = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("`ps` could not run ({e})"))?;
    if !out.status.success() {
        return Err(format!("`ps` failed ({})", out.status));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout), |pid| getsid(Some(Pid::from_raw(pid))).ok().map(|s| s.as_raw())))
}

/// `pid ppid pgid` per line; the session id comes from `getsid` (macOS `ps` has no sid column).
#[cfg(any(not(target_os = "linux"), test))]
pub fn parse_ps(text: &str, sid_of: impl Fn(i32) -> Option<i32>) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<i32> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            match f[..] {
                [pid, ppid, pgid] => Some(Proc { pid, ppid, pgid, sid: sid_of(pid) }),
                _ => None,
            }
        })
        .collect()
}

/// The leader's tree within `procs`: its session plus descendants of any member.
pub fn members(leader: i32, procs: &[Proc]) -> Vec<Proc> {
    let mut children: BTreeMap<i32, Vec<&Proc>> = BTreeMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut seen: BTreeSet<i32> = BTreeSet::new();
    let mut stack: Vec<&Proc> = procs.iter().filter(|p| p.pid == leader || p.sid == Some(leader)).collect();
    let mut out = Vec::new();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.pid) {
            continue;
        }
        out.push(*p);
        if let Some(kids) = children.get(&p.pid) {
            stack.extend(kids.iter().copied());
        }
    }
    out.sort_by_key(|p| (p.pid != leader, p.pid));
    out
}

/// Signals the leader's whole tree; never the calling process or its own group, never pid ≤ 1.
pub fn kill_tree(leader: u32) -> Kill {
    let Ok(leader) = i32::try_from(leader) else {
        return Kill { killed: vec![], note: Some(format!("pid {leader} is out of range; nothing was signalled")) };
    };
    if leader <= 1 {
        return Kill { killed: vec![], note: Some(format!("refused to signal pid {leader}")) };
    }
    let (me, my_group) = (getpid().as_raw(), getpgrp().as_raw());
    let mut killed: BTreeSet<i32> = BTreeSet::new();
    let mut note = None;
    let round = |sig: Signal, killed: &mut BTreeSet<i32>, note: &mut Option<String>| -> usize {
        let procs = match list() {
            Ok(p) => p,
            Err(e) => {
                *note = Some(format!("could not list processes ({e}); only the terminal's own process group was signalled"));
                vec![Proc { pid: leader, ppid: 0, pgid: leader, sid: Some(leader) }]
            }
        };
        let targets: Vec<Proc> = members(leader, &procs).into_iter().filter(|p| p.pid > 1 && p.pid != me).collect();
        let groups: BTreeSet<i32> = targets.iter().map(|p| p.pgid).filter(|g| *g > 1 && *g != my_group).collect();
        let mut refused = Vec::new();
        for g in &groups {
            let g = Pid::from_raw(*g);
            refused.extend(gone_is_fine(killpg(g, sig)).err().map(|e| format!("group {g}: {e}")));
            if sig == Signal::SIGHUP {
                refused.extend(gone_is_fine(killpg(g, Signal::SIGCONT)).err().map(|e| format!("group {g}: {e}")));
            }
        }
        for p in &targets {
            refused.extend(gone_is_fine(kill(Pid::from_raw(p.pid), sig)).err().map(|e| format!("pid {}: {e}", p.pid)));
            killed.insert(p.pid);
        }
        if !refused.is_empty() {
            *note = Some(format!("the host refused some signals ({})", refused.join(", ")));
        }
        targets.len()
    };
    if round(Signal::SIGHUP, &mut killed, &mut note) > 0 {
        let deadline = Instant::now() + HUP_GRACE;
        while Instant::now() < deadline && alive(&killed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    for _ in 0..3 {
        if round(Signal::SIGKILL, &mut killed, &mut note) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut killed: Vec<u32> = killed.into_iter().filter_map(|p| u32::try_from(p).ok()).collect();
    killed.sort_by_key(|p| (*p != leader as u32, *p));
    Kill { killed, note }
}

/// Any of `pids` still exists (a zombie counts until its parent reaps it).
fn alive(pids: &BTreeSet<i32>) -> bool {
    pids.iter().any(|p| kill(Pid::from_raw(*p), None).is_ok())
}

/// ESRCH (the target is already gone) counts as success for a kill.
fn gone_is_fine(r: nix::Result<()>) -> nix::Result<()> {
    match r {
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgid: i32, sid: i32) -> Proc {
        Proc { pid, ppid, pgid, sid: Some(sid) }
    }

    #[test]
    fn members_are_the_session_plus_descendants_that_left_it() {
        let procs = vec![
            p(1, 0, 1, 1),
            p(50, 1, 50, 50),      // unrelated session
            p(100, 50, 100, 100),  // leader
            p(101, 100, 101, 100), // background job, own group
            p(102, 1, 102, 100),   // double-forked: reparented to init, still in the session
            p(103, 101, 103, 103), // setsid'd grandchild: another session, still a descendant
            p(104, 103, 103, 103),
            p(200, 50, 200, 50),
        ];
        let got: Vec<i32> = members(100, &procs).iter().map(|p| p.pid).collect();
        assert_eq!(got, vec![100, 101, 102, 103, 104]);
    }

    #[test]
    fn ps_lines_parse_and_garbage_is_skipped() {
        let text = "  100     1   100\n  101   100   101\nbad line\n\n";
        let got = parse_ps(text, |pid| Some(if pid == 101 { 100 } else { pid }));
        assert_eq!(got, vec![p(100, 1, 100, 100), p(101, 100, 101, 100)]);
    }

    #[test]
    fn refuses_init_and_out_of_range_pids() {
        assert!(kill_tree(1).killed.is_empty());
        assert!(kill_tree(u32::MAX).killed.is_empty());
    }
}
