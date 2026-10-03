//! Test fixture: a tiny language server speaking LSP over stdio. Not shipped behavior; the
//! integration tests in `tests/write_diagnostics.rs` start it as a configured server command.
//!
//! Its analysis: every line containing `ERROR` gets an error at that column, `WARN` a warning,
//! `INFO` an information. Full-text sync, versioned pushes on every `didOpen`/`didChange`.
//! Flags:
//! * `--pull`: offers `diagnosticProvider` and answers `textDocument/diagnostic` instead of
//!   pushing its analysis;
//! * `--check=<ms>`: wants `didSave`; on each one runs a "check" announced as work-done progress
//!   that lasts `<ms>` and then pushes an error for every line containing `BORROW`;
//! * `--slow=<ms>`: takes `<ms>` to answer `initialize` (a cold start).
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

fn send(out: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

/// The next framed message; `None` at end of input.
fn receive(input: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(n) = line.strip_prefix("Content-Length:") {
            length = n.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

fn diagnostics(text: &str, marks: &[(&str, u64)]) -> Vec<Value> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for (mark, severity) in marks {
            if let Some(at) = line.find(mark) {
                let col = line[..at].encode_utf16().count();
                out.push(json!({
                    "range": {"start": {"line": n, "character": col}, "end": {"line": n, "character": col + mark.len()}},
                    "severity": severity,
                    "source": "fixture",
                    "message": format!("found {mark}"),
                }));
            }
        }
    }
    out
}

const ANALYSIS: &[(&str, u64)] = &[("ERROR", 1), ("WARN", 2), ("INFO", 3)];
const CHECK: &[(&str, u64)] = &[("BORROW", 1)];

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pull = args.iter().any(|a| a == "--pull");
    let check_ms = args.iter().find_map(|a| a.strip_prefix("--check=")).and_then(|ms| ms.parse::<u64>().ok());
    let slow_ms = args.iter().find_map(|a| a.strip_prefix("--slow=")).and_then(|ms| ms.parse::<u64>().ok()).unwrap_or(0);
    let mut input = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut checks = 0u64;
    while let Some(msg) = receive(&mut input)? {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = msg.get("id").cloned();
        let uri = params.pointer("/textDocument/uri").and_then(Value::as_str).unwrap_or("").to_string();
        match method {
            "initialize" => {
                std::thread::sleep(Duration::from_millis(slow_ms));
                let mut caps = json!({"textDocumentSync": {"openClose": true, "change": 1}});
                if check_ms.is_some() {
                    caps["textDocumentSync"]["save"] = json!({"includeText": false});
                }
                if pull {
                    caps["diagnosticProvider"] = json!({"interFileDependencies": false, "workspaceDiagnostics": false});
                }
                send(&mut out, &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": caps}}))?;
            }
            "shutdown" => send(&mut out, &json!({"jsonrpc": "2.0", "id": id, "result": null}))?,
            "exit" => return Ok(()),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let text = params
                    .pointer("/textDocument/text")
                    .or_else(|| params.pointer("/contentChanges/0/text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let version = params.pointer("/textDocument/version").cloned().unwrap_or(Value::Null);
                if !pull {
                    let items = diagnostics(&text, ANALYSIS);
                    let publish = json!({"uri": uri, "version": version, "diagnostics": items});
                    send(&mut out, &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": publish}))?;
                }
                docs.insert(uri, text);
            }
            "textDocument/diagnostic" => {
                let items = diagnostics(docs.get(&uri).map(String::as_str).unwrap_or(""), ANALYSIS);
                send(&mut out, &json!({"jsonrpc": "2.0", "id": id, "result": {"kind": "full", "items": items}}))?;
            }
            "textDocument/didSave" => {
                let Some(ms) = check_ms else { continue };
                checks += 1;
                let token = format!("check/{checks}");
                let progress = |kind: &str| json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": token, "value": {"kind": kind, "title": "check"}}});
                send(&mut out, &progress("begin"))?;
                std::thread::sleep(Duration::from_millis(ms));
                let text = docs.get(&uri).map(String::as_str).unwrap_or("");
                let mut items = diagnostics(text, CHECK);
                if !pull {
                    // A pushing server's list is whole: its own analysis plus the check's.
                    items.extend(diagnostics(text, ANALYSIS));
                }
                send(&mut out, &progress("end"))?;
                let publish = json!({"uri": uri, "diagnostics": items});
                send(&mut out, &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": publish}))?;
            }
            _ => {
                if let Some(id) = id.filter(|_| !method.is_empty()) {
                    send(&mut out, &json!({"jsonrpc": "2.0", "id": id, "result": null}))?;
                }
            }
        }
    }
    Ok(())
}
