//! Anthropic response contract for OpenAI Chat Completions.
//!
//! One semantic machine serves both output modes (unary JSON and streamed SSE
//! frames) so the two cannot drift: the machine -- never the transport -- owns
//! the terminal decision, so a stream cut by EOF, a duplicate "[DONE]", or
//! residual bytes after the framing terminal can never synthesize a success
//! the provider did not declare. Text-only for this slice; tools, reasoning,
//! and multi-choice expansion belong to later plans (13-02/13-03).

use serde_json::{json, Value};

pub use crate::model::gemini::SseEvent;

const MAX_RETAINED_SEMANTIC_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalState {
    Open,
    SuccessPending,
    SuccessEmitted,
    ProviderFailed,
    ProtocolFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiChatSemanticError {
    message: String,
}

impl OpenAiChatSemanticError {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for OpenAiChatSemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenAiChatSemanticError {}

#[derive(Debug)]
struct CheckedChunk {
    parts: Vec<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    finish_reason: Option<&'static str>,
    provider_error: Option<Value>,
    retained_bytes: usize,
}

/// State machine over OpenAI Chat chunks (SSE frames or a full completion
/// object) that emits Anthropic SSE events and the final Anthropic response.
pub struct OpenAiChatSseMachine {
    model: String,
    message_id: String,
    started: bool,
    terminal: TerminalState,
    block_index: usize,
    active_text_block: Option<usize>,
    input_tokens: u64,
    output_tokens: u64,
    last_finish_reason: Option<&'static str>,
    accumulate_content: bool,
    content: Vec<Value>,
    retained_bytes: usize,
}

impl OpenAiChatSseMachine {
    pub fn new_for_upstream(model: impl Into<String>) -> Self {
        let random_id = format!("{:016x}", rand::random::<u64>());
        Self {
            model: model.into(),
            message_id: format!("msg_openai_chat_{random_id}"),
            started: false,
            terminal: TerminalState::Open,
            block_index: 0,
            active_text_block: None,
            input_tokens: 0,
            output_tokens: 0,
            last_finish_reason: None,
            accumulate_content: true,
            content: Vec::new(),
            retained_bytes: 0,
        }
    }

    /// Construct a machine for incremental relay: streamed text is forwarded
    /// as deltas and never retained as a second complete response copy.
    pub fn new_streaming_for_upstream(model: impl Into<String>) -> Self {
        let mut machine = Self::new_for_upstream(model);
        machine.accumulate_content = false;
        machine
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Validate one Chat Completions chunk (or whole completion) and atomically
    /// apply its semantic meaning.
    pub fn process_chunk_checked(
        &mut self,
        chunk: &Value,
    ) -> Result<Vec<SseEvent>, OpenAiChatSemanticError> {
        match self.terminal {
            TerminalState::Open => {}
            // With stream_options.include_usage the official stream appends a
            // usage-only chunk (empty choices) after the finish_reason chunk
            // and before [DONE]. Accept exactly that shape; content, a second
            // finish, or any other payload after finish_reason fails closed.
            TerminalState::SuccessPending => return self.process_trailing_usage_chunk(chunk),
            _ => {
            self.terminal = TerminalState::ProtocolFailed;
            return Err(OpenAiChatSemanticError::protocol(
                "OpenAI Chat semantic data arrived after a terminal outcome",
            ));
            }
        }

        let checked = match self.validate_chunk(chunk) {
            Ok(checked) => checked,
            Err(error) => {
                self.terminal = TerminalState::ProtocolFailed;
                return Err(error);
            }
        };

        if let Some(error_val) = checked.provider_error {
            self.terminal = TerminalState::ProviderFailed;
            return Ok(vec![SseEvent {
                event: "error".to_string(),
                data: error_val,
            }]);
        }

        self.retained_bytes = self
            .retained_bytes
            .checked_add(checked.retained_bytes)
            .filter(|total| *total <= MAX_RETAINED_SEMANTIC_BYTES)
            .ok_or_else(|| {
                self.terminal = TerminalState::ProtocolFailed;
                OpenAiChatSemanticError::protocol(
                    "OpenAI Chat retained semantic state exceeds limit",
                )
            })?;

        if let Some(tokens) = checked.input_tokens {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = checked.output_tokens {
            self.output_tokens = tokens;
        }

        let mut events = Vec::new();
        if !self.started && (!checked.parts.is_empty() || checked.finish_reason.is_some()) {
            self.started = true;
            events.push(self.message_start_event());
        }
        for text in checked.parts {
            self.apply_part(text, &mut events);
        }
        if let Some(reason) = checked.finish_reason {
            self.last_finish_reason = Some(reason);
            self.terminal = TerminalState::SuccessPending;
        }
        Ok(events)
    }

    fn process_trailing_usage_chunk(
        &mut self,
        chunk: &Value,
    ) -> Result<Vec<SseEvent>, OpenAiChatSemanticError> {
        let chunk = chunk
            .as_object()
            .ok_or_else(|| {
                OpenAiChatSemanticError::protocol("OpenAI Chat chunk must be an object")
            })?;
        if chunk.contains_key("error") {
            self.terminal = TerminalState::ProviderFailed;
            let message = chunk
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Chat backend error");
            return Ok(vec![SseEvent {
                event: "error".to_string(),
                data: json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": message}
                }),
            }]);
        }
        if let Some(choices) = chunk.get("choices") {
            let choices = choices.as_array().ok_or_else(|| {
                OpenAiChatSemanticError::protocol("OpenAI Chat choices must be an array")
            })?;
            let carries_payload = choices.iter().any(|choice| {
                choice
                    .get("delta")
                    .and_then(|delta| delta.get("content"))
                    .is_some_and(|content| !content.is_null())
                    || choice.get("finish_reason").is_some()
            });
            if carries_payload {
                self.terminal = TerminalState::ProtocolFailed;
                return Err(OpenAiChatSemanticError::protocol(
                    "content or a second finish_reason arrived after the finish_reason chunk",
                ));
            }
        }
        let (input_tokens, output_tokens) = validate_usage(chunk.get("usage"))?;
        if let Some(tokens) = input_tokens {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = output_tokens {
            self.output_tokens = tokens;
        }
        Ok(Vec::new())
    }

    fn validate_chunk(&self, chunk: &Value) -> Result<CheckedChunk, OpenAiChatSemanticError> {
        let chunk = chunk
            .as_object()
            .ok_or_else(|| {
                OpenAiChatSemanticError::protocol("OpenAI Chat chunk must be an object")
            })?;

        if let Some(error) = chunk.get("error") {
            let error = error.as_object().ok_or_else(|| {
                OpenAiChatSemanticError::protocol("OpenAI Chat provider error must be an object")
            })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Chat backend error");
            return Ok(CheckedChunk {
                parts: Vec::new(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                provider_error: Some(json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": message}
                })),
                retained_bytes: 0,
            });
        }

        let (input_tokens, output_tokens) = validate_usage(chunk.get("usage"))?;

        let mut parts = Vec::new();
        let mut finish_reason = None;
        let mut retained_bytes = 0usize;
        if let Some(choices) = chunk.get("choices") {
            let choices = choices.as_array().ok_or_else(|| {
                OpenAiChatSemanticError::protocol("OpenAI Chat choices must be an array")
            })?;
            if choices.len() > 1 {
                return Err(OpenAiChatSemanticError::protocol(
                    "multiple OpenAI Chat choices have ambiguous ordering",
                ));
            }
            if let Some(choice) = choices.first() {
                let choice = choice.as_object().ok_or_else(|| {
                    OpenAiChatSemanticError::protocol("OpenAI Chat choice must be an object")
                })?;
                if let Some(reason) = choice.get("finish_reason") {
                    let reason = reason.as_str().ok_or_else(|| {
                        OpenAiChatSemanticError::protocol(
                            "OpenAI Chat finish_reason must be a string",
                        )
                    })?;
                    let mapped = match reason {
                        "stop" => "end_turn",
                        "length" => "max_tokens",
                        other => {
                            return Err(OpenAiChatSemanticError::protocol(format!(
                                "unsupported OpenAI Chat finish_reason {other}"
                            )))
                        }
                    };
                    finish_reason = Some(mapped);
                }
                // Both output shapes share one field path check: a whole
                // completion carries message content, a stream frame carries
                // delta content.
                let content = choice
                    .get("message")
                    .or_else(|| choice.get("delta"))
                    .and_then(|message| message.get("content"));
                if let Some(content) = content {
                    let text = match content {
                        Value::Null => "",
                        Value::String(text) => text,
                        _ => {
                            return Err(OpenAiChatSemanticError::protocol(
                                "OpenAI Chat content must be a string or null",
                            ))
                        }
                    };
                    if !text.is_empty() {
                        retained_bytes = retained_bytes.checked_add(text.len()).ok_or_else(|| {
                            OpenAiChatSemanticError::protocol(
                                "OpenAI Chat retained semantic state exceeds limit",
                            )
                        })?;
                        parts.push(text.to_string());
                    }
                }
                // Unknown choice fields (tool_calls, reasoning, ...) are
                // outside this slice's whitelist; response expansion is
                // deferred to later plans rather than guessed.
            }
        }
        Ok(CheckedChunk {
            parts,
            input_tokens,
            output_tokens,
            finish_reason,
            provider_error: None,
            retained_bytes,
        })
    }

    fn message_start_event(&self) -> SseEvent {
        SseEvent {
            event: "message_start".to_string(),
            data: json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}
                }
            }),
        }
    }

    fn apply_part(&mut self, text: String, events: &mut Vec<SseEvent>) {
        if self.active_text_block.is_none() {
            self.close_active_block(events);
            let idx = self.block_index;
            events.push(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {"type": "text", "text": ""}
                }),
            });
            self.active_text_block = Some(idx);
        }
        let idx = self.active_text_block.unwrap();
        events.push(SseEvent {
            event: "content_block_delta".to_string(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": text}
            }),
        });
        if self.accumulate_content {
            let should_push = match self.content.last_mut() {
                Some(last) if last.get("type").and_then(Value::as_str) == Some("text") => {
                    if let Some(Value::String(s)) = last.get_mut("text") {
                        s.push_str(&text);
                    }
                    false
                }
                _ => true,
            };
            if should_push {
                self.content.push(json!({"type": "text", "text": text}));
            }
        }
    }

    fn close_active_block(&mut self, events: &mut Vec<SseEvent>) {
        if let Some(idx) = self.active_text_block.take() {
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({
                    "type": "content_block_stop",
                    "index": idx
                }),
            });
            self.block_index += 1;
        }
    }

    /// The transport -- EOF or the "[DONE]" framing marker -- may only *close*
    /// a provider-declared success. Without a preceding accepted
    /// finish_reason this fails closed, so an upstream cut mid-stream can
    /// never become a synthesized success.
    pub fn transport_close_checked(&mut self) -> Result<Vec<SseEvent>, OpenAiChatSemanticError> {
        if self.terminal != TerminalState::SuccessPending {
            self.terminal = TerminalState::ProtocolFailed;
            return Err(OpenAiChatSemanticError::protocol(
                "OpenAI Chat transport closed without a finish_reason or [DONE]",
            ));
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(self.message_start_event());
        }
        self.close_active_block(&mut events);
        events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop_reason(),
                    "stop_sequence": null
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens
                }
            }),
        });
        events.push(SseEvent {
            event: "message_stop".to_string(),
            data: json!({"type": "message_stop"}),
        });
        self.terminal = TerminalState::SuccessEmitted;
        Ok(events)
    }

    /// Return the full final Anthropic response JSON for the unary path.
    pub fn final_json_checked(&self) -> Result<Value, OpenAiChatSemanticError> {
        if self.terminal != TerminalState::SuccessEmitted || !self.accumulate_content {
            return Err(OpenAiChatSemanticError::protocol(
                "OpenAI Chat final JSON requested before checked unary completion",
            ));
        }
        Ok(json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "content": self.content,
            "model": self.model,
            "stop_reason": self.stop_reason(),
            "stop_sequence": null,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens
            }
        }))
    }

    fn stop_reason(&self) -> &'static str {
        match self.last_finish_reason {
            Some("max_tokens") => "max_tokens",
            _ => "end_turn",
        }
    }
}

fn validate_usage(
    usage: Option<&Value>,
) -> Result<(Option<u64>, Option<u64>), OpenAiChatSemanticError> {
    let Some(usage) = usage else {
        return Ok((None, None));
    };
    let usage = usage.as_object().ok_or_else(|| {
        OpenAiChatSemanticError::protocol("OpenAI Chat usage must be an object")
    })?;
    let parse = |key: &str| -> Result<Option<u64>, OpenAiChatSemanticError> {
        match usage.get(key) {
            None => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                OpenAiChatSemanticError::protocol(format!(
                    "OpenAI Chat usage.{key} must be a non-negative integer"
                ))
            }),
        }
    };
    let input = parse("prompt_tokens")?;
    let output = parse("completion_tokens")?;
    Ok((input, output))
}
