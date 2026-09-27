//! Strict validation of JSON values and whole frames against the [`Protocol`] table: every
//! required field present with the right type, no field the table does not name, every enum
//! value and union tag known. Readers of the wire ignore unknown fields; this check does
//! not, because it is what proves the table and the server agree (`tests/schema.rs`).

use serde_json::Value;

use crate::protocol::{Field, Protocol, Shape, Ty};

/// Checks `v` against `ty`; the error names the JSON path that failed.
pub fn check(p: &Protocol, ty: &Ty, v: &Value) -> Result<(), String> {
    walk(p, ty, v, "$")
}

/// Checks a client → server frame: the envelope, then the params of its method (`hello` too).
pub fn check_client_frame(p: &Protocol, frame: &Value) -> Result<(), String> {
    check(p, &Ty::Ref("ClientFrame"), frame)?;
    let method = frame["method"].as_str().unwrap_or_default();
    let params = frame.get("params").filter(|v| !v.is_null());
    let def = match method {
        "hello" => Some("Hello"),
        m => p.method(m).ok_or_else(|| format!("$.method: `{m}` is not a method of this protocol"))?.params,
    };
    match (def, params) {
        (Some(d), Some(v)) => walk(p, &Ty::Ref(d), v, "$.params"),
        (Some(d), None) => walk(p, &Ty::Ref(d), &Value::Object(Default::default()), "$.params"),
        (None, Some(v)) if v.as_object().is_some_and(|o| !o.is_empty()) => Err(format!("$.params: `{method}` takes no params, got {v}")),
        (None, _) => Ok(()),
    }
}

