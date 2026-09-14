//! `output::parse` on what the real TLC printed (fixtures/tlc, recorded by record-tlc.sh).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod common;

use formal_tlc::output::{self, TlcResult, Violation};

/// Every recording whose file name starts with `module-`, as `(exit code, output)`.
fn recordings(module: &str) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(common::fixtures().join("tlc")).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{module}-")) {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        let (head, body) = text.split_once('\n').unwrap();
        out.push((head.trim_start_matches("#exit ").trim().parse().unwrap(), body.to_string()));
    }
    assert!(!out.is_empty(), "no recordings for {module}");
    out
}

#[test]
fn pass_recordings_parse_as_pass_with_their_depth() {
    for (code, out) in recordings("Pass").into_iter().chain(recordings("AhoBounded")) {
        match output::parse(&out, code) {
            TlcResult::Pass { depth, distinct } => {
                assert!(depth.is_some() && distinct.is_some(), "{out}");
            }
            TlcResult::Violation { what: Violation::Deadlock, trace } => assert_eq!(code, 11, "{trace:?}"),
            other => panic!("{other:?}\n{out}"),
        }
    }
}

#[test]
fn violation_recording_has_the_invariant_and_every_state() {
    let (code, out) = recordings("Violation").remove(0);
    assert_eq!(code, 12, "TLC's exit code for a safety violation");
    let TlcResult::Violation { what, trace } = output::parse(&out, code) else { panic!("{out}") };
    assert_eq!(what, Violation::Invariant("Small".into()));
    assert_eq!(trace.iter().map(|s| s.step).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(trace[1].action, "Call(\"c1\")");
    assert!(trace[1].location.ends_with("of module Violation"));
    assert_eq!(trace[2].var("seen"), Some("[c1 |-> 1, c2 |-> 1]"));
    assert_eq!(trace[0].vars.len(), 3);
}

#[test]
fn parse_error_recording_names_the_line() {
    let (code, out) = recordings("Broken").remove(0);
    let TlcResult::ParseError(msg) = output::parse(&out, code) else { panic!("{out}") };
    assert!(msg.contains("***Parse Error***") && msg.contains("line 6"), "{msg}");
}

#[test]
fn trace_recordings_are_accepted_or_stuck() {
    let mut seen = (false, false);
    for (code, out) in recordings("AhoTrace") {
        match output::parse(&out, code) {
            TlcResult::Pass { .. } => seen.0 = true,
            TlcResult::Violation { what: Violation::Deadlock, trace } => {
                seen.1 = true;
                assert_eq!(trace.last().unwrap().var("ahoTraceIdx"), Some("3"), "stuck on the third record");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(seen, (true, true));
}
