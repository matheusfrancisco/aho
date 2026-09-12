//! The byte-offset tail: partial lines, resume, truncation, replacement.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use foreign_cli::{Reset, Tail, file_ident};

fn append(p: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(bytes).unwrap();
}

fn texts(lines: &[foreign_cli::Line]) -> Vec<String> {
    lines.iter().map(|l| String::from_utf8(l.bytes.clone()).unwrap()).collect()
}

#[test]
fn a_missing_file_is_not_an_error_and_is_picked_up_when_it_appears() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    let mut t = Tail::new(&p);
    let r = t.poll().unwrap();
    assert!(!r.exists && r.lines.is_empty() && r.reset.is_none());
    append(&p, b"{\"a\":1}\n");
    let r = t.poll().unwrap();
    assert!(r.exists);
    assert_eq!(texts(&r.lines), vec!["{\"a\":1}"]);
    assert_eq!(r.lines[0].end, 8);
    assert_eq!(t.offset(), 8);
}

#[test]
fn a_partial_line_waits_across_reads_and_the_offset_stays_on_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"{\"n\":1}\n{\"n\":");
    let mut t = Tail::new(&p);
    let r = t.poll().unwrap();
    assert_eq!(texts(&r.lines), vec!["{\"n\":1}"]);
    assert_eq!(t.offset(), 8, "the half-written second line is not consumed");

    append(&p, b"2");
    assert!(t.poll().unwrap().lines.is_empty(), "still no newline");
    assert_eq!(t.offset(), 8);

    append(&p, b"}\n\n{\"n\":3}\r\n");
    let r = t.poll().unwrap();
    assert_eq!(texts(&r.lines), vec!["{\"n\":2}", "{\"n\":3}"], "blank lines are skipped, CRLF is trimmed");
    assert_eq!(r.lines[0].end, 16);
    assert_eq!(r.lines[1].end, 26);
    assert_eq!(t.offset(), 26);
    assert!(t.poll().unwrap().lines.is_empty());
}

#[test]
fn resume_continues_at_the_saved_offset_without_repeating_lines() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"one\ntwo\n");
    let mut first = Tail::new(&p);
    let r = first.poll().unwrap();
    let saved = r.lines[0].end; // the crash happened after mirroring only `one`
    assert_eq!(saved, 4);
    append(&p, b"three\n");

    let mut resumed = Tail::resume(&p, saved, file_ident(&p));
    let r = resumed.poll().unwrap();
    assert!(r.reset.is_none());
    assert_eq!(texts(&r.lines), vec!["two", "three"]);
    assert_eq!(resumed.offset(), 14);
}

#[test]
fn a_truncated_file_restarts_from_zero_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"aaaa\nbbbb\n");
    let mut t = Tail::new(&p);
    assert_eq!(t.poll().unwrap().lines.len(), 2);
    std::fs::write(&p, b"c\n").unwrap();
    let r = t.poll().unwrap();
    assert_eq!(r.reset, Some(Reset::Truncated { was: 10, len: 2 }));
    assert_eq!(texts(&r.lines), vec!["c"]);
    assert_eq!(t.offset(), 2);
}

#[test]
fn a_resume_offset_past_the_end_counts_as_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"x\n");
    let mut t = Tail::resume(&p, 500, file_ident(&p));
    let r = t.poll().unwrap();
    assert_eq!(r.reset, Some(Reset::Truncated { was: 500, len: 2 }));
    assert_eq!(texts(&r.lines), vec!["x"]);
}

#[cfg(unix)]
#[test]
fn a_replaced_file_is_read_from_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"old-1\nold-2\n");
    let mut t = Tail::new(&p);
    assert_eq!(t.poll().unwrap().lines.len(), 2);
    // Rotation: the old file moves away, a new one (longer, so no truncation) takes the path.
    std::fs::rename(&p, dir.path().join("t.jsonl.1")).unwrap();
    append(&p, b"new-1 with a longer line\n");
    let r = t.poll().unwrap();
    assert_eq!(r.reset, Some(Reset::Replaced));
    assert_eq!(texts(&r.lines), vec!["new-1 with a longer line"]);
}

#[cfg(unix)]
#[test]
fn resume_against_a_different_file_identity_is_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"a\nb\n");
    let old = file_ident(&p);
    std::fs::rename(&p, dir.path().join("gone")).unwrap();
    append(&p, b"fresh\nlines\n");
    let mut t = Tail::resume(&p, 2, old);
    let r = t.poll().unwrap();
    assert_eq!(r.reset, Some(Reset::Replaced));
    assert_eq!(texts(&r.lines), vec!["fresh", "lines"]);
}

#[test]
fn a_resume_offset_inside_a_line_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    append(&p, b"abcdef\nxyz\n");
    let mut t = Tail::resume(&p, 3, file_ident(&p));
    let r = t.poll().unwrap();
    assert_eq!(r.reset, Some(Reset::Misaligned { offset: 3 }));
    assert_eq!(texts(&r.lines), vec!["abcdef", "xyz"]);
}