/// Checks a server → client frame. `reply_to_method` is the method of the request a
/// `result` answers (a `result` payload's type depends on it).
pub fn check_server_frame(p: &Protocol, frame: &Value, reply_to_method: Option<&str>) -> Result<(), String> {
    check(p, &Ty::Ref("ServerFrame"), frame)?;
    let event = frame["event"].as_str().unwrap_or_default();
    let payload = &frame["payload"];
    let replying = !frame["reply_to"].is_null();
    let ty = match event {
        "hello_ok" => Ty::Ref("HelloOk"),
        "error" => Ty::Ref("ErrorPayload"),
        "result" => {
            let m = reply_to_method.ok_or("$.event: a `result` frame needs the method it answers to be checked")?;
            p.method(m).ok_or_else(|| format!("`{m}` is not a method of this protocol"))?.result
        }
        e => {
            if replying {
                return Err(format!("$.reply_to: event `{e}` answers no request but carries reply_to {}", frame["reply_to"]));
            }
            p.event(e).ok_or_else(|| format!("$.event: `{e}` is not an event of this protocol"))?.payload
        }
    };
    walk(p, &ty, payload, "$.payload")
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn expected(path: &str, what: &str, v: &Value) -> String {
    let shown = v.to_string();
    let shown = if shown.chars().count() > 80 { format!("{}…", shown.chars().take(80).collect::<String>()) } else { shown };
    format!("{path}: expected {what}, got {} {shown}", kind(v))
}

fn walk(p: &Protocol, ty: &Ty, v: &Value, path: &str) -> Result<(), String> {
    match *ty {
        Ty::Str => v.is_string().then_some(()).ok_or_else(|| expected(path, "a string", v)),
        Ty::UInt => v.as_u64().map(|_| ()).ok_or_else(|| expected(path, "a non-negative integer", v)),
        Ty::Int => (v.is_i64() || v.is_u64()).then_some(()).ok_or_else(|| expected(path, "an integer", v)),
        Ty::Bool => v.is_boolean().then_some(()).ok_or_else(|| expected(path, "a boolean", v)),
        Ty::Json => Ok(()),
        Ty::Const(s) => (v.as_str() == Some(s)).then_some(()).ok_or_else(|| expected(path, &format!("\"{s}\""), v)),
        Ty::Opt(t) => {
            if v.is_null() {
                Ok(())
            } else {
                walk(p, t, v, path)
            }
        }
        Ty::List(t) => {
            let items = v.as_array().ok_or_else(|| expected(path, "an array", v))?;
            items.iter().enumerate().try_for_each(|(i, x)| walk(p, t, x, &format!("{path}[{i}]")))
        }
        Ty::Map(t) => {
            let o = v.as_object().ok_or_else(|| expected(path, "an object", v))?;
            o.iter().try_for_each(|(k, x)| walk(p, t, x, &format!("{path}.{k}")))
        }
        Ty::Tuple(ts) => {
            let items =
                v.as_array().filter(|a| a.len() == ts.len()).ok_or_else(|| expected(path, &format!("an array of {}", ts.len()), v))?;
            ts.iter().zip(items).enumerate().try_for_each(|(i, (t, x))| walk(p, t, x, &format!("{path}[{i}]")))
        }
        Ty::Ref(name) => {
            let def = p.def(name).ok_or_else(|| format!("{path}: the table references `{name}` but does not define it"))?;
            match def.shape {
                Shape::Alias(t) => walk(p, &t, v, path),
                Shape::Enum(values) => match v.as_str() {
                    Some(s) if values.contains(&s) => Ok(()),
                    _ => Err(expected(path, &format!("one of {} ({name})", values.join(", ")), v)),
                },
                Shape::Object(fields) => fields_of(p, name, fields, None, v, path),
                Shape::Union { tag, variants } => {
                    let o = v.as_object().ok_or_else(|| expected(path, &format!("an object ({name})"), v))?;
                    let t = o.get(tag).and_then(Value::as_str).ok_or_else(|| format!("{path}: {name} has no string `{tag}`"))?;
                    let variant = variants.iter().find(|x| x.tag == t).ok_or_else(|| {
                        let known: Vec<&str> = variants.iter().map(|x| x.tag).collect();
                        format!("{path}.{tag}: `{t}` is not a {name} variant (known: {})", known.join(", "))
                    })?;
                    fields_of(p, &format!("{name}.{t}"), variant.fields, Some(tag), v, path)
                }
            }
        }
    }
}

fn fields_of(p: &Protocol, name: &str, fields: &[Field], tag: Option<&str>, v: &Value, path: &str) -> Result<(), String> {
    let o = v.as_object().ok_or_else(|| expected(path, &format!("an object ({name})"), v))?;
    for f in fields {
        match o.get(f.name) {
            Some(x) => walk(p, &f.ty, x, &format!("{path}.{}", f.name))?,
            None if f.optional => {}
            None => return Err(format!("{path}: {name} is missing required field `{}`", f.name)),
        }
    }
    match o.keys().find(|k| Some(k.as_str()) != tag && !fields.iter().any(|f| f.name == *k)) {
        Some(k) => Err(format!("{path}.{k}: {name} has no field `{k}` in the protocol table")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::PROTOCOL;

    #[test]
    fn objects_reject_unknown_and_missing_fields_and_wrong_types() {
        let ty = Ty::Ref("Usage");
        let ok = json!({"input_tokens": 1, "output_tokens": 2, "cache_read_tokens": 0, "cache_write_tokens": 0});
        assert_eq!(check(&PROTOCOL, &ty, &ok), Ok(()));
        let mut extra = ok.clone();
        extra["surprise"] = json!(1);
        assert!(check(&PROTOCOL, &ty, &extra).unwrap_err().contains("no field `surprise`"));
        let mut missing = ok.clone();
        missing.as_object_mut().unwrap().remove("output_tokens");
        assert!(check(&PROTOCOL, &ty, &missing).unwrap_err().contains("missing required field `output_tokens`"));
        let mut negative = ok;
        negative["input_tokens"] = json!(-1);
        assert!(check(&PROTOCOL, &ty, &negative).unwrap_err().starts_with("$.input_tokens: expected a non-negative integer"));
    }

    #[test]
    fn unions_pick_the_variant_by_tag_and_check_its_fields() {
        let ty = Ty::Ref("Transition");
        assert_eq!(check(&PROTOCOL, &ty, &json!({"transition": "completed"})), Ok(()));
        assert_eq!(check(&PROTOCOL, &ty, &json!({"transition": "aborted", "reason": "stop"})), Ok(()));
        assert!(check(&PROTOCOL, &ty, &json!({"transition": "aborted"})).unwrap_err().contains("missing required field `reason`"));
        assert!(check(&PROTOCOL, &ty, &json!({"transition": "completed", "reason": "x"})).unwrap_err().contains("no field `reason`"));
        assert!(check(&PROTOCOL, &ty, &json!({"transition": "teleported"})).unwrap_err().contains("not a Transition variant"));
    }

    #[test]
    fn frames_are_checked_against_their_method_or_event() {
        let prompt = json!({"v": 1, "id": 2, "method": "session.prompt", "params": {"text": "hi"}});
        assert_eq!(check_client_frame(&PROTOCOL, &prompt), Ok(()));
        let bad = json!({"v": 1, "id": 2, "method": "session.prompt", "params": {"txt": "hi"}});
        assert!(check_client_frame(&PROTOCOL, &bad).is_err());
        let fold = json!({"v": 1, "id": 3, "method": "session.fold"});
        assert_eq!(check_client_frame(&PROTOCOL, &fold), Ok(()));
        let unknown = json!({"v": 1, "id": 3, "method": "session.teleport"});
        assert!(check_client_frame(&PROTOCOL, &unknown).unwrap_err().contains("not a method"));
        let result = json!({"v": 1, "reply_to": 2, "event": "result", "payload": {"transition": {"transition": "completed"}}});
        assert_eq!(check_server_frame(&PROTOCOL, &result, Some("session.prompt")), Ok(()));
        assert!(check_server_frame(&PROTOCOL, &result, Some("session.subscribe")).is_err(), "a result is typed by its method");
        let idle = json!({"v": 1, "reply_to": 4, "event": "idle", "payload": {"turn": 1, "transition": {"transition": "completed"}}});
        assert!(check_server_frame(&PROTOCOL, &idle, None).unwrap_err().contains("answers no request"));
    }
}
