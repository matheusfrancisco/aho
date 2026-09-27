//! ACP, the Agent Client Protocol (agentclientprotocol.com), protocol version 1.
//!
//! Types follow the published JSON Schema `schema/v1/schema.json`, schema release **1.23.0**
//! (github.com/agentclientprotocol/agent-client-protocol, 2026-09-18); field names are the
//! schema's. Transport is stdio framing: newline-delimited JSON-RPC 2.0 messages that never
//! contain an embedded newline.
//!
//! Both aho sides share this crate: `surface-acp` is aho as an ACP server (answers `initialize` /
//! `session/*`, sends `session/update` + `session/request_permission`); `subagent-acp` is the
//! client side and drives an external agent the other way round.
//!
//! * [`schema`]: `initialize`, `session/new`, `session/load`, `session/prompt`,
//!   `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update`, `plan`, …),
//!   `session/request_permission`, `session/cancel`.
//! * [`jsonrpc`]: the message model, error codes, decode rules, and the line framing.
//! * [`peer`]: a bidirectional connection over a caller-supplied reader/writer. This crate opens
//!   no process, socket, or file.

#![forbid(unsafe_code)]

pub mod jsonrpc;
pub mod peer;
pub mod schema;

pub use jsonrpc::{Frame, FrameReader, JSONRPC, MAX_FRAME, Message, Rejected, RequestId, RpcError, code, decode, params, result};
pub use peer::{Driver, Handler, HandlerFuture, Peer, connection};

/// The protocol version this crate implements (`ProtocolVersion`, a MAJOR number).
pub const PROTOCOL_VERSION: u16 = 1;
/// The schema release the types were taken from.
pub const SCHEMA_RELEASE: &str = "1.23.0";

/// Method names (`schema/v1/meta.json`).
pub mod methods {
    // agent side (the client calls these)
    pub const INITIALIZE: &str = "initialize";
    pub const AUTHENTICATE: &str = "authenticate";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_SET_MODE: &str = "session/set_mode";
    pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_DELETE: &str = "session/delete";
    pub const SESSION_RESUME: &str = "session/resume";
    pub const SESSION_CLOSE: &str = "session/close";
    pub const LOGOUT: &str = "logout";
    // client side (the agent calls these)
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
    pub const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
    pub const TERMINAL_CREATE: &str = "terminal/create";
    pub const TERMINAL_OUTPUT: &str = "terminal/output";
    pub const TERMINAL_RELEASE: &str = "terminal/release";
    pub const TERMINAL_WAIT_FOR_EXIT: &str = "terminal/wait_for_exit";
    pub const TERMINAL_KILL: &str = "terminal/kill";
    pub const ELICITATION_CREATE: &str = "elicitation/create";
    pub const ELICITATION_COMPLETE: &str = "elicitation/complete";
    // protocol level
    pub const CANCEL_REQUEST: &str = "$/cancel_request";
}

/// Version negotiation (initialization §Version Negotiation): the agent answers the requested
/// version when it supports it, else the latest it supports.
pub fn negotiate(requested: u16) -> u16 {
    if requested == PROTOCOL_VERSION { requested } else { PROTOCOL_VERSION }
}
