//! TypeScript output. Only erasable syntax (no enums, namespaces, or parameter properties) and
//! `.ts` import specifiers, so node ≥ 22.6 runs the sources with `--experimental-strip-types`
//! and `tsc` checks them with `allowImportingTsExtensions`.

use super::{HEADER, camel, pascal};
use crate::protocol::{Def, Field, Protocol, Shape, Ty};

const RUNTIME: &str = include_str!("../../templates/runtime.ts");

fn header(o: &mut String, what: &str) {
    o.push_str(&format!("// {HEADER}\n//\n// {what}\n\n"));
}

/// `ns` prefixes named types (`P.` in the client, empty in protocol.ts).
fn ty(t: &Ty, ns: &str) -> String {
    match *t {
        Ty::Str => "string".into(),
        Ty::UInt | Ty::Int => "number".into(),
        Ty::Bool => "boolean".into(),
        Ty::Json => format!("{ns}Json"),
        Ty::Const(s) => format!("\"{s}\""),
        Ty::Opt(t) => format!("{} | null", ty(t, ns)),
        Ty::List(t) => format!("Array<{}>", ty(t, ns)),
        Ty::Map(t) => format!("{{ [key: string]: {} }}", ty(t, ns)),
        Ty::Tuple(ts) => format!("[{}]", ts.iter().map(|t| ty(t, ns)).collect::<Vec<_>>().join(", ")),
        Ty::Ref(n) => format!("{ns}{n}"),
    }
}

fn doc(o: &mut String, indent: &str, text: &str) {
    if !text.is_empty() {
        o.push_str(&format!("{indent}/** {text} */\n"));
    }
}

fn interface(o: &mut String, name: &str, tag: Option<(&str, &str)>, fields: &[Field]) {
    o.push_str(&format!("export interface {name} {{\n"));
    if let Some((k, v)) = tag {
        o.push_str(&format!("  {k}: \"{v}\";\n"));
    }
    for f in fields {
        doc(o, "  ", f.doc);
        let q = if f.optional { "?" } else { "" };
        o.push_str(&format!("  {}{q}: {};\n", f.name, ty(&f.ty, "")));
    }
    o.push_str("}\n");
}

fn alternatives(o: &mut String, name: &str, alts: &[String]) {
    o.push_str(&format!("export type {name} =\n"));
    for (i, a) in alts.iter().enumerate() {
        o.push_str(&format!("  | {a}{}\n", if i + 1 == alts.len() { ";" } else { "" }));
    }
}

fn def(o: &mut String, d: &Def) {
    match d.shape {
        Shape::Alias(t) => {
            doc(o, "", d.doc);
            o.push_str(&format!("export type {} = {};\n", d.name, ty(&t, "")));
        }
        Shape::Enum(values) => {
            doc(o, "", d.doc);
            alternatives(o, d.name, &values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>());
        }
        Shape::Object([]) => {
            doc(o, "", d.doc);
            o.push_str(&format!("export type {} = Record<string, never>;\n", d.name));
        }
        Shape::Object(fields) => {
            doc(o, "", d.doc);
            interface(o, d.name, None, fields);
        }
        Shape::Union { tag, variants } => {
            let names: Vec<String> = variants.iter().map(|v| format!("{}{}", d.name, pascal(v.tag))).collect();
            for (v, n) in variants.iter().zip(&names) {
                interface(o, n, Some((tag, v.tag)), v.fields);
                o.push('\n');
            }
            doc(o, "", d.doc);
            alternatives(o, d.name, &names);
        }
    }
}

