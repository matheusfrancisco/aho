#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::sync::Arc;

use aho_log::{EntryKind, IndexQuery, JsonlLog, LogStore, SessionIndex};
use aho_types::{CallId, Consequence, ContentBlock, EntryId, Principal, SessionId};
use serde_json::json;

fn note(log: &dyn LogStore, text: &str) -> EntryId {
    log.append(None, Principal::Operator, EntryKind::Note { text: text.into() }).unwrap().id
}
fn search(index: &SessionIndex, text: &str) -> aho_log::IndexPage {
    index.query(&IndexQuery { terms: vec![text.into()], ..IndexQuery::default() }).unwrap()
}

#[test]
fn composite_ids_live_append_reopen_and_explicit_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let db = dir.path().join("index.sqlite3");
    let index = Arc::new(SessionIndex::open(&db, &root).unwrap());
    let a = index.open_log(SessionId(0xa)).unwrap();
    let b = index.open_log(SessionId(0xb)).unwrap();
    assert_eq!(note(&a, "alpha shared"), EntryId(1));
    assert_eq!(note(&b, "beta shared"), EntryId(1));
    let page = search(&index, "shared");
    let identities = page.hits.iter().map(|hit| (hit.session_id.as_str(), hit.entry_id)).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(identities, [("a", EntryId(1)), ("b", EntryId(1))].into_iter().collect());
    assert!(matches!(index.get(SessionId(0xa), EntryId(1)).unwrap().unwrap().kind, EntryKind::Note { text } if text == "alpha shared"));
    drop(a);
    drop(b);
    drop(index);
    let index = Arc::new(SessionIndex::open(&db, &root).unwrap());
    assert_eq!(search(&index, "shared").total, 2);
    let a = index.open_log(SessionId(0xa)).unwrap();
    assert_eq!(note(&a, "resumed unique"), EntryId(2));
    assert_eq!(search(&index, "resumed").hits[0].entry_id, EntryId(2));
    drop(a);
    let raw = JsonlLog::open(root.join("c.jsonl")).unwrap();
    note(&raw, "external unique");
    // Queries use the projection, never scan for newly created sources.
    assert_eq!(search(&index, "external").total, 0);
    assert_eq!(index.rebuild().unwrap().indexed_sessions, 3);
    assert_eq!(search(&index, "external").hits[0].session_id, "c");
    let raw = JsonlLog::open(root.join("a.jsonl")).unwrap();
    note(&raw, "crash recovery append");
    drop(raw);
    let resumed = index.open_log(SessionId(0xa)).unwrap();
    assert_eq!(resumed.head(), Some(EntryId(3)));
    assert_eq!(search(&index, "recovery").hits[0].entry_id, EntryId(3));
}

#[test]
fn fork_pointer_is_durable_and_shared_ancestors_are_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let db = dir.path().join("index.sqlite3");
    let index = Arc::new(SessionIndex::open(&db, &root).unwrap());
    let log = index.open_log(SessionId(7)).unwrap();
    let root_entry = note(&log, "shared ancestor");
    let old = note(&log, "abandoned leaf");
    log.fork(root_entry).unwrap();
    assert_eq!(search(&index, "abandoned").total, 0);
    let all = index.query(&IndexQuery { terms: vec!["abandoned".into()], all_branches: true, ..IndexQuery::default() }).unwrap();
    assert_eq!(all.hits[0].entry_id, old);
    assert!(!all.hits[0].current_branch);
    assert_eq!(all.hits[0].branch_head, Some(root_entry));
    drop(log);
    let log = index.open_log(SessionId(7)).unwrap();
    let new = note(&log, "new leaf");
    assert_eq!(log.get(new).unwrap().parent, Some(root_entry));
    assert_eq!(search(&index, "ancestor").total, 1);
    index.rebuild().unwrap();
    assert_eq!(search(&index, "abandoned").total, 0);
    assert_eq!(search(&index, "new").hits[0].entry_id, new);
}

