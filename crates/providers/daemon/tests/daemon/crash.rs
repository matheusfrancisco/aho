#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::process::Command;

use aho_log::{CustomFolds, EntryKind, JsonlLog, LogStore, OpKind, ProcessIdentity, fold};
use aho_types::*;
use daemon::crash::{CrashScanner, candidate, identity_alive, mark_crashed, resume_crashed};
use serde_json::json;

fn id(pid: u32) -> ProcessIdentity {
    ProcessIdentity { pid, start_time: Timestamp(1_000) }
}

#[test]
fn an_identity_must_be_missing_on_two_consecutive_scans() {
    let dead = |_: &ProcessIdentity| false;
    let alive = |_: &ProcessIdentity| true;
    let mut s = CrashScanner::new();
    let one = [(SessionId(1), id(10))];
    assert!(s.scan(&one, &dead).is_empty(), "missing once is a flap");
    assert!(s.scan(&one, &alive).is_empty());
    assert!(s.scan(&one, &dead).is_empty(), "a live sighting in between resets the count");
    assert_eq!(s.scan(&one, &dead), vec![(SessionId(1), id(10))]);
    assert!(s.scan(&one, &dead).is_empty(), "a confirmed crash is reported once");

    let (x, y) = ([(SessionId(2), id(11))], [(SessionId(2), id(12))]);
    assert!(s.scan(&x, &dead).is_empty());
    assert!(s.scan(&y, &dead).is_empty(), "a changed identity starts over");
    assert_eq!(s.scan(&y, &dead), vec![(SessionId(2), id(12))]);

    let z = [(SessionId(3), id(13))];
    assert!(s.scan(&z, &dead).is_empty());
    assert!(s.scan(&[], &dead).is_empty());
    assert!(s.scan(&z, &dead).is_empty(), "dropping out of the candidates resets the count");
}

fn exited_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn liveness_is_by_pid_and_start_time() {
    let me = ProcessIdentity { pid: std::process::id(), start_time: Timestamp::now() };
    assert!(identity_alive(&me));
    let recycled = ProcessIdentity { pid: std::process::id(), start_time: Timestamp(1_000) };
    assert!(!identity_alive(&recycled), "our pid, but our process started long after that identity");
    let gone = ProcessIdentity { pid: exited_pid(), start_time: Timestamp::now() };
    assert!(!identity_alive(&gone));
}

fn call(log: &JsonlLog, call: &str, tool: &str, class: Consequence, op: u64, rerunnable: bool, external_effect: bool) {
    let by = Principal::system("loop");
    let c = CallId(call.into());
    log.append(None, by.clone(), EntryKind::ToolCall { call: c.clone(), tool: tool.into(), args: json!({}), class }).unwrap();
    log.append(None, by, EntryKind::OpStarted { op: OpId(op), call: c, kind: OpKind::Process, rerunnable, external_effect }).unwrap();
}

#[test]
fn a_crashed_session_forks_at_the_last_balanced_turn_and_names_the_uncertain_tools() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("5.jsonl");
    let dead = ProcessIdentity { pid: exited_pid(), start_time: Timestamp::now() };
    let me = ProcessIdentity { pid: std::process::id(), start_time: Timestamp::now() };
    let balanced = {
        let log = JsonlLog::open(&path).unwrap();
        let a = |k: EntryKind| log.append(None, Principal::system("loop"), k).unwrap().id;
        a(EntryKind::SessionMeta { title: None, tags: vec![], last_prompt: None, process: Some(dead) });
        a(EntryKind::TurnStart { turn: TurnId(1) });
        a(EntryKind::UserMessage { form: Form::Prompt, text: "one".into(), client_key: None });
        let balanced = a(EntryKind::TurnEnd { turn: TurnId(1) });
        // Turn 2 ends while a call is still running: not a safe fork point.
        a(EntryKind::TurnStart { turn: TurnId(2) });
        call(&log, "c-bg", "bash", Consequence::SpawnsProcess, 1, false, true);
        a(EntryKind::TurnEnd { turn: TurnId(2) });
        // The crash hits inside turn 3.
        a(EntryKind::TurnStart { turn: TurnId(3) });
        call(&log, "c-deploy", "deploy", Consequence::NetworkEgress, 2, false, true);
        call(&log, "c-read", "read", Consequence::ReadOnly, 3, true, false);

        assert_eq!(candidate(&log, me), Some(dead));
        assert_eq!(candidate(&log, dead), None, "our own identity is never a candidate");
        let marked = mark_crashed(&log, dead).unwrap();
        let EntryKind::Custom { ns, name, body } = log.get(marked).unwrap().kind else { panic!("marker is a custom entry") };
        assert_eq!((ns.as_str(), name.as_str()), ("Daemon", "Crashed"));
        assert_eq!(body["pid"], json!(dead.pid));
        assert_eq!(body["start_time"], json!(dead.start_time.0));
        assert_eq!(body["open_turn"], json!(3));
        assert_eq!(body["open_calls"], json!(["c-bg", "c-deploy", "c-read"]));
        assert_eq!(body["uncertain"], json!(["bash", "deploy"]));
        assert_eq!(candidate(&log, me), None, "an identity is marked crashed once");
        balanced
    };

    // A later daemon resumes it from the file.
    let log = JsonlLog::open(&path).unwrap();
    assert_eq!(resume_crashed(&log).unwrap(), Some(balanced));
    let log = JsonlLog::open(&path).unwrap();
    let branch = log.read_branch(log.head().unwrap()).unwrap();
    let state = fold(&branch, &CustomFolds::new()).unwrap();
    assert!(state.turn.is_none() && state.open_calls.is_empty() && state.open_ops.is_empty());
    let last = branch.last().unwrap();
    assert_eq!(last.by, Principal::system("recovery"));
    let EntryKind::UserMessage { form: Form::Notice, text, .. } = &last.kind else { panic!("the notice is last: {:?}", last.kind) };
    assert!(text.contains("may or may not have run: bash, deploy"), "{text}");
    assert!(branch.iter().all(|e| e.id <= balanced || e.id == last.id), "the lost turns are off the branch");
    assert!(resume_crashed(&log).is_err(), "a resolved crash is not resumed twice");
    assert_eq!(candidate(&log, me), None);
}
