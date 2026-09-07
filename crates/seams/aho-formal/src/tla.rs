//! TLA+ values ↔ JSON. `to_tla` renders a trace record as a TLA+ expression (records, tuples,
//! strings, 32-bit integers, booleans); `parse_value` reads a value as TLC prints it in an error
//! trace, falling back to the raw text when the syntax is not one TLC emits.

use aho_types::Value;
use serde_json::{Map, Number};

/// TLC integers are 32-bit; larger literals do not parse.
pub const TLC_INT_MAX: i64 = i32::MAX as i64;
pub const TLC_INT_MIN: i64 = i32::MIN as i64;

/// JSON → TLA+ expression. Objects become records (keys must be identifiers; `null` fields are
/// dropped), arrays become tuples. Fails on floats, out-of-range integers, empty or ill-keyed
/// objects — values TLA+ cannot hold without a silent change of meaning.
pub fn to_tla(v: &Value) -> Result<String, String> {
    match v {
        Value::Null => Err("null has no TLA+ counterpart".into()),
        Value::Bool(b) => Ok(if *b { "TRUE".into() } else { "FALSE".into() }),
        Value::Number(n) => match n.as_i64() {
            Some(i) if (TLC_INT_MIN..=TLC_INT_MAX).contains(&i) => Ok(i.to_string()),
            Some(i) => Err(format!("integer {i} is outside TLC's 32-bit range")),
            None => Err(format!("number {n} is not an integer; TLC has no reals")),
        },
        Value::String(s) => Ok(quote(s)),
        Value::Array(xs) => {
            let parts: Result<Vec<String>, String> = xs.iter().map(to_tla).collect();
            Ok(format!("<<{}>>", parts?.join(", ")))
        }
        Value::Object(m) => {
            let mut fields = Vec::new();
            for (k, x) in m {
                if x.is_null() {
                    continue;
                }
                if !is_ident(k) {
                    return Err(format!("record field `{k}` is not a TLA+ identifier"));
                }
                fields.push(format!("{k} |-> {}", to_tla(x).map_err(|e| format!("{k}: {e}"))?));
            }
            if fields.is_empty() {
                return Err("an empty object has no TLA+ record form".into());
            }
            Ok(format!("[{}]", fields.join(", ")))
        }
    }
}

pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&s)
}

const RESERVED: &[&str] = &[
    "ASSUME",
    "ASSUMPTION",
    "AXIOM",
    "CASE",
    "CHOOSE",
    "CONSTANT",
    "CONSTANTS",
    "DOMAIN",
    "ELSE",
    "ENABLED",
    "EXCEPT",
    "EXTENDS",
    "IF",
    "IN",
    "INSTANCE",
    "LET",
    "LOCAL",
    "MODULE",
    "OTHER",
    "SF_",
    "SUBSET",
    "THEN",
    "THEOREM",
    "UNCHANGED",
    "UNION",
    "VARIABLE",
    "VARIABLES",
    "WF_",
    "WITH",
    "TRUE",
    "FALSE",
];

/// `---- MODULE Name ----` → `Name`.
pub fn module_name(src: &str) -> Option<String> {
    src.lines().find_map(|l| {
        let t = l.trim();
        if !t.starts_with("----") {
            return None;
        }
        let rest = t.trim_start_matches('-').trim_start();
        let rest = rest.strip_prefix("MODULE")?;
        let name: String = rest.trim_start().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        (!name.is_empty()).then_some(name)
    })
}