pub fn protocol(p: &Protocol) -> String {
    let mut o = String::new();
    header(&mut o, "The aho rpc protocol as types. Readers ignore unknown fields, so a newer server's additive fields never break them.");
    o.push_str(&format!("export const VERSION_MIN = {};\nexport const VERSION_MAX = {};\n\n", p.version_min, p.version_max));
    o.push_str("/** Any JSON value. */\nexport type Json = null | boolean | number | string | Json[] | { [key: string]: Json };\n");
    for d in p.defs {
        o.push('\n');
        def(&mut o, d);
    }
    o.push_str("\n/** Every method with its params and result. */\nexport interface Methods {\n");
    for m in p.methods {
        doc(&mut o, "  ", m.doc);
        o.push_str(&format!("  \"{}\": {{ params: {}; result: {} }};\n", m.name, m.params.unwrap_or("Empty"), ty(&m.result, "")));
    }
    o.push_str("}\n\nexport type Method = keyof Methods;\n\n/** Every method, in `hello_ok.capabilities` order. */\nexport const METHODS: readonly Method[] = [\n");
    for m in p.methods {
        o.push_str(&format!("  \"{}\",\n", m.name));
    }
    o.push_str("];\n");
    let names: Vec<String> = p.events.iter().map(|e| format!("Event{}", pascal(e.name))).collect();
    for (e, n) in p.events.iter().zip(&names) {
        o.push('\n');
        doc(&mut o, "", e.doc);
        o.push_str(&format!(
            "export interface {n} {{\n  v: number;\n  reply_to: null;\n  event: \"{}\";\n  payload: {};\n}}\n",
            e.name,
            ty(&e.payload, "")
        ));
    }
    o.push_str("\n/** A server-initiated frame: what `Client.events()` yields. */\n");
    alternatives(&mut o, "Event", &names);
    o
}

pub fn runtime() -> String {
    let mut o = String::new();
    header(&mut o, "Transport: one child process speaking NDJSON on stdio, replies matched by id, events as an async iterator.");
    o.push_str(RUNTIME);
    o
}

fn takes_params(p: &Protocol, params: Option<&str>) -> Option<(&'static str, bool)> {
    let name = params?;
    let required = p.def(name).is_some_and(|d| matches!(d.shape, Shape::Object(fs) if fs.iter().any(|f| !f.optional)));
    p.def(name).map(|d| (d.name, required))
}

pub fn client(p: &Protocol) -> String {
    let mut o = String::new();
    header(&mut o, "A typed client for `aho rpc`: one method per rpc method, the Session API 1:1.");
    o.push_str(
        "import { Connection } from \"./runtime.ts\";
import type { SpawnOptions } from \"./runtime.ts\";
import type * as P from \"./protocol.ts\";

/** One connection to one aho session. */
export class Client {
  readonly connection: Connection;

  constructor(connection: Connection) {
    this.connection = connection;
  }

  /** Spawns `aho rpc` (or `options.command`) and completes the `hello` handshake. */
  static async spawn(options: SpawnOptions = {}): Promise<Client> {
    return new Client(await Connection.spawn(options));
  }

  /** The server's `hello_ok`: negotiated version, methods, session id (hex), log head. */
  get hello(): P.HelloOk {
    return this.connection.hello;
  }

  /** Server-initiated frames in wire order since connect; ends when the connection closes. One consumer. */
  events(): AsyncIterableIterator<P.Event> {
    return this.connection.events();
  }

  /** Ends stdin; the server answers what is in flight and exits. Resolves its exit code. */
  close(): Promise<number | null> {
    return this.connection.close();
  }

  /** Any method by wire name. */
  request<M extends P.Method>(method: M, params: P.Methods[M][\"params\"]): Promise<P.Methods[M][\"result\"]> {
    return this.connection.request(method, params) as Promise<P.Methods[M][\"result\"]>;
  }
",
    );
    for m in p.methods {
        o.push('\n');
        doc(&mut o, "  ", m.doc);
        let result = ty(&m.result, "P.");
        let name = camel(m.sdk);
        let (signature, arg) = match takes_params(p, m.params) {
            Some((def, true)) => (format!("params: P.{def}"), "params"),
            Some((def, false)) => (format!("params: P.{def} = {{}}"), "params"),
            None => (String::new(), "{}"),
        };
        o.push_str(&format!("  {name}({signature}): Promise<{result}> {{\n    return this.request(\"{}\", {arg});\n  }}\n", m.name));
    }
    o.push_str("}\n");
    o
}

pub fn index() -> String {
    let mut o = String::new();
    header(&mut o, "The aho TypeScript SDK: `Client.spawn({ project, sets })`, then one method per rpc method.");
    o.push_str(
        "export { Client } from \"./client.ts\";
export { Connection, ConnectionClosed, RpcError, argv } from \"./runtime.ts\";
export type { SpawnOptions } from \"./runtime.ts\";
export * from \"./protocol.ts\";
",
    );
    o
}
