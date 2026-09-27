//! The egress allow/deny list and the conservative URL reading it is checked against.
//!
//! Patterns are host globs: `*` matches any run of characters (dots included), `?` one
//! character; matching is case-insensitive against the host alone (no scheme, no port).
//! `*.example.com` does not match `example.com`, so list both. Deny wins; an empty allow list
//! admits every host not denied.
//!
//! [`parse_url`] refuses anything a WHATWG parser (what the HTTP client uses) could read as a
//! different host: whitespace or control characters, backslashes, percent-encoded or non-ASCII
//! hosts, and non-canonical IPv4 spellings (`2130706433`, `0x7f.1`). Implementations of `Egress`
//! additionally check the host their own parser produced on every hop.

use aho_cap::ConfigError;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_types::Value;
use serde_json::json;

pub const ALLOW_KEY: &str = "egress.allow";
pub const DENY_KEY: &str = "egress.deny";

/// The two keys, shared by every capability that enforces the list (tool-web, MCP over HTTP).
pub fn config_keys() -> Vec<ConfigKey> {
    vec![
        ConfigKey::new(
            ALLOW_KEY,
            Ty::Array,
            Some(json!([])),
            "Host globs web_fetch/web_search/MCP-over-HTTP may reach (`*` any run, `?` one char). Empty: every host not denied. Under `world = host` this is policy only: bash can still reach anything.",
        ),
        ConfigKey::new(
            DENY_KEY,
            Ty::Array,
            Some(json!([])),
            "Host globs web_fetch/web_search/MCP-over-HTTP must never reach; wins over egress.allow.",
        ),
    ]
}

/// `*` / `?` glob over ASCII, case-insensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedUrl {
    /// `http` or `https`.
    pub scheme: String,
    /// Lower-cased, no brackets, no trailing dot.
    pub host: String,
    pub port: Option<u16>,
}

fn is_numeric_label(l: &str) -> bool {
    !l.is_empty() && (l.bytes().all(|b| b.is_ascii_digit()) || l.starts_with("0x"))
}

fn canonical_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Reads scheme, host and port the way the operator's list sees them, refusing ambiguity.
pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("it contains whitespace or control characters".into());
    }
    if url.contains('\\') {
        return Err("it contains a backslash".into());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(|| "it has no `scheme://`".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http and https are fetched, not `{scheme}`"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (h, after) = inner.split_once(']').ok_or_else(|| "its IPv6 host has no closing `]`".to_string())?;
        if !h.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.') || h.is_empty() {
            return Err("its IPv6 host is malformed".into());
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(|| "text follows its IPv6 host".to_string())?),
        };
        (h.to_ascii_lowercase(), port)
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), Some(p)),
            None => (hostport.to_ascii_lowercase(), None),
        }
    };
    let port = match port {
        None | Some("") => None,
        Some(p) => Some(p.parse::<u16>().map_err(|_| format!("its port `{p}` is not a number from 0 to 65535"))?),
    };
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    if host.is_empty() {
        return Err("it has no host".into());
    }
    if !host.contains(':') {
        if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
            return Err(
                "its host has characters other than letters, digits, `.`, `-`, `_` (write internationalized names as `xn--` punycode)"
                    .into(),
            );
        }
        if host.split('.').next_back().is_some_and(is_numeric_label) && !canonical_ipv4(&host) {
            return Err("its host is a non-canonical IPv4 address; write it as four decimal parts like 127.0.0.1".into());
        }
    }
    Ok(ParsedUrl { scheme, host, port })
}

/// The operator's egress list. Patterns are stored lower-cased.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostList {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

fn valid_pattern(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'*' | b'?' | b':'))
}

fn normalize_host(host: &str) -> String {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    h.strip_suffix('.').map(str::to_string).unwrap_or(h)
}

impl HostList {
    /// Validates and lower-cases every pattern; the error names the offending key and pattern.
    pub fn new(allow: Vec<String>, deny: Vec<String>) -> Result<HostList, String> {
        for (key, list) in [(ALLOW_KEY, &allow), (DENY_KEY, &deny)] {
            if let Some(bad) = list.iter().find(|p| !valid_pattern(p)) {
                return Err(format!(
                    "`{bad}` in `{key}` is not a host glob (letters, digits, `.`, `-`, `_`, `:`, `*`, `?`); fix or remove it"
                ));
            }
        }
        let lower = |v: Vec<String>| v.into_iter().map(|p| p.to_ascii_lowercase()).collect();
        Ok(HostList { allow: lower(allow), deny: lower(deny) })
    }