#[test]
fn tool_result_lookup_stays_on_its_session_and_ancestry() {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(SessionIndex::open(dir.path().join("index.sqlite3"), dir.path().join("sessions")).unwrap());
    let log = index.open_log(SessionId(8)).unwrap();
    let root = note(&log, "root");
    let call = |name: &str| EntryKind::ToolCall {
        call: CallId("reused".into()),
        tool: name.into(),
        args: json!({}),
        class: Consequence::ReadOnly,
    };
    log.append(None, Principal::Operator, call("wrong_branch")).unwrap();
    log.fork(root).unwrap();
    log.append(None, Principal::Operator, call("right_branch")).unwrap();
    let other = index.open_log(SessionId(9)).unwrap();
    other.append(None, Principal::Operator, call("wrong_session")).unwrap();
    let result = log
        .append(
            None,
            Principal::Operator,
            EntryKind::ToolResult {
                call: CallId("reused".into()),
                outcome: aho_log::ToolOutcome::Ok,
                value: json!({}),
                content: vec![ContentBlock::text("needle response")],
                spill_ref: None,
                meta: None,
                late: false,
            },
        )
        .unwrap();
    let page =
        index.query(&IndexQuery { terms: vec!["needle".into()], tool: Some("right_branch".into()), ..IndexQuery::default() }).unwrap();
    assert_eq!(page.hits[0].entry_id, result.id);
    assert_eq!(page.hits[0].session_id, "8");
    let wrong =
        index.query(&IndexQuery { terms: vec!["needle".into()], tool: Some("wrong_branch".into()), ..IndexQuery::default() }).unwrap();
    assert_eq!(wrong.total, 0);
}

#[test]
fn corrupt_missing_and_interrupted_sources_do_not_leak_stale_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let db = dir.path().join("index.sqlite3");
    let index = Arc::new(SessionIndex::open(&db, &root).unwrap());
    let bad = index.open_log(SessionId(1)).unwrap();
    let good = index.open_log(SessionId(2)).unwrap();
    note(&bad, "secret stale");
    note(&good, "healthy searchable");
    drop(bad);
    drop(good);
    // Simulate the persistent dirty marker left by a crash between the source and index commits.
    let sql = rusqlite::Connection::open(&db).unwrap();
    sql.execute("UPDATE sessions SET error='interrupted' WHERE id='1'", []).unwrap();
    assert_eq!(search(&index, "stale").total, 0);
    assert!(index.get(SessionId(1), EntryId(1)).unwrap().is_none());
    assert_eq!(index.refresh_session(SessionId(1)).unwrap().indexed_entries, 1);
    let original = std::fs::read_to_string(root.join("1.jsonl")).unwrap();
    // Repeated ids would be ambiguous even when each entry's own body hash is valid.
    std::fs::write(root.join("1.jsonl"), format!("{original}{original}")).unwrap();
    let report = index.rebuild().unwrap();
    assert_eq!(report.indexed_sessions, 1);
    assert_eq!(report.issues[0].session_id, "1");
    assert_eq!(search(&index, "stale").total, 0);
    assert_eq!(search(&index, "healthy").hits[0].session_id, "2");
    std::fs::write(root.join("1.jsonl"), format!("{original}{{\"head\":999}}\n")).unwrap();
    assert_eq!(index.refresh_session(SessionId(1)).unwrap().issues.len(), 1);
    std::fs::write(root.join("1.jsonl"), original.trim_end()).unwrap();
    assert_eq!(index.refresh_session(SessionId(1)).unwrap().issues.len(), 1);
    std::fs::remove_file(root.join("2.jsonl")).unwrap();
    assert_eq!(index.refresh_session(SessionId(2)).unwrap().issues[0].session_id, "2");
    assert_eq!(search(&index, "healthy").total, 0);
}

#[test]
fn index_root_isolation_and_literal_fts_operators() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    let index = Arc::new(SessionIndex::open(&db, dir.path().join("first")).unwrap());
    let log = index.open_log(SessionId(1)).unwrap();
    note(&log, "alpha OR beta");
    note(&log, "alpha alone");
    assert_eq!(search(&index, "alpha OR beta").total, 1);
    assert!(search(&index, "\" OR * NOT \"").hits.is_empty());
    assert!(SessionIndex::open(&db, dir.path().join("other")).is_err());
}
