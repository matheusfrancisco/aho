//! Test fixture: a tiny MCP server speaking JSON-RPC 2.0 over stdio. Not shipped behavior; the
//! integration tests in `tests/mcp.rs` start it through `mcp.servers.<name>.cmd`.
//!
//! `FIXTURE_PROFILE` selects the behavior:
//! * `full` (default): tools over two pages, resources over two pages, one template; pings the
//!   client before answering the first `tools/list`; checks `initialize` then `initialized`.
//! * `dup`: lists the tool `a` twice.
//! * `badversion`: answers `initialize` with protocol `1999-01-01`.
//! * `crash`: writes to stderr and exits on `initialize`.
//! * `notools`: advertises resources only.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn send(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn result(id: &Value, r: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": r}));
}

fn error(id: &Value, code: i64, message: &str) {
    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
}

fn text(t: &str) -> Value {
    json!({"content": [{"type": "text", "text": t}]})
}

fn tools_page(profile: &str, cursor: Option<&str>) -> Value {
    if profile == "dup" {
        return json!({"tools": [
            {"name": "a", "description": "first", "inputSchema": {"type": "object"}},
            {"name": "a", "description": "again", "inputSchema": {"type": "object"}}
        ]});
    }
    match cursor {
        None => json!({"tools": [
            {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
            {"name": "add", "title": "Add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}},
            {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}}
        ], "nextCursor": "p2"}),
        Some(_) => json!({"tools": [
            {"name": "env", "description": "Read an environment variable", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
            {"name": "weird.name/with spaces", "description": "Needs a hashed name"},
            {"name": "image", "description": "Returns an image", "inputSchema": {"type": "object"}},
            {"name": "slow", "description": "Sleeps", "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}}
        ]}),
    }
}

fn call(params: &Value) -> Result<Value, (i64, String)> {
    let args = &params["arguments"];
    Ok(match params["name"].as_str().unwrap_or("") {
        "echo" => text(args["text"].as_str().unwrap_or("")),
        "add" => {
            let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
            json!({"content": [{"type": "text", "text": sum.to_string()}], "structuredContent": {"sum": sum}})
        }
        "fail" => json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        "env" => text(&std::env::var(args["name"].as_str().unwrap_or("")).unwrap_or_else(|_| "(unset)".into())),
        "weird.name/with spaces" => text("ok"),
        "image" => json!({"content": [{"type": "image", "data": "aGk=", "mimeType": "image/gif"}]}),
        "slow" => {
            std::thread::sleep(std::time::Duration::from_millis(args["ms"].as_u64().unwrap_or(1000)));
            text("done")
        }
        other => return Err((-32602, format!("unknown tool {other}"))),
    })
}

fn main() {
    let profile = std::env::var("FIXTURE_PROFILE").unwrap_or_else(|_| "full".into());
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let (mut initialize_seen, mut initialized, mut pinged) = (false, false, false);
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let Some(id) = msg.get("id").cloned() else {
            if method == "notifications/initialized" {
                if !initialize_seen {
                    eprintln!("initialized before initialize");
                    std::process::exit(4);
                }
                initialized = true;
            }
            continue;
        };
        if method.is_empty() {
            continue; // a response to one of our requests
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "initialize" => {
                if profile == "crash" {
                    eprintln!("fixture crashed on purpose");
                    std::process::exit(3);
                }
                if params["protocolVersion"] != "2025-06-18" {
                    error(&id, -32602, "unsupported protocol version");
                    continue;
                }
                initialize_seen = true;
                let version = if profile == "badversion" { "1999-01-01" } else { "2025-06-18" };
                let caps =
                    if profile == "notools" { json!({"resources": {}}) } else { json!({"tools": {"listChanged": true}, "resources": {}}) };
                result(
                    &id,
                    json!({"protocolVersion": version, "capabilities": caps, "serverInfo": {"name": "fixture", "version": "1.0"}, "instructions": "Fixture server."}),
                );
            }
            _ if !initialized => error(&id, -32002, "server not initialized"),
            "tools/list" => {
                if profile == "full" && !pinged {
                    pinged = true;
                    send(&json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "listing"}}));
                    send(&json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}));
                    loop {
                        let Some(Ok(l)) = lines.next() else { std::process::exit(5) };
                        let Ok(m) = serde_json::from_str::<Value>(&l) else { continue };
                        if m["id"] == "srv-1" {
                            if m.get("result").is_none() {
                                eprintln!("client did not answer ping");
                                std::process::exit(6);
                            }
                            break;
                        }
                    }
                }
                result(&id, tools_page(&profile, params["cursor"].as_str()));
            }
            "tools/call" => match call(&params) {
                Ok(r) => result(&id, r),
                Err((code, m)) => error(&id, code, &m),
            },
            "resources/list" => match params["cursor"].as_str() {
                None => result(
                    &id,
                    json!({"resources": [{"uri": "mem://readme", "name": "readme", "mimeType": "text/plain", "description": "The readme"}], "nextCursor": "r2"}),
                ),
                Some(_) => result(&id, json!({"resources": [{"uri": "mem://logo", "name": "logo", "mimeType": "image/png"}]})),
            },
            "resources/templates/list" => result(
                &id,
                json!({"resourceTemplates": [{"uriTemplate": "mem://notes/{id}", "name": "note", "description": "A note by id"}]}),
            ),
            "resources/read" => match params["uri"].as_str().unwrap_or("") {
                "mem://readme" => {
                    result(&id, json!({"contents": [{"uri": "mem://readme", "mimeType": "text/plain", "text": "hello from the fixture"}]}))
                }
                "mem://logo" => result(&id, json!({"contents": [{"uri": "mem://logo", "mimeType": "image/png", "blob": "iVBORw0KGgo="}]})),
                other => error(&id, -32002, &format!("resource not found: {other}")),
            },
            _ => error(&id, -32601, "method not found"),
        }
    }
}
