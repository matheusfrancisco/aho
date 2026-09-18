//! Cross-session query DSL. Bare words and double-quoted phrases are ANDed Unicode-token
//! searches; filters are `session:<hex>`, `kind:<snake_case>`, `tool:<name>`,
//! `after:<unix_ms>`, `before:<unix_ms>`, `branch:current|all`, `limit:<1..200>`.
//! Repeated kinds are ORed; other repeated filters are rejected. Quoting protects colons:
//! `"https://example.org"` is text, whereas an unknown unquoted filter is an error.

use std::collections::BTreeSet;

use aho_log::{IndexError, IndexPage, IndexQuery};
use aho_types::Timestamp;

pub fn parse_session_query(input: &str) -> Result<IndexQuery, IndexError> {
    if input.len() > 4000 {
        return Err(IndexError("session query is longer than 4000 bytes".into()));
    }
    let mut query = IndexQuery::default();
    let mut seen = BTreeSet::new();
    for (token, colon) in tokens(input)? {
        let Some(at) = colon else {
            query.terms.push(token);
            continue;
        };
        let (key, rest) = token.split_at(at);
        let value = &rest[1..];
        if value.is_empty() {
            return Err(IndexError(format!("filter `{key}` needs a value")));
        }
        if key != "kind" && !seen.insert(key.to_string()) {
            return Err(IndexError(format!("filter `{key}` was specified more than once")));
        }
        match key {
            "session" => {
                let id = u128::from_str_radix(value, 16).map_err(|_| IndexError("session filter needs a full hexadecimal id".into()))?;
                if format!("{id:x}") != value {
                    return Err(IndexError("session id must be canonical lowercase hexadecimal".into()));
                }
                query.session_id = Some(value.to_string());
            }
            "kind" => query.kinds.push(value.to_string()),
            "tool" => query.tool = Some(value.to_string()),
            "after" => query.after = Some(Timestamp(timestamp(value)?)),
            "before" => query.before = Some(Timestamp(timestamp(value)?)),
            "branch" => {
                query.all_branches = match value {
                    "current" => false,
                    "all" => true,
                    _ => return Err(IndexError("branch must be current or all".into())),
                }
            }
            "limit" => {
                query.limit = value.parse().map_err(|_| IndexError("limit must be an integer in 1..=200".into()))?;
                if query.limit == 0 || query.limit > 200 {
                    return Err(IndexError("limit must be in 1..=200".into()));
                }
            }
            other => return Err(IndexError(format!("unknown session query filter `{other}`; quote text containing a colon"))),
        }
    }
    if query.after.zip(query.before).is_some_and(|(a, b)| a > b) {
        return Err(IndexError("after must not exceed before".into()));
    }
    Ok(query)
}

fn timestamp(value: &str) -> Result<u64, IndexError> {
    let time: u64 = value.parse().map_err(|_| IndexError("timestamps must be nonnegative Unix milliseconds".into()))?;
    if time > i64::MAX as u64 {
        return Err(IndexError("timestamp exceeds supported range".into()));
    }
    Ok(time)
}

/// Quotes can occur in filter values (`tool:"name with spaces"`). A colon is a filter delimiter
/// only outside quotes and only before any quoted segment in the token.
fn tokens(input: &str) -> Result<Vec<(String, Option<usize>)>, IndexError> {
    let mut out = Vec::new();
    let mut token = String::new();
    let mut colon = None;
    let mut quoted = false;
    let mut saw_quote = false;
    let mut escaped = false;
    let mut started = false;
    for ch in input.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quoted {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            saw_quote = true;
            started = true;
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if started {
                if token.trim().is_empty() {
                    return Err(IndexError("empty quoted search phrase".into()));
                }
                out.push((std::mem::take(&mut token), colon.take()));
                saw_quote = false;
                started = false;
            }
            continue;
        }
        if ch == ':' && !quoted && !saw_quote && colon.is_none() {
            colon = Some(token.len());
        }
        token.push(ch);
        started = true;
    }
    if quoted || escaped {
        return Err(IndexError("unterminated quoted search phrase".into()));
    }
    if started {
        if token.trim().is_empty() {
            return Err(IndexError("empty quoted search phrase".into()));
        }
        out.push((token, colon));
    }
    Ok(out)
}

pub fn render_session_query(page: &IndexPage) -> String {
    let mut lines = vec![format!("{} matching entries{}", page.total, if page.has_more { " (more available)" } else { "" })];
    for hit in &page.hits {
        lines.push(format!(
            "{}#{} {}{}: {}",
            hit.session_id,
            hit.entry_id.0,
            hit.kind,
            if hit.current_branch { "" } else { " (off branch)" },
            hit.snippet.replace('\n', " ")
        ));
    }
    for issue in &page.issues {
        lines.push(format!("unavailable {}: {}", issue.session_id, issue.reason));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_text_and_typed_filters_stay_distinct() {
        let query = parse_session_query(
            r#""https://host" "two words" kind:note kind:user_message session:af tool:"a tool" branch:all after:10 before:20 limit:7"#,
        )
        .unwrap();
        assert_eq!(query.terms, ["https://host", "two words"]);
        assert_eq!(query.kinds, ["note", "user_message"]);
        assert_eq!(query.session_id.as_deref(), Some("af"));
        assert_eq!(query.tool.as_deref(), Some("a tool"));
        assert!(query.all_branches);
        assert_eq!((query.after, query.before, query.limit), (Some(Timestamp(10)), Some(Timestamp(20)), 7));
    }

    #[test]
    fn malformed_filters_never_widen_to_text_search() {
        for input in [
            "unknown:x",
            "session:../f",
            "session:AF",
            "tool:",
            "branch:other",
            "limit:0",
            "limit:201",
            "after:2 before:1",
            "tool:a tool:b",
            "\"open",
            "\"\"",
        ] {
            assert!(parse_session_query(input).is_err(), "accepted {input}");
        }
    }
}