/// A value as TLC prints it → JSON. Tuples/sequences and sets → arrays, records → objects,
/// functions `k :> v @@ …` → objects (keys stringified), model values → strings. Anything
/// unparseable comes back as the trimmed text.
pub fn parse_value(s: &str) -> Value {
    let mut p = Parser { s: s.as_bytes(), i: 0 };
    match p.value() {
        Some(v) if p.at_end() => v,
        _ => Value::String(s.trim().to_string()),
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn at_end(&mut self) -> bool {
        self.ws();
        self.i >= self.s.len()
    }
    fn eat(&mut self, tok: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(tok.as_bytes()) {
            self.i += tok.len();
            true
        } else {
            false
        }
    }
    fn peek(&mut self, tok: &str) -> bool {
        self.ws();
        self.s[self.i..].starts_with(tok.as_bytes())
    }

    fn value(&mut self) -> Option<Value> {
        let first = self.atom()?;
        // functions: a :> b @@ c :> d
        if self.eat(":>") {
            let mut map = Map::new();
            let v = self.atom()?;
            map.insert(key_of(&first), v);
            while self.eat("@@") {
                let k = self.atom()?;
                if !self.eat(":>") {
                    return None;
                }
                let v = self.atom()?;
                map.insert(key_of(&k), v);
            }
            return Some(Value::Object(map));
        }
        Some(first)
    }

    fn atom(&mut self) -> Option<Value> {
        self.ws();
        if self.eat("<<") {
            return self.list(">>").map(Value::Array);
        }
        if self.eat("{") {
            return self.list("}").map(Value::Array);
        }
        if self.eat("(") {
            let v = self.value()?;
            return self.eat(")").then_some(v);
        }
        if self.eat("[") {
            let mut map = Map::new();
            if self.eat("]") {
                return Some(Value::Object(map));
            }
            loop {
                let k = self.ident()?;
                if !self.eat("|->") {
                    return None;
                }
                let v = self.value()?;
                map.insert(k, v);
                if self.eat("]") {
                    return Some(Value::Object(map));
                }
                if !self.eat(",") {
                    return None;
                }
            }
        }
        if self.peek("\"") {
            return self.string().map(Value::String);
        }
        if self.eat("TRUE") {
            return Some(Value::Bool(true));
        }
        if self.eat("FALSE") {
            return Some(Value::Bool(false));
        }
        let start = self.i;
        if self.i < self.s.len() && (self.s[self.i] == b'-' || self.s[self.i].is_ascii_digit()) {
            self.i += 1;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
            return text.parse::<i64>().ok().map(|n| Value::Number(Number::from(n)));
        }
        self.ident().map(Value::String)
    }

    fn list(&mut self, close: &str) -> Option<Vec<Value>> {
        let mut out = Vec::new();
        if self.eat(close) {
            return Some(out);
        }
        loop {
            out.push(self.value()?);
            if self.eat(close) {
                return Some(out);
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        (self.i > start).then(|| String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("\"") {
            return None;
        }
        let mut out = Vec::new();
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'"' => {
                    self.i += 1;
                    return String::from_utf8(out).ok();
                }
                b'\\' if self.i + 1 < self.s.len() => {
                    let c = self.s[self.i + 1];
                    out.push(match c {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        c => c,
                    });
                    self.i += 2;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        None
    }
}

fn key_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A TLC `.cfg` → `CheckConfig`: `CONSTANT(S)` (`N = v` and `N <- Op`, one per line), `INIT`,
/// `NEXT`, `INVARIANT(S)`, `PROPERTY(IES)`, `CONSTRAINT(S)`, `CHECK_DEADLOCK` (TLC's default:
/// on). Sections `CheckConfig` cannot express (`SPECIFICATION`, `SYMMETRY`, …) are refused
/// rather than dropped.
pub fn parse_cfg(text: &str) -> Result<crate::CheckConfig, crate::FormalError> {
    let mut c = crate::CheckConfig { check_deadlock: true, ..crate::CheckConfig::default() };
    let mut section = "";
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split("\\*").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        let rest = if CFG_KEYWORDS.contains(&first) {
            section = first;
            line[first.len()..].trim()
        } else {
            line
        };
        if rest.is_empty() {
            continue;
        }
        let bad = |what: &str| crate::FormalError::Invalid(format!("cfg line {}: {what}", i + 1));
        match section {
            "CONSTANT" | "CONSTANTS" => {
                let (k, v) = if let Some((k, v)) = rest.split_once("<-") {
                    (k.trim(), format!("<- {}", v.trim()))
                } else if let Some((k, v)) = rest.split_once('=') {
                    (k.trim(), v.trim().to_string())
                } else {
                    return Err(bad("expected `Name = value` or `Name <- Operator`"));
                };
                c.constants.push((k.to_string(), v));
            }
            "INIT" => c.init = rest.to_string(),
            "NEXT" => c.next = rest.to_string(),
            "INVARIANT" | "INVARIANTS" => c.invariants.extend(rest.split_whitespace().map(str::to_string)),
            "PROPERTY" | "PROPERTIES" => c.properties.extend(rest.split_whitespace().map(str::to_string)),
            "CONSTRAINT" | "CONSTRAINTS" => c.constraints.extend(rest.split_whitespace().map(str::to_string)),
            "CHECK_DEADLOCK" => {
                c.check_deadlock = match rest {
                    "TRUE" => true,
                    "FALSE" => false,
                    _ => return Err(bad("CHECK_DEADLOCK takes TRUE or FALSE")),
                }
            }
            "" => return Err(bad("text before any section keyword")),
            other => {
                return Err(bad(&format!(
                    "`{other}` is not supported here; aho checks INIT/NEXT specs with INVARIANT/PROPERTY/CONSTRAINT/CONSTANTS"
                )));
            }
        }
    }
    Ok(c)
}

pub const CFG_KEYWORDS: &[&str] = &[
    "CONSTANT",
    "CONSTANTS",
    "INIT",
    "NEXT",
    "SPECIFICATION",
    "INVARIANT",
    "INVARIANTS",
    "PROPERTY",
    "PROPERTIES",
    "CONSTRAINT",
    "CONSTRAINTS",
    "ACTION_CONSTRAINT",
    "ACTION_CONSTRAINTS",
    "SYMMETRY",
    "VIEW",
    "CHECK_DEADLOCK",
    "POSTCONDITION",
    "ALIAS",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_render_and_reject_what_tla_cannot_hold() {
        let r = to_tla(&json!({"action": "Call", "id": 3, "uses": ["a", "b"], "late": false, "skip": null})).unwrap();
        assert_eq!(r, r#"[action |-> "Call", id |-> 3, late |-> FALSE, uses |-> <<"a", "b">>]"#);
        assert_eq!(to_tla(&json!("say \"hi\"\n")).unwrap(), r#""say \"hi\"\n""#);
        assert!(to_tla(&json!(1.5)).is_err());
        assert!(to_tla(&json!(1_u64 << 40)).unwrap_err().contains("32-bit"));
        assert!(to_tla(&json!({"not an ident": 1})).is_err());
        assert!(to_tla(&json!({})).is_err());
        assert!(to_tla(&json!({"CASE": 1})).is_err(), "reserved words are not field names");
    }

    #[test]
    fn tlc_printed_values_parse_to_json() {
        assert_eq!(parse_value("<<1, \"a\", TRUE>>"), json!([1, "a", true]));
        assert_eq!(parse_value("{}"), json!([]));
        assert_eq!(parse_value("[kind |-> \"call\", id |-> -2]"), json!({"kind": "call", "id": -2}));
        assert_eq!(parse_value("(c1 :> 1 @@ c2 :> 0)"), json!({"c1": 1, "c2": 0}));
        assert_eq!(parse_value("<<[a |-> {1, 2}]>>"), json!([{"a": [1, 2]}]));
        assert_eq!(parse_value("c1"), json!("c1"), "model values are strings");
        assert_eq!(parse_value("x \\in S"), json!("x \\in S"), "unparseable text is kept verbatim");
    }

    #[test]
    fn cfg_sections_are_read_and_unsupported_ones_refused() {
        assert!(parse_cfg("INIT Init\nNEXT Next\n").unwrap().check_deadlock, "TLC checks deadlock unless told not to");
        assert!(matches!(parse_cfg("SPECIFICATION Spec\n"), Err(crate::FormalError::Invalid(m)) if m.contains("SPECIFICATION")));
        assert!(matches!(parse_cfg("CONSTANT N\n"), Err(crate::FormalError::Invalid(_))));
        let c = parse_cfg("CONSTANTS\n  N = 3\nINIT I\nNEXT N2\nINVARIANT A B\nPROPERTY P\nCONSTRAINT C\nCHECK_DEADLOCK FALSE\n").unwrap();
        assert_eq!((c.init.as_str(), c.next.as_str()), ("I", "N2"));
        assert_eq!((c.invariants.len(), c.properties.len(), c.constraints.len()), (2, 1, 1));
        assert!(!c.check_deadlock);
    }

    #[test]
    fn module_header_is_found() {
        assert_eq!(module_name("\\* c\n------------------------------ MODULE Log ------------------------------\n"), Some("Log".into()));
        assert_eq!(module_name("---- MODULE A_1 ----"), Some("A_1".into()));
        assert_eq!(module_name("no header"), None);
    }
}
