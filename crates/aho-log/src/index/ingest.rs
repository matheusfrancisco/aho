use std::io::BufRead;
use std::path::Path;

use aho_types::{EntryId, Hash, SessionId, Value};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use super::{IndexError, IndexIssue, IndexReport, hex, number};
use crate::{Entry, EntryKind};

#[derive(Deserialize)]
#[serde(untagged)]
enum Line {
    Entry(Box<Entry>),
    Head { head: EntryId },
}

#[allow(clippy::disallowed_methods, reason = "authority: session-index-rebuild")]
pub(super) fn rebuild(db: &Connection, root: &Path) -> Result<IndexReport, IndexError> {
    let mut ids = Vec::new();
    for file in std::fs::read_dir(root)? {
        let file = file?;
        let name = file.file_name();
        let Some(stem) = name.to_str().and_then(|s| s.strip_suffix(".jsonl")) else { continue };
        let Ok(id) = u128::from_str_radix(stem, 16) else { continue };
        // One spelling per id prevents duplicate aliases and path traversal.
        if format!("{id:x}") != stem {
            continue;
        }
        ids.push(SessionId(id));
    }
    ids.sort();
    db.execute("DELETE FROM sessions", [])?;
    let mut report = IndexReport::default();
    for id in ids {
        let one = refresh(db, root, id)?;
        report.indexed_sessions += one.indexed_sessions;
        report.indexed_entries += one.indexed_entries;
        report.issues.extend(one.issues);
    }
    Ok(report)
}

pub(super) fn refresh(db: &Connection, root: &Path, id: SessionId) -> Result<IndexReport, IndexError> {
    let sid = hex(id);
    db.execute("DELETE FROM sessions WHERE id=?1", [&sid])?;
    db.execute("INSERT INTO sessions(id) VALUES (?1)", [&sid])?;
    db.execute_batch("SAVEPOINT ingest_session")?;
    let result = load_session(db, &root.join(format!("{sid}.jsonl")), &sid);
    match result {
        Ok(count) => {
            db.execute_batch("RELEASE ingest_session")?;
            Ok(IndexReport { indexed_sessions: 1, indexed_entries: count, issues: Vec::new() })
        }
        Err(error) => {
            db.execute_batch("ROLLBACK TO ingest_session; RELEASE ingest_session")?;
            let reason = error.to_string();
            db.execute("UPDATE sessions SET error=?2 WHERE id=?1", params![sid, reason])?;
            Ok(IndexReport { indexed_sessions: 0, indexed_entries: 0, issues: vec![IndexIssue { session_id: sid, reason }] })
        }
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: session-index-source")]
fn load_session(db: &Connection, path: &Path, sid: &str) -> Result<usize, IndexError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(IndexError("session source is not a regular file (symlinks are not indexed)".into()));
    }
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut head = None;
    let mut count = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(IndexError("incomplete final JSONL record; finish or repair the source before refresh".into()));
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(&line)? {
            Line::Entry(entry) => {
                insert(db, sid, &entry)?;
                head = Some(entry.id);
                count += 1;
            }
            Line::Head { head: next } => {
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries WHERE session_id=?1 AND id=?2)",
                    params![sid, number(next.0)?],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Err(IndexError(format!("fork points to unknown entry {}", next.0)));
                }
                head = Some(next);
            }
        }
    }
    let after = reader.get_ref().metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(IndexError("session changed while indexing; refresh after its writer settles".into()));
    }
    set_head(db, sid, head)?;
    Ok(count)
}