    pub fn from_loadout(l: &Loadout) -> Result<HostList, ConfigError> {
        let list = |key: &str| -> Result<Vec<String>, ConfigError> {
            let bad = || ConfigError::Type {
                key: key.to_string(),
                expected: "an array of host globs (letters, digits, `.`, `-`, `_`, `:`, `*`, `?`)",
            };
            let items: Vec<String> = match l.get(key) {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => {
                    items.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>().ok_or_else(bad)?
                }
                Some(_) => return Err(bad()),
            };
            if items.iter().all(|p| valid_pattern(p)) { Ok(items) } else { Err(bad()) }
        };
        let (allow, deny) = (list(ALLOW_KEY)?, list(DENY_KEY)?);
        let lower = |v: Vec<String>| v.into_iter().map(|p| p.to_ascii_lowercase()).collect();
        Ok(HostList { allow: lower(allow), deny: lower(deny) })
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// `Err` is the operator-facing refusal naming the key to change.
    pub fn check_host(&self, host: &str) -> Result<(), String> {
        let host = normalize_host(host);
        if let Some(p) = self.deny.iter().find(|p| glob_match(p, &host)) {
            return Err(format!(
                "`{host}` matches `{p}` in `{DENY_KEY}`, so the request was not sent. To reach it, remove `{p}` from `{DENY_KEY}`."
            ));
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|p| glob_match(p, &host)) {
            return Err(format!(
                "`{host}` is not in `{ALLOW_KEY}` ({}), so the request was not sent. To reach it, add `{host}` (or a glob covering it) to `{ALLOW_KEY}`.",
                self.allow.join(", ")
            ));
        }
        Ok(())
    }

    /// Parses `url` conservatively ([`parse_url`]) and checks its host.
    pub fn check_url(&self, url: &str) -> Result<ParsedUrl, String> {
        let parsed = parse_url(url).map_err(|why| format!("`{url}` is not a URL aho will fetch: {why}. Nothing was sent."))?;
        self.check_host(&parsed.host)?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(allow: &[&str], deny: &[&str]) -> HostList {
        HostList::new(allow.iter().map(|s| s.to_string()).collect(), deny.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    #[test]
    fn globs_match_hosts_case_insensitively() {
        assert!(glob_match("*.example.com", "a.b.EXAMPLE.com"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("10.*", "10.0.0.1") && !glob_match("10.*", "110.0.0.1"));
        assert!(glob_match("api?.x.io", "api1.x.io") && !glob_match("api?.x.io", "api.x.io"));
        assert!(glob_match("*", "anything") && glob_match("a*b*c", "aXXbYYc") && !glob_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn deny_wins_and_an_empty_allow_admits_the_rest() {
        let l = list(&[], &["*.internal", "169.254.*"]);
        assert!(l.check_host("example.com").is_ok());
        let e = l.check_host("db.internal").unwrap_err();
        assert!(e.contains("`*.internal`") && e.contains("egress.deny") && e.contains("not sent"), "{e}");
        let l = list(&["docs.rs", "*.docs.rs"], &["evil.docs.rs"]);
        assert!(l.check_host("docs.rs").is_ok() && l.check_host("x.docs.rs").is_ok());
        assert!(l.check_host("evil.docs.rs").unwrap_err().contains("egress.deny"));
        let e = l.check_host("crates.io").unwrap_err();
        assert!(e.contains("egress.allow") && e.contains("docs.rs, *.docs.rs"), "{e}");
    }

    #[test]
    fn urls_are_read_the_way_the_list_sees_them() {
        let p = parse_url("HTTPS://user:pw@Example.COM.:8443/a?b#c").unwrap();
        assert_eq!((p.scheme.as_str(), p.host.as_str(), p.port), ("https", "example.com", Some(8443)));
        let p = parse_url("http://[::1]:80/").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("::1", Some(80)));
        assert_eq!(parse_url("http://a.com@b.com/").unwrap().host, "b.com");
        assert_eq!(parse_url("http://127.0.0.1/").unwrap().host, "127.0.0.1");
    }

    #[test]
    fn ambiguous_urls_are_refused() {
        for bad in [
            "ftp://x.com/",
            "x.com/path",
            "http://allowed.com\\@evil.com/",
            "http://ev\til.com/",
            "http://evil%2ecom/",
            "http://2130706433/",
            "http://0x7f.0.0.1/",
            "http://127.1/",
            "http://010.0.0.1/",
            "http://bücher.de/",
            "http:///path",
            "http://x.com:99999/",
        ] {
            assert!(parse_url(bad).is_err(), "{bad} should be refused");
        }
        assert!(list(&[], &[]).check_url("file:///etc/passwd").unwrap_err().contains("Nothing was sent"));
    }

    #[test]
    fn bad_patterns_name_their_key() {
        let e = HostList::new(vec![], vec!["http://x.com".into()]).unwrap_err();
        assert!(e.contains("egress.deny"), "{e}");
    }
}
