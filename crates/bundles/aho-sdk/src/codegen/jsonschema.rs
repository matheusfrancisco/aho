//! The protocol as JSON Schema (draft 2020-12). `$defs` holds every table type; the root
//! validates any frame (`ClientMessage` per method, `ServerMessage` per event); `x-aho-methods`
//! pairs each method with its params and result (a reply's type depends on the request it
//! answers, which JSON Schema cannot say). Objects are open: readers ignore unknown fields.
//! Keys are emitted sorted so the file is byte-stable whatever map order serde_json was built with.

use serde_json::{Map, Value, json};

use super::HEADER;
use crate::protocol::{Def, Field, Protocol, Shape, Ty};

fn r(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn ty(t: &Ty) -> Value {
    match *t {
        Ty::Str => json!({"type": "string"}),
        Ty::UInt => json!({"type": "integer", "minimum": 0}),
        Ty::Int => json!({"type": "integer"}),
        Ty::Bool => json!({"type": "boolean"}),
        Ty::Json => json!({}),
        Ty::Const(s) => json!({"const": s}),
        Ty::Opt(t) => json!({"anyOf": [ty(t), {"type": "null"}]}),
        Ty::List(t) => json!({"type": "array", "items": ty(t)}),
        Ty::Map(t) => json!({"type": "object", "additionalProperties": ty(t)}),
        Ty::Tuple(ts) => json!({
            "type": "array",
            "prefixItems": ts.iter().map(ty).collect::<Vec<_>>(),
            "minItems": ts.len(),
            "maxItems": ts.len(),
        }),
        Ty::Ref(n) => r(n),
    }
}

fn described(mut v: Value, doc: &str) -> Value {
    if let Some(o) = v.as_object_mut().filter(|_| !doc.is_empty()) {
        o.insert("description".into(), json!(doc));
    }
    v
}

fn object(fields: &[Field], tag: Option<(&str, &str)>) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    if let Some((k, v)) = tag {
        props.insert(k.into(), json!({"const": v}));
        required.push(json!(k));
    }
    for f in fields {
        props.insert(f.name.into(), described(ty(&f.ty), f.doc));
        if !f.optional {
            required.push(json!(f.name));
        }
    }
    json!({"type": "object", "properties": props, "required": required})
}

fn def(d: &Def) -> Value {
    let v = match d.shape {
        Shape::Alias(t) => ty(&t),
        Shape::Enum(values) => json!({"type": "string", "enum": values}),
        Shape::Object(fields) => object(fields, None),
        Shape::Union { tag, variants } => {
            json!({"oneOf": variants.iter().map(|v| object(v.fields, Some((tag, v.tag)))).collect::<Vec<_>>()})
        }
    };
    let mut v = described(v, d.doc);
    if let Some(o) = v.as_object_mut() {
        o.insert("title".into(), json!(d.name));
    }
    v
}

fn params_required(p: &Protocol, params: Option<&str>) -> bool {
    params.and_then(|n| p.def(n)).is_some_and(|d| matches!(d.shape, Shape::Object(fs) if fs.iter().any(|f| !f.optional)))
}

