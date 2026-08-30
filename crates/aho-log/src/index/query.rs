use aho_types::{EntryId, Timestamp};
use rusqlite::{params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

use super::{IndexError, IndexIssue, SessionIndex, issues, number};

/// ANDed FTS5 Unicode-token phrases plus metadata predicates. No SQL or FTS syntax is accepted
/// from callers: each phrase is escaped. This is separate from current-session literal/regex search.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexQuery {
    pub terms: Vec<String>,
    /// Canonical full hexadecimal session id. Entry ids alone are never cross-session identities.
    pub session_id: Option<String>,
    /// Serialized snake_case core kinds or exact custom `Namespace/Name`; ORed within this list.
    pub kinds: Vec<String>,
    pub tool: Option<String>,
    pub after: Option<Timestamp>,
    pub before: Option<Timestamp>,
    /// False selects the current branch of each session; true includes abandoned branches once.
    pub all_branches: bool,
    pub limit: usize,
    pub offset: usize,
}
impl Default for IndexQuery {
    fn default() -> Self {
        Self {
            terms: Vec::new(),
            session_id: None,
            kinds: Vec::new(),
            tool: None,
            after: None,
            before: None,
            all_branches: false,
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexHit {
    pub session_id: String,
    pub entry_id: EntryId,
    pub parent: Option<EntryId>,
    pub branch_head: Option<EntryId>,
    pub current_branch: bool,
    pub at: Timestamp,
    pub kind: String,
    pub tool: Option<String>,
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexPage {
    pub total: usize,
    pub offset: usize,
    pub has_more: bool,
    pub hits: Vec<IndexHit>,
    /// Search is explicitly partial if any source is quarantined or interrupted.
    pub issues: Vec<IndexIssue>,
}

impl SessionIndex {
    /// Reads one SQLite snapshot only. Source changes made outside IndexedLog require explicit
    /// refresh/rebuild; this operation deliberately performs no directory scan or file reads.
    pub fn query(&self, query: &IndexQuery) -> Result<IndexPage, IndexError> {
        if query.limit == 0 || query.limit > 200 {
            return Err(IndexError("query limit must be 1..=200".into()));
        }
        if query.terms.iter().any(|s| s.trim().is_empty()) || query.terms.iter().map(String::len).sum::<usize>() > 4000 {
            return Err(IndexError("query terms must be nonempty and at most 4000 bytes in total".into()));
        }
        if query.after.zip(query.before).is_some_and(|(a, b)| a > b) {
            return Err(IndexError("after must not exceed before".into()));
        }
        let mut sql = String::from(" FROM entries e JOIN sessions s ON s.id=e.session_id");
        let has_text = !query.terms.is_empty();
        if has_text {
            sql.push_str(" JOIN search ON search.rowid=e.row_id");
        }
        sql.push_str(" WHERE s.error IS NULL");
        let mut args = Vec::<Value>::new();
        if has_text {
            sql.push_str(" AND search MATCH ?");
            let text = query.terms.iter().map(|s| format!("\"{}\"", s.replace('"', "\"\""))).collect::<Vec<_>>().join(" AND ");
            args.push(text.into());
        }
        if !query.all_branches {
            sql.push_str(" AND e.current=1");
        }
        if let Some(id) = &query.session_id {
            let n = u128::from_str_radix(id, 16).map_err(|_| IndexError("session id must be full hexadecimal".into()))?;
            if format!("{n:x}") != *id {
                return Err(IndexError("session id must use canonical lowercase hexadecimal".into()));
            }
            sql.push_str(" AND e.session_id=?");
            args.push(id.clone().into());
        }
        if !query.kinds.is_empty() {
            sql.push_str(" AND e.kind IN (");
            sql.push_str(&vec!["?"; query.kinds.len()].join(","));
            sql.push(')');
            args.extend(query.kinds.iter().cloned().map(Value::from));
        }
        if let Some(tool) = &query.tool {
            sql.push_str(" AND e.tool=?");
            args.push(tool.clone().into());
        }
        if let Some(after) = query.after {
            sql.push_str(" AND e.at>=?");
            args.push(number(after.0)?.into());
        }
        if let Some(before) = query.before {
            sql.push_str(" AND e.at<=?");
            args.push(number(before.0)?.into());
        }
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let total: usize = tx.query_row(&format!("SELECT count(*){sql}"), params_from_iter(&args), |r| r.get(0))?;
        let snippet = if has_text { "snippet(search,0,'','',' … ',32)" } else { "substr(e.text,1,240)" };
        let order = if has_text { "bm25(search),e.at DESC,e.session_id,e.id DESC" } else { "e.at DESC,e.session_id,e.id DESC" };
        let select = format!(
            "SELECT e.session_id,e.id,e.parent,s.head,e.current,e.at,e.kind,e.tool,{snippet}{sql} ORDER BY {order} LIMIT ? OFFSET ?"
        );
        args.push(i64::try_from(query.limit).map_err(|e| IndexError(e.to_string()))?.into());
        args.push(i64::try_from(query.offset).map_err(|e| IndexError(e.to_string()))?.into());
        let hits = {
            let mut stmt = tx.prepare(&select)?;
            stmt.query_map(params_from_iter(&args), |r| {
                Ok(IndexHit {
                    session_id: r.get(0)?,
                    entry_id: EntryId(r.get(1)?),
                    parent: r.get::<_, Option<u64>>(2)?.map(EntryId),
                    branch_head: r.get::<_, Option<u64>>(3)?.map(EntryId),
                    current_branch: r.get(4)?,
                    at: Timestamp(r.get(5)?),
                    kind: r.get(6)?,
                    tool: r.get(7)?,
                    snippet: r.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let issues = issues(&tx)?;
        tx.commit()?;
        Ok(IndexPage { total, offset: query.offset, has_more: query.offset.saturating_add(hits.len()) < total, hits, issues })
    }
}
