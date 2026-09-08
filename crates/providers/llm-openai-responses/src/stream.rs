use std::collections::BTreeMap;

use aho_llm::{LlmError, Response, Sink, StreamEvent};
use aho_types::{CallId, ContentBlock, StopReason, Usage, Value};
use serde_json::json;

use crate::{PROTOCOL, classify};

#[derive(Debug)]
enum Item {
    Text { text: String, refusal: bool },
    Thinking(String),
    Tool { id: String, name: String, arguments: String, started: bool, ended: bool },
}

#[derive(Debug)]
struct Slot {
    item: Item,
    native: Option<Value>,
}

/// Output indices, not arrival order, identify items: reasoning, messages and parallel tool
/// calls can interleave. Only a terminal response event commits the final response.
#[derive(Debug)]
pub struct StreamDecoder {
    route: String,
    model: String,
    response_id: String,
    slots: BTreeMap<u64, Slot>,
    usage: Usage,
    stop: Option<StopReason>,
}

impl StreamDecoder {
    pub fn new(route: &str) -> Self {
        Self::with_model(route, "")
    }

    pub fn with_model(route: &str, model: &str) -> Self {
        Self {
            route: route.to_string(),
            model: model.to_string(),
            response_id: String::new(),
            slots: BTreeMap::new(),
            usage: Usage::default(),
            stop: None,
        }
    }

    pub fn is_done(&self) -> bool {
        self.stop.is_some()
    }