fn client_message(p: &Protocol) -> Value {
    let frame = |method: &str, params: Value, required: bool| {
        let mut req = vec![json!("v"), json!("method")];
        if required {
            req.push(json!("params"));
        }
        json!({
            "type": "object",
            "properties": {
                "v": {"type": "integer", "minimum": 0},
                "id": {"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}]},
                "method": {"const": method},
                "params": params,
            },
            "required": req,
        })
    };
    let mut all = vec![frame("hello", r("Hello"), true)];
    for m in p.methods {
        let params = match m.params {
            Some(n) => json!({"anyOf": [r(n), {"type": "null"}]}),
            None => json!({"anyOf": [r("Empty"), {"type": "null"}]}),
        };
        all.push(frame(m.name, params, params_required(p, m.params)));
    }
    json!({"title": "ClientMessage", "description": "Any client → server frame.", "oneOf": all})
}

fn server_message(p: &Protocol) -> Value {
    let frame = |event: &str, reply_to: Value, payload: Value| {
        json!({
            "type": "object",
            "properties": {
                "v": {"type": "integer", "minimum": 0},
                "reply_to": reply_to,
                "event": {"const": event},
                "payload": payload,
            },
            "required": ["v", "reply_to", "event", "payload"],
        })
    };
    let id = json!({"type": "integer", "minimum": 0});
    let mut all = vec![
        frame("hello_ok", id.clone(), r("HelloOk")),
        described(frame("result", id.clone(), json!({})), "The reply to a request; its payload is the method's result (x-aho-methods)."),
        frame("error", json!({"anyOf": [id, {"type": "null"}]}), r("ErrorPayload")),
    ];
    for e in p.events.iter().filter(|e| e.name != "error") {
        all.push(frame(e.name, json!({"type": "null"}), ty(&e.payload)));
    }
    json!({"title": "ServerMessage", "description": "Any server → client frame.", "oneOf": all})
}

pub fn render(p: &Protocol) -> String {
    let mut defs = Map::new();
    for d in p.defs {
        defs.insert(d.name.into(), def(d));
    }
    defs.insert("ClientMessage".into(), client_message(p));
    defs.insert("ServerMessage".into(), server_message(p));
    let methods: Map<String, Value> = p
        .methods
        .iter()
        .map(|m| {
            let params = m.params.map(r).unwrap_or_else(|| r("Empty"));
            (m.name.to_string(), json!({"sdk": m.sdk, "description": m.doc, "params": params, "result": ty(&m.result)}))
        })
        .collect();
    let events: Map<String, Value> =
        p.events.iter().map(|e| (e.name.to_string(), json!({"description": e.doc, "payload": ty(&e.payload)}))).collect();
    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "urn:aho:rpc",
        "$comment": HEADER,
        "title": "aho rpc protocol",
        "description": "NDJSON frames over stdio or a unix socket. The first client frame is `hello`; every request carries `id` and the server answers it with a `result` or `error` frame with `reply_to = id`; events carry `reply_to: null`. Readers ignore unknown fields.",
        "x-aho-version": {"min": p.version_min, "max": p.version_max},
        "x-aho-methods": methods,
        "x-aho-events": events,
        "oneOf": [r("ClientMessage"), r("ServerMessage")],
        "$defs": defs,
    });
    let mut out = String::new();
    pretty(&doc, 0, &mut out);
    out.push('\n');
    out
}

fn pad(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n("  ", n));
}

/// Pretty JSON with object keys sorted.
fn pretty(v: &Value, indent: usize, out: &mut String) {
    match v {
        Value::Array(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                pad(out, indent + 1);
                pretty(x, indent + 1, out);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            pad(out, indent);
            out.push(']');
        }
        Value::Object(o) if !o.is_empty() => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push_str("{\n");
            for (i, k) in keys.iter().enumerate() {
                pad(out, indent + 1);
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push_str(": ");
                pretty(&o[k.as_str()], indent + 1, out);
                out.push_str(if i + 1 < keys.len() { ",\n" } else { "\n" });
            }
            pad(out, indent);
            out.push('}');
        }
        scalar_or_empty => out.push_str(&scalar_or_empty.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL;

    #[test]
    fn keys_are_sorted_and_the_document_parses_back() {
        let text = render(&PROTOCOL);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["$defs"]["Hash"]["type"], "string");
        assert_eq!(v["x-aho-methods"]["session.prompt"]["result"]["$ref"], "#/$defs/PromptResult");
        let top: Vec<&str> = text.lines().filter(|l| l.starts_with("  \"")).map(|l| l.trim_start()).collect();
        let mut sorted = top.clone();
        sorted.sort();
        assert_eq!(top, sorted);
    }
}