pub(super) fn insert(db: &Connection, sid: &str, entry: &Entry) -> Result<(), IndexError> {
    let id = number(entry.id.0)?;
    let last: Option<i64> = db.query_row("SELECT max(id) FROM entries WHERE session_id=?1", [sid], |r| r.get(0))?;
    if id <= last.unwrap_or(0) {
        return Err(IndexError(format!("non-increasing or duplicate entry id {}", entry.id.0)));
    }
    if !entry.verify() {
        return Err(IndexError(format!("hash chain broken at entry {}", entry.id.0)));
    }
    match entry.parent {
        Some(parent) => {
            let hash: Option<String> = db
                .query_row("SELECT hash FROM entries WHERE session_id=?1 AND id=?2", params![sid, number(parent.0)?], |r| r.get(0))
                .optional()?;
            if hash.as_deref() != Some(entry.prev_hash.to_hex().as_str()) {
                return Err(IndexError(format!("missing parent or broken parent hash at entry {}", entry.id.0)));
            }
        }
        None if last.is_some() || entry.prev_hash != Hash::ZERO => return Err(IndexError(format!("invalid root at entry {}", entry.id.0))),
        None => {}
    }
    let body = serde_json::to_value(&entry.kind)?;
    let kind = match &entry.kind {
        EntryKind::Custom { ns, name, .. } => format!("{ns}/{name}"),
        _ => body.get("type").and_then(Value::as_str).unwrap_or(entry.kind.tag()).to_string(),
    };
    let (call, tool) = match &entry.kind {
        EntryKind::ToolCall { call, tool, .. } => (Some(call.0.as_str()), Some(tool.clone())),
        EntryKind::ToolResult { call, .. } => {
            // Call ids are scoped to the ancestry, not another session or a sibling fork.
            let tool = db.query_row(
                "WITH RECURSIVE ancestry(id,parent,call_id,tool,kind) AS (
                    SELECT id,parent,call_id,tool,kind FROM entries WHERE session_id=?1 AND id=?2
                    UNION ALL SELECT e.id,e.parent,e.call_id,e.tool,e.kind FROM entries e JOIN ancestry a ON e.id=a.parent WHERE e.session_id=?1
                ) SELECT tool FROM ancestry WHERE kind='tool_call' AND call_id=?3 ORDER BY id DESC LIMIT 1",
                params![sid, entry.parent.map(|p| number(p.0)).transpose()?, call.0], |r| r.get::<_, String>(0),
            ).optional()?;
            (Some(call.0.as_str()), tool)
        }
        _ => (None, None),
    };
    let mut text = String::new();
    extract_text(&body, &mut text);
    db.execute(
        "INSERT INTO entries(session_id,id,parent,hash,at,kind,tool,call_id,text,json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            sid,
            id,
            entry.parent.map(|p| number(p.0)).transpose()?,
            entry.hash.to_hex(),
            number(entry.at.0)?,
            kind,
            tool,
            call,
            text,
            serde_json::to_string(entry)?
        ],
    )?;
    Ok(())
}

pub(super) fn set_head(db: &Connection, sid: &str, head: Option<EntryId>) -> Result<(), IndexError> {
    let head = head.map(|h| number(h.0)).transpose()?;
    db.execute("UPDATE entries SET current=0 WHERE session_id=?1 AND current=1", [sid])?;
    db.execute(
        "WITH RECURSIVE ancestry(id,parent) AS (
            SELECT id,parent FROM entries WHERE session_id=?1 AND id=?2
            UNION ALL SELECT e.id,e.parent FROM entries e JOIN ancestry a ON e.id=a.parent WHERE e.session_id=?1
        ) UPDATE entries SET current=1 WHERE session_id=?1 AND id IN (SELECT id FROM ancestry)",
        params![sid, head],
    )?;
    db.execute("UPDATE sessions SET head=?2 WHERE id=?1", params![sid, head])?;
    Ok(())
}

fn extract_text(value: &Value, text: &mut String) {
    match value {
        Value::String(s) => {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(s);
        }
        Value::Array(a) => {
            for v in a {
                extract_text(v, text);
            }
        }
        Value::Object(m) if m.len() == 1 && m.contains_key("digest") => {}
        Value::Object(m) => {
            for (key, v) in m {
                if key != "type" && !(key == "data" && m.get("type").and_then(Value::as_str) == Some("image")) {
                    extract_text(v, text);
                }
            }
        }
        _ => {}
    }
}
