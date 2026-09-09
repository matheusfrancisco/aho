#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::process::{Command, Stdio};
use std::time::Duration;

use daemon::ready::{ReadyError, spawn_with_ready};

const PROBE: &str = env!("CARGO_BIN_EXE_daemon-ready-probe");
const WAIT: Duration = Duration::from_secs(20);

fn probe(args: &[&str]) -> Command {
    let mut c = Command::new(PROBE);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped());
    c
}

fn pids(out: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(out).lines().map(|l| l.trim().parse().unwrap()).collect()
}

#[test]
fn a_child_that_signals_is_ready() {
    let child = spawn_with_ready(&mut probe(&["signal"]), WAIT).unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(pids(&out.stdout), vec![pid]);
}

#[test]
fn a_child_that_exits_first_reads_as_closed() {
    assert_eq!(spawn_with_ready(&mut probe(&["exit"]), WAIT).unwrap_err(), ReadyError::Closed);
}

#[test]
fn exec_in_place_hands_the_fd_to_the_new_image() {
    let child = spawn_with_ready(&mut probe(&["exec-into", PROBE]), WAIT).unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(pids(&out.stdout), vec![pid, pid], "both images print the same pid: exec replaced the process in place");
}

#[test]
fn a_silent_child_times_out() {
    let started = std::time::Instant::now();
    let err = spawn_with_ready(Command::new("sleep").arg("30"), Duration::from_millis(300)).unwrap_err();
    assert_eq!(err, ReadyError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(10), "the child is killed, not waited out");
}
