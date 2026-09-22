//! Python output (≥ 3.11): TypedDicts over the wire JSON (no conversion layer) and an asyncio
//! client. Class-syntax TypedDicts under `from __future__ import annotations`; a type with a field
//! named like a keyword (`from`, `class`) uses the functional form, whose values are evaluated
//! eagerly and so quote their references.

use super::{HEADER, pascal};
use crate::protocol::{Def, Field, Protocol, Shape, Ty};

const RUNTIME: &str = include_str!("../../templates/runtime.py");

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield",
];

fn header(o: &mut String, doc: &str) {
    o.push_str(&format!("# {HEADER}\n\"\"\"{doc}\"\"\"\n\nfrom __future__ import annotations\n\n"));
}

/// `eager`: the expression is evaluated at import time, so named types are quoted.
fn ty(t: &Ty, eager: bool) -> String {
    match *t {
        Ty::Str => "str".into(),
        Ty::UInt | Ty::Int => "int".into(),
        Ty::Bool => "bool".into(),
        Ty::Json => "Json".into(),
        Ty::Const(s) => format!("Literal[\"{s}\"]"),
        Ty::Opt(t) => format!("Optional[{}]", ty(t, eager)),
        Ty::List(t) => format!("list[{}]", ty(t, eager)),
        Ty::Map(t) => format!("dict[str, {}]", ty(t, eager)),
        Ty::Tuple(ts) => format!("tuple[{}]", ts.iter().map(|t| ty(t, eager)).collect::<Vec<_>>().join(", ")),
        Ty::Ref(n) if eager => format!("\"{n}\""),
        Ty::Ref(n) => n.to_string(),
    }
}

fn field_ty(f: &Field, eager: bool) -> String {
    let t = ty(&f.ty, eager);
    if f.optional { format!("NotRequired[{t}]") } else { t }
}

fn typed_dict(o: &mut String, name: &str, doc: &str, tag: Option<(&str, &str)>, fields: &[Field]) {
    let keyword = fields.iter().any(|f| KEYWORDS.contains(&f.name));
    if keyword {
        o.push_str(&format!("{name} = TypedDict(\n    \"{name}\",\n    {{\n"));
        if let Some((k, v)) = tag {
            o.push_str(&format!("        \"{k}\": Literal[\"{v}\"],\n"));
        }
        for f in fields {
            o.push_str(&format!("        \"{}\": {},\n", f.name, field_ty(f, true)));
        }
        o.push_str("    },\n)\n");
        if !doc.is_empty() {
            o.push_str(&format!("\"\"\"{doc}\"\"\"\n"));
        }
        return;
    }
    o.push_str(&format!("class {name}(TypedDict):\n"));
    if !doc.is_empty() {
        o.push_str(&format!("    \"\"\"{doc}\"\"\"\n"));
        if tag.is_some() || !fields.is_empty() {
            o.push('\n');
        }
    }
    if let Some((k, v)) = tag {
        o.push_str(&format!("    {k}: Literal[\"{v}\"]\n"));
    }
    for f in fields {
        o.push_str(&format!("    {}: {}\n", f.name, field_ty(f, false)));
        if !f.doc.is_empty() {
            o.push_str(&format!("    \"\"\"{}\"\"\"\n", f.doc));
        }
    }
    if doc.is_empty() && tag.is_none() && fields.is_empty() {
        o.push_str("    pass\n");
    }
}

fn alternatives(o: &mut String, name: &str, head: &str, alts: &[String], doc: &str) {
    o.push_str(&format!("{name} = {head}[\n"));
    for a in alts {
        o.push_str(&format!("    {a},\n"));
    }
    o.push_str("]\n");
    if !doc.is_empty() {
        o.push_str(&format!("\"\"\"{doc}\"\"\"\n"));
    }
}

fn def(o: &mut String, d: &Def) {
    match d.shape {
        Shape::Alias(t) => {
            o.push_str(&format!("{} = {}\n", d.name, ty(&t, true)));
            if !d.doc.is_empty() {
                o.push_str(&format!("\"\"\"{}\"\"\"\n", d.doc));
            }
        }
        Shape::Enum(values) => alternatives(o, d.name, "Literal", &values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>(), d.doc),
        Shape::Object(fields) => typed_dict(o, d.name, d.doc, None, fields),
        Shape::Union { tag, variants } => {
            let names: Vec<String> = variants.iter().map(|v| format!("{}{}", d.name, pascal(v.tag))).collect();
            for (v, n) in variants.iter().zip(&names) {
                typed_dict(o, n, "", Some((tag, v.tag)), v.fields);
                o.push_str("\n\n");
            }
            alternatives(o, d.name, "Union", &names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>(), d.doc);
        }
    }
}

