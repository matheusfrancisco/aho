//! MCP JSON → `aho_mcp` types, and JSON-RPC message classes. Pure and transport-agnostic: the
//! same shapes `mcp-stdio` reads (this module should move into `aho-mcp` so both transports
//! share it).

use aho_mcp::{CallResult, McpError, RemoteTool, Resource, ResourceContents, ResourceTemplate, SUPPORTED_VERSIONS};
use aho_types::{RootSchema, Value};
use serde_json::json;

/// The `initialize` answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub protocol_version: String,
    pub name: String,
    pub version: String,
    pub instructions: Option<String>,
    pub tools: bool,
    /// The server promised `notifications/tools/list_changed`.
    pub tools_list_changed: bool,
    pub resources: bool,
}

impl ServerInfo {
    /// Reads an `InitializeResult`; a protocol revision aho does not speak fails the handshake.
    pub fn from_initialize(server: &str, init: &Value) -> Result<ServerInfo, McpError> {
        let version = init.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(McpError::Handshake(format!(
                "MCP server `{server}` speaks protocol `{version}`, which aho does not support ({}); aho did not connect to it.",
                SUPPORTED_VERSIONS.join(", ")
            )));
        }
        let caps = init.get("capabilities").cloned().unwrap_or(Value::Null);
        let info = init.get("serverInfo").cloned().unwrap_or(Value::Null);
        Ok(ServerInfo {
            protocol_version: version.to_string(),
            name: info.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            version: info.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
            instructions: opt_str(init, "instructions"),
            tools: caps.get("tools").is_some(),
            tools_list_changed: caps.pointer("/tools/listChanged").and_then(Value::as_bool).unwrap_or(false),
            resources: caps.get("resources").is_some(),
        })
    }
}

pub fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// One `tools/list` entry. A missing schema becomes `{"type": "object"}`.
pub fn parse_tool(server: &str, t: Value) -> Result<RemoteTool, McpError> {
    let Some(name) = t.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else {
        return Err(McpError::Transport(format!("MCP server `{server}` listed a tool without a name; its tool list was not used")));
    };
    let mut schema = match t.get("inputSchema") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({"type": "object"}),
    };
    if schema.get("type").is_none() {
        schema["type"] = json!("object");
    }
    Ok(RemoteTool {
        raw_name: name.to_string(),
        title: opt_str(&t, "title").or_else(|| t.get("annotations").and_then(|a| opt_str(a, "title"))),
        description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        schema: RootSchema(schema),
    })
}

/// A `tools/call` result.
pub fn parse_call(r: &Value) -> CallResult {
    CallResult {
        content: r.get("content").and_then(Value::as_array).cloned().unwrap_or_default(),
        structured: r.get("structuredContent").cloned().filter(|v| !v.is_null()),
        is_error: r.get("isError").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// A `resources/list` entry; `None` without a `uri`.
pub fn parse_resource(r: &Value) -> Option<Resource> {
    Some(Resource {
        uri: r.get("uri")?.as_str()?.to_string(),
        name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        title: opt_str(r, "title"),
        description: opt_str(r, "description"),
        mime: opt_str(r, "mimeType"),
    })
}

/// A `resources/templates/list` entry; `None` without a `uriTemplate`.
pub fn parse_template(r: &Value) -> Option<ResourceTemplate> {
    Some(ResourceTemplate {
        uri_template: r.get("uriTemplate")?.as_str()?.to_string(),
        name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        title: opt_str(r, "title"),
        description: opt_str(r, "description"),
        mime: opt_str(r, "mimeType"),
    })
}

/// A `resources/read` result; entries without their own `uri` take the requested one.
pub fn parse_contents(uri: &str, r: &Value) -> Vec<ResourceContents> {
    let contents = r.get("contents").and_then(Value::as_array).cloned().unwrap_or_default();
    contents
        .into_iter()
        .map(|c| {
            let uri = c.get("uri").and_then(Value::as_str).unwrap_or(uri).to_string();
            let mime = opt_str(&c, "mimeType");
            match (c.get("text").and_then(Value::as_str), c.get("blob").and_then(Value::as_str)) {
                (Some(t), _) => ResourceContents::Text { uri, mime, text: t.to_string() },
                (None, Some(b)) => ResourceContents::Blob { uri, mime, base64: b.to_string() },
                (None, None) => ResourceContents::Text { uri, mime, text: String::new() },
            }
        })
        .collect()
}

/// A JSON-RPC message from the server.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Response { id: Value, result: Result<Value, (i64, String)> },
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

/// Classifies one message or a batch (arrays flatten). Anything that is none of the three is dropped.
pub fn classify(v: Value) -> Vec<Incoming> {
    fn one(v: Value) -> Option<Incoming> {
        let Value::Object(mut m) = v else { return None };
        let params = m.remove("params").unwrap_or(Value::Null);
        match (m.remove("method"), m.remove("id")) {
            (Some(Value::String(method)), Some(id)) if !id.is_null() => Some(Incoming::Request { id, method, params }),
            (Some(Value::String(method)), _) => Some(Incoming::Notification { method, params }),
            (None, Some(id)) => {
                if let Some(e) = m.remove("error") {
                    let code = e.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                    let message = e.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
                    Some(Incoming::Response { id, result: Err((code, message)) })
                } else {
                    m.remove("result").map(|r| Incoming::Response { id, result: Ok(r) })
                }
            }
            _ => None,
        }
    }
    match v {
        Value::Array(items) => items.into_iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_splits_requests_notifications_responses_and_batches() {
        let batch = json!([
            {"jsonrpc": "2.0", "id": 7, "result": {"ok": true}},
            {"jsonrpc": "2.0", "id": "s1", "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"},
            {"jsonrpc": "2.0", "id": 8, "error": {"code": -32000, "message": "nope"}},
            {"jsonrpc": "2.0", "id": 9},
            "garbage"
        ]);
        assert_eq!(
            classify(batch),
            vec![
                Incoming::Response { id: json!(7), result: Ok(json!({"ok": true})) },
                Incoming::Request { id: json!("s1"), method: "ping".into(), params: Value::Null },
                Incoming::Notification { method: "notifications/tools/list_changed".into(), params: Value::Null },
                Incoming::Response { id: json!(8), result: Err((-32000, "nope".into())) },
            ]
        );
    }

    #[test]
    fn initialize_reads_capabilities_and_refuses_unknown_revisions() {
        let init = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "fx", "version": "1"},
            "instructions": "be nice"
        });
        let info = ServerInfo::from_initialize("fx", &init).unwrap();
        assert!(info.tools && info.tools_list_changed && !info.resources);
        assert_eq!(info.instructions.as_deref(), Some("be nice"));
        let err = ServerInfo::from_initialize("fx", &json!({"protocolVersion": "1999-01-01"})).unwrap_err();
        assert!(err.to_string().contains("`1999-01-01`"), "{err}");
    }
}