    pub fn on_data(&mut self, data: &str, sink: &mut Sink<'_>) -> Result<(), LlmError> {
        if self.is_done() || data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data).map_err(|_| self.protocol_error("received invalid JSON"))?;
        let kind = event["type"].as_str().ok_or_else(|| self.protocol_error("received an event without a type"))?;
        match kind {
            "error" | "response.failed" => return Err(classify(None, None, data, false, &self.route)),
            "response.created" | "response.in_progress" => {
                if let Some(id) = event["response"]["id"].as_str() {
                    self.response_id = id.to_string();
                }
            }
            "response.output_item.added" => {
                let index = self.index(&event)?;
                self.item(index, &event["item"], false, sink)?;
            }
            "response.output_item.done" => {
                let index = self.index(&event)?;
                self.item(index, &event["item"], true, sink)?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let index = self.index(&event)?;
                let delta = self.field(&event, "delta")?;
                let slot = self
                    .slots
                    .entry(index)
                    .or_insert_with(|| Slot { item: Item::Text { text: String::new(), refusal: false }, native: None });
                if let Item::Text { text, refusal } = &mut slot.item {
                    *refusal |= kind == "response.refusal.delta";
                    text.push_str(delta);
                    if !delta.is_empty() {
                        sink(StreamEvent::Text(delta.to_string()));
                    }
                } else {
                    return Err(self.protocol_error("text delta changed an output item's type"));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let index = self.index(&event)?;
                let delta = self.field(&event, "delta")?;
                let slot = self.slots.entry(index).or_insert_with(|| Slot { item: Item::Thinking(String::new()), native: None });
                if let Item::Thinking(text) = &mut slot.item {
                    text.push_str(delta);
                    if !delta.is_empty() {
                        sink(StreamEvent::Thinking(delta.to_string()));
                    }
                } else {
                    return Err(self.protocol_error("reasoning delta changed an output item's type"));
                }
            }
            "response.reasoning_summary_part.done" => {
                let index = self.index(&event)?;
                if let Some(Slot { item: Item::Thinking(text), .. }) = self.slots.get_mut(&index) {
                    text.push_str("\n\n");
                    sink(StreamEvent::Thinking("\n\n".to_string()));
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let index = self.index(&event)?;
                let value = self.field(&event, if kind.ends_with(".delta") { "delta" } else { "arguments" })?;
                let slot = self.slots.entry(index).or_insert_with(|| Slot { item: empty_tool(), native: None });
                if let Item::Tool { arguments, ended, .. } = &mut slot.item {
                    if *ended {
                        return Err(self.protocol_error("received arguments after a tool call finished"));
                    }
                    if kind.ends_with(".delta") {
                        arguments.push_str(value);
                    } else {
                        *arguments = value.to_string();
                    }
                } else {
                    return Err(self.protocol_error("tool delta changed an output item's type"));
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => self.complete(&event, sink)?,
            "response.cancelled" => return Err(self.protocol_error("the server cancelled the response")),
            // Content/summary part-added and text-done events carry no new content: the item
            // and terminal snapshots below reconcile providers that omit individual deltas.
            _ => {}
        }
        Ok(())
    }

    fn item(&mut self, index: u64, value: &Value, done: bool, sink: &mut Sink<'_>) -> Result<(), LlmError> {
        let kind = value["type"].as_str().ok_or_else(|| self.protocol_error("received an output item without a type"))?;
        let initial = match kind {
            "message" => Item::Text { text: String::new(), refusal: false },
            "reasoning" => Item::Thinking(String::new()),
            "function_call" => empty_tool(),
            _ => return Ok(()),
        };
        let slot = self.slots.entry(index).or_insert_with(|| Slot { item: initial, native: None });
        match (&mut slot.item, kind) {
            (Item::Text { text, refusal }, "message") if done => {
                if let Some(parts) = value["content"].as_array() {
                    let mut complete = String::new();
                    for part in parts {
                        if part["type"] == "refusal" {
                            *refusal = true;
                            if let Some(part) = part["refusal"].as_str() {
                                complete.push_str(part);
                            }
                        } else if let Some(part) = part["text"].as_str() {
                            complete.push_str(part);
                        }
                    }
                    reconcile_text(text, complete, false, sink);
                }
            }
            (Item::Thinking(text), "reasoning") if done => {
                let summary = joined_parts(&value["summary"]);
                let complete = if summary.is_empty() { joined_parts(&value["content"]) } else { summary };
                if !complete.is_empty() {
                    reconcile_text(text, complete, true, sink);
                }
            }
            (Item::Tool { id, name, arguments, started, ended }, "function_call") => {
                if let Some(next) = value["call_id"].as_str().filter(|value| !value.is_empty()) {
                    if !id.is_empty() && id != next {
                        return Err(self.protocol_error("a tool call changed its call_id"));
                    }
                    if id.is_empty() {
                        *id = next.to_string();
                    }
                }
                if let Some(next) = value["name"].as_str().filter(|value| !value.is_empty()) {
                    if !name.is_empty() && name != next {
                        return Err(self.protocol_error("a tool call changed its name"));
                    }
                    if name.is_empty() {
                        *name = next.to_string();
                    }
                }
                if let Some(next) = value["arguments"].as_str() {
                    // A done snapshot is authoritative, not another delta. Reject a changed
                    // completed call rather than silently executing different final arguments.
                    if *ended && next != arguments {
                        return Err(self.protocol_error("a completed tool call changed its arguments"));
                    }
                    if done || arguments.is_empty() {
                        *arguments = next.to_string();
                    }
                }
                if !*started && !id.is_empty() && !name.is_empty() {
                    *started = true;
                    sink(StreamEvent::ToolUseStart { id: CallId(id.clone()), name: name.clone() });
                }
                if done && !*ended {
                    if !*started {
                        return Err(self.protocol_error("a completed tool call is missing its call_id or name"));
                    }
                    *ended = true;
                    sink(StreamEvent::ToolUseEnd { id: CallId(id.clone()), name: name.clone(), args: parse_args(arguments) });
                }
            }
            (Item::Text { .. }, "message") | (Item::Thinking(_), "reasoning") => {}
            _ => return Err(self.protocol_error("an output index changed its item type")),
        }
        if done {
            slot.native = Some(value.clone());
        }
        Ok(())
    }

    fn complete(&mut self, event: &Value, sink: &mut Sink<'_>) -> Result<(), LlmError> {
        let response = &event["response"];
        if !response.is_object() {
            return Err(self.protocol_error("received a terminal event without a response"));
        }
        if response["error"].is_object() {
            return Err(classify(None, None, &event.to_string(), false, &self.route));
        }
        let status = response["status"].as_str().unwrap_or(if event["type"] == "response.incomplete" { "incomplete" } else { "completed" });
        let stop = match status {
            "completed" => StopReason::EndTurn,
            "incomplete" => match response["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::Refusal,
                _ => return Err(self.protocol_error("the response is incomplete without a supported stop reason")),
            },
            "failed" | "cancelled" => return Err(self.protocol_error("the server did not complete the response")),
            _ => return Err(self.protocol_error("a terminal event contained a non-terminal response status")),
        };
        if let Some(id) = response["id"].as_str() {
            self.response_id = id.to_string();
        }
        if let Some(output) = response["output"].as_array() {
            // Terminal output may be the only source of encrypted reasoning (e.g. gateways
            // that omit it from output_item.done), and may contain entire unstreamed items.
            for (index, item) in output.iter().enumerate() {
                self.item(index as u64, item, true, sink)?;
            }
        }
        for slot in self.slots.values_mut() {
            if let Item::Tool { id, name, arguments, started, ended } = &mut slot.item {
                if !*started {
                    return Err(self.protocol_error("the response finished with an unidentified tool call"));
                }
                if !*ended {
                    *ended = true;
                    sink(StreamEvent::ToolUseEnd { id: CallId(id.clone()), name: name.clone(), args: parse_args(arguments) });
                }
            }
        }
        if response["usage"].is_object() {
            let usage = &response["usage"];
            let read = usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            let write = usage["input_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0);
            self.usage = Usage {
                input_tokens: usage["input_tokens"].as_u64().unwrap_or(0).saturating_sub(read).saturating_sub(write),
                output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: read,
                cache_write_tokens: write,
            };
            sink(StreamEvent::Usage(self.usage));
        }
        self.stop = Some(stop);
        Ok(())
    }

    pub fn finish(self, _sink: &mut Sink<'_>) -> Result<Response, LlmError> {
        let Some(mut stop) = self.stop else {
            return Err(self.protocol_error("closed the stream before a terminal response event"));
        };
        let mut content = Vec::with_capacity(self.slots.len() + 1);
        let mut native = Vec::with_capacity(self.slots.len());
        let mut has_tools = false;
        let mut refused = false;
        let mut complete_native = true;
        for slot in self.slots.into_values() {
            if let Some(item) = slot.native {
                native.push(item);
            } else {
                complete_native = false;
            }
            match slot.item {
                Item::Text { text, refusal } => {
                    refused |= refusal;
                    if !text.is_empty() {
                        content.push(ContentBlock::Text { text });
                    }
                }
                Item::Thinking(text) => {
                    if !text.is_empty() {
                        content.push(ContentBlock::Thinking { text });
                    }
                }
                Item::Tool { id, name, arguments, .. } => {
                    has_tools = true;
                    content.push(ContentBlock::ToolUse { id: CallId(id), name, args: parse_args(&arguments) });
                }
            }
        }
        if stop == StopReason::EndTurn {
            if refused {
                stop = StopReason::Refusal;
            } else if has_tools {
                stop = StopReason::ToolUse;
            }
        }
        // Only replace history with native output if it accounts for every visible block.
        if complete_native && !native.is_empty() {
            content.push(ContentBlock::ProviderMetadata {
                protocol: PROTOCOL.tag().to_string(),
                data: json!({"route": self.route, "model": self.model, "response_id": self.response_id, "output": native}),
            });
        }
        Ok(Response { content, usage: self.usage, stop })
    }

    fn index(&self, event: &Value) -> Result<u64, LlmError> {
        event["output_index"].as_u64().ok_or_else(|| self.protocol_error("an output event is missing output_index"))
    }

    fn field<'a>(&self, event: &'a Value, key: &str) -> Result<&'a str, LlmError> {
        event[key].as_str().ok_or_else(|| self.protocol_error("an output event is missing its string payload"))
    }

    fn protocol_error(&self, problem: &str) -> LlmError {
        LlmError::Transient(format!("route `{}` {problem}; the partial Responses response was discarded", self.route))
    }
}

fn empty_tool() -> Item {
    Item::Tool { id: String::new(), name: String::new(), arguments: String::new(), started: false, ended: false }
}

fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() { json!({}) } else { serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())) }
}

fn joined_parts(value: &Value) -> String {
    value.as_array().map(|parts| parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n\n")).unwrap_or_default()
}

fn reconcile_text(text: &mut String, complete: String, thinking: bool, sink: &mut Sink<'_>) {
    if let Some(delta) = complete.strip_prefix(text.as_str()).filter(|delta| !delta.is_empty()) {
        sink(if thinking { StreamEvent::Thinking(delta.to_string()) } else { StreamEvent::Text(delta.to_string()) });
    }
    *text = complete;
}