pub fn protocol(p: &Protocol) -> String {
    let mut o = String::new();
    header(
        &mut o,
        "The aho rpc protocol (08, 12) as typed dicts over the wire JSON.\n\nReaders ignore unknown fields, so a newer server's additive fields never break these types.\n",
    );
    o.push_str("from typing import Any, Literal, NotRequired, Optional, TypedDict, Union\n\n");
    o.push_str(&format!("VERSION_MIN = {}\nVERSION_MAX = {}\n\nJson = Any\n\"\"\"Any JSON value.\"\"\"\n", p.version_min, p.version_max));
    for d in p.defs {
        o.push_str("\n\n");
        def(&mut o, d);
    }
    o.push_str("\n\nMETHODS: tuple[str, ...] = (\n");
    for m in p.methods {
        o.push_str(&format!("    \"{}\",\n", m.name));
    }
    o.push_str(")\n\"\"\"Every method, in `hello_ok.capabilities` order.\"\"\"\n");
    let names: Vec<String> = p.events.iter().map(|e| format!("Event{}", pascal(e.name))).collect();
    for (e, n) in p.events.iter().zip(&names) {
        o.push_str(&format!(
            "\n\nclass {n}(TypedDict):\n    \"\"\"{}\"\"\"\n\n    v: int\n    reply_to: None\n    event: Literal[\"{}\"]\n    payload: {}\n",
            e.doc,
            e.name,
            ty(&e.payload, false)
        ));
    }
    o.push_str("\n\n");
    alternatives(
        &mut o,
        "Event",
        "Union",
        &names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>(),
        "A server-initiated frame: what `Client.events()` yields.",
    );
    o
}

pub fn runtime() -> String {
    let mut o = String::new();
    header(&mut o, "Transport: one child process speaking NDJSON on stdio, replies matched by id, events as an async iterator.");
    o.push_str(RUNTIME);
    o
}

fn takes_params(p: &Protocol, params: Option<&str>) -> Option<(&'static str, bool)> {
    let d = p.def(params?)?;
    Some((d.name, matches!(d.shape, Shape::Object(fs) if fs.iter().any(|f| !f.optional))))
}

pub fn client(p: &Protocol) -> String {
    let mut o = String::new();
    header(&mut o, "A typed asyncio client for `aho rpc` (08): one coroutine per rpc method, the Session API 1:1.");
    o.push_str(
        "from typing import Any, AsyncIterator, Optional, Unpack, cast

from . import protocol as p
from .runtime import Connection, SpawnOptions


class Client:
    \"\"\"One connection to one aho session.\"\"\"

    def __init__(self, connection: Connection) -> None:
        self.connection = connection

    @classmethod
    async def spawn(cls, **options: Unpack[SpawnOptions]) -> Client:
        \"\"\"Spawns `aho rpc` (or `command`) and completes the `hello` handshake.\"\"\"
        return cls(await Connection.spawn(**options))

    @property
    def hello(self) -> p.HelloOk:
        \"\"\"The server's `hello_ok`: negotiated version, methods, session id (hex), log head.\"\"\"
        return self.connection.hello

    def events(self) -> AsyncIterator[p.Event]:
        \"\"\"Server-initiated frames in wire order since connect; ends when the connection closes. One consumer.\"\"\"
        return self.connection.events()

    async def close(self) -> Optional[int]:
        \"\"\"Closes stdin; the server answers what is in flight and exits. Returns its exit code.\"\"\"
        return await self.connection.close()

    async def __aenter__(self) -> Client:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def request(self, method: str, params: Any = None) -> Any:
        \"\"\"Any method by wire name.\"\"\"
        return await self.connection.request(method, params)
",
    );
    for m in p.methods {
        let result = match m.result {
            Ty::Ref(n) => format!("p.{n}"),
            _ => "p.Json".to_string(),
        };
        let (signature, arg) = match takes_params(p, m.params) {
            Some((def, true)) => (format!(", params: p.{def}"), "params"),
            Some((def, false)) => (format!(", params: Optional[p.{def}] = None"), "params or {}"),
            None => (String::new(), "{}"),
        };
        o.push_str(&format!(
            "\n    async def {}(self{signature}) -> {result}:\n        \"\"\"{}\"\"\"\n        return cast(\"{result}\", await self.connection.request(\"{}\", {arg}))\n",
            m.sdk, m.doc, m.name
        ));
    }
    o
}

pub fn init() -> String {
    let mut o = String::new();
    header(&mut o, "The aho Python SDK: `await Client.spawn(project=..., sets={...})`, then one coroutine per rpc method.");
    o.push_str(
        "from . import protocol
from .client import Client
from .runtime import Connection, ConnectionClosed, RpcError, SpawnOptions, argv

__all__ = [\"Client\", \"Connection\", \"ConnectionClosed\", \"RpcError\", \"SpawnOptions\", \"argv\", \"protocol\"]
",
    );
    o
}
