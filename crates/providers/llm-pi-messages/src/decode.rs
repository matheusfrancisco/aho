//! Serialized pi assistant events, including native replay signatures and terminal metadata.

use std::collections::BTreeMap;

use aho_llm::{LlmError, Response, Sink, StreamEvent};
use aho_types::{CallId, ContentBlock, StopReason, Timestamp, Usage, Value};
use serde_json::json;

use crate::error::{classify, malformed};
use crate::{PROTOCOL, ROUTE};

#[derive(Debug)]
struct Block {
    content: ContentBlock,
    metadata: Value,
    complete: bool,
}

/// Fold SSE `data` payloads into the seam. A terminal `done` is required; `[DONE]` is not one.
#[derive(Debug)]
pub struct StreamDecoder {
    blocks: BTreeMap<u64, Block>,
    message: Value,
    usage: Usage,
    stop: Option<StopReason>,
    has_images: bool,
}

impl StreamDecoder {
    pub fn new(model: &str) -> Self {
        Self {
            blocks: BTreeMap::new(),
            message: json!({"api": PROTOCOL.tag(), "provider": ROUTE, "model": model, "timestamp": Timestamp::now().0}),
            usage: Usage::default(),
            stop: None,
            has_images: false,
        }
    }

    pub(crate) fn with_images(mut self, has_images: bool) -> Self {
        self.has_images = has_images;
        self
    }

    pub fn is_done(&self) -> bool {
        self.stop.is_some()
    }

    pub fn on_event(&mut self, data: &str, sink: &mut Sink<'_>) -> Result<(), LlmError> {
        if self.is_done() || data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data).map_err(|_| malformed())?;
        let kind = text(&event, "type")?;
        match kind {
            "start" => {}
            "text_start" => self.start(&event, ContentBlock::text(""))?,
            "thinking_start" => self.start(&event, ContentBlock::Thinking { text: String::new() })?,
            "toolcall_start" => {
                let id = CallId(text(&event, "id")?.to_string());
                let name = text(&event, "toolName")?.to_string();
                self.start(&event, ContentBlock::ToolUse { id: id.clone(), name: name.clone(), args: Value::Null })?;
                sink(StreamEvent::ToolUseStart { id, name });
            }
            "text_delta" | "thinking_delta" => {
                let delta = text(&event, "delta")?;
                let block = self.open(&event)?;
                match (&mut block.content, kind) {
                    (ContentBlock::Text { text }, "text_delta") => {
                        text.push_str(delta);
                        sink(StreamEvent::Text(delta.to_string()));
                    }
                    (ContentBlock::Thinking { text }, "thinking_delta") => {
                        text.push_str(delta);
                        sink(StreamEvent::Thinking(delta.to_string()));
                    }
                    _ => return Err(malformed()),
                }
            }
            "toolcall_delta" => {
                // The seam emits complete arguments only. `toolcall_end` carries the gateway's
                // authoritative parsed object, not a concatenation of partial JSON guesses.
                text(&event, "delta")?;
                if !matches!(self.open(&event)?.content, ContentBlock::ToolUse { .. }) {
                    return Err(malformed());
                }
            }
            "text_end" | "thinking_end" => {
                let content = text(&event, "content")?;
                let block = self.open(&event)?;
                let signature = match (&mut block.content, kind) {
                    (ContentBlock::Text { text }, "text_end") => {
                        *text = content.to_string();
                        "textSignature"
                    }
                    (ContentBlock::Thinking { text }, "thinking_end") => {
                        *text = content.to_string();
                        "thinkingSignature"
                    }
                    _ => return Err(malformed()),
                };
                if let Some(value) = event.get("contentSignature") {
                    block.metadata[signature] = value.clone();
                }
                if kind == "thinking_end"
                    && let Some(value) = event.get("redacted")
                {
                    block.metadata["redacted"] = value.clone();
                }
                block.complete = true;
            }
            "toolcall_end" => {
                let call = &event["toolCall"];
                let id = text(call, "id")?;
                let name = text(call, "name")?;
                let args = call.get("arguments").ok_or_else(malformed)?;
                let block = self.open(&event)?;
                match &mut block.content {
                    ContentBlock::ToolUse { id: original_id, name: original_name, args: original_args }
                        if original_id.0 == id && original_name.as_str() == name =>
                    {
                        *original_args = args.clone();
                        sink(StreamEvent::ToolUseEnd { id: original_id.clone(), name: original_name.clone(), args: args.clone() });
                    }
                    _ => return Err(malformed()),
                }
                for key in ["thoughtSignature", "namespace"] {
                    if let Some(value) = call.get(key) {
                        block.metadata[key] = value.clone();
                    }
                }
                block.complete = true;
            }
            "done" | "error" => {
                let usage = &event["usage"];
                self.usage = Usage {
                    input_tokens: count(usage, "input")?,
                    output_tokens: count(usage, "output")?,
                    cache_read_tokens: count(usage, "cacheRead")?,
                    cache_write_tokens: count(usage, "cacheWrite")?,
                };
                sink(StreamEvent::Usage(self.usage));
                if kind == "error" {
                    return Err(classify(None, None, data, self.has_images));
                }
                let stop = match text(&event, "reason")? {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "toolUse" => StopReason::ToolUse,
                    _ => return Err(malformed()),
                };
                self.message["stopReason"] = event["reason"].clone();
                self.message["usage"] = usage.clone();
                for key in ["responseId", "providerThinkingLevel"] {
                    if let Some(value) = event.get(key) {
                        self.message[key] = value.clone();
                    }
                }
                if let Some(rewrite) = event.get("rewrite") {
                    self.message["diagnostics"] = json!([{
                        "type": "pi_messages_rewrite", "timestamp": Timestamp::now().0, "details": rewrite
                    }]);
                }
                self.stop = Some(stop);
            }
            _ => {} // Forward-compatible additions do not replace the mandatory terminal event.
        }
        Ok(())
    }

    fn start(&mut self, event: &Value, content: ContentBlock) -> Result<(), LlmError> {
        let index = count(event, "contentIndex")?;
        if self.blocks.contains_key(&index) {
            return Err(malformed());
        }
        self.blocks.insert(index, Block { content, metadata: json!({}), complete: false });
        Ok(())
    }

    fn open(&mut self, event: &Value) -> Result<&mut Block, LlmError> {
        let index = count(event, "contentIndex")?;
        self.blocks.get_mut(&index).filter(|block| !block.complete).ok_or_else(malformed)
    }

    pub fn finish(self) -> Result<Response, LlmError> {
        let stop = self.stop.ok_or_else(|| {
            LlmError::Transient("Radius gateway closed the stream without a terminal event; the partial response was discarded".into())
        })?;
        if self.blocks.values().any(|block| !block.complete) {
            return Err(malformed());
        }
        let mut content = Vec::with_capacity(self.blocks.len() + 1);
        let mut metadata = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.into_values() {
            content.push(block.content);
            metadata.push(block.metadata);
        }
        content.push(ContentBlock::ProviderMetadata {
            protocol: PROTOCOL.tag().into(),
            data: json!({"message": self.message, "blocks": metadata}),
        });
        Ok(Response { content, usage: self.usage, stop })
    }
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, LlmError> {
    value[field].as_str().ok_or_else(malformed)
}

fn count(value: &Value, field: &str) -> Result<u64, LlmError> {
    value[field].as_u64().ok_or_else(malformed)
}
