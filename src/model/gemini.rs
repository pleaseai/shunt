//! Gemini response and SSE stream translation -> Anthropic Messages SSE events.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};

use crate::adapters::AdapterError;

const GEMINI_TOOL_USE_ID_PREFIX: &str = "call_gemini_v1_";
const MAX_RETAINED_SEMANTIC_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOOL_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_TOOL_USE_ID_BYTES: usize = 96 * 1024;
const MAX_CONTENT_BLOCKS: usize = 4_096;
/// Provider Part fields with content semantics. These must never fall through
/// to the metadata no-op path: unsupported kinds are rejected explicitly, and
/// more than one kind in a Part is ambiguous.
const GEMINI_PART_SEMANTIC_KEYS: &[&str] = &[
    "text",
    "functionCall",
    "functionResponse",
    "inlineData",
    "executableCode",
    "codeExecutionResult",
    "fileData",
];
const UNSUPPORTED_GEMINI_PART_KEYS: &[&str] = &[
    "inlineData",
    "executableCode",
    "codeExecutionResult",
    "fileData",
];
/// `citationMetadata` is the currently evidenced metadata-only Part field.
/// Truly unknown fields also remain no-ops for forward compatibility, but any
/// newly recognized semantic field must be added to the classifier above.
const GEMINI_PART_METADATA_KEYS: &[&str] = &["citationMetadata"];

/// Pack Gemini's opaque function-call signature into the Anthropic tool-use join
/// key. Claude Code returns this id unchanged in assistant history and in the
/// matching `tool_result`, including when extended thinking is disabled.

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBlockKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalState {
    Open,
    SuccessPending,
    SuccessEmitted,
    ProviderFailed,
    ProtocolFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiSemanticError {
    message: String,
}

impl GeminiSemanticError {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GeminiSemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GeminiSemanticError {}

#[derive(Debug, Clone)]
enum CheckedPart {
    Text(String),
    Thinking(String),
    Tool {
        name: String,
        args: Value,
        id: String,
    },
    Metadata,
}

#[derive(Debug)]
struct CheckedChunk {
    parts: Vec<CheckedPart>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    finish_reason: Option<String>,
    provider_error: Option<Value>,
    retained_bytes: usize,
    part_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveBlock {
    index: usize,
    kind: ActiveBlockKind,
}

/// State machine that processes Gemini response chunks (SSE or JSON)
/// and emits Anthropic SSE events (`message_start`, `content_block_start`,
/// `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`).
pub struct GeminiSseMachine {
    model: String,
    upstream_model: String,
    message_id: String,
    started: bool,
    terminal: TerminalState,
    block_index: usize,
    active_block: Option<ActiveBlock>,
    saw_tool_use: bool,
    input_tokens: u64,
    output_tokens: u64,
    last_finish_reason: Option<String>,
    accumulate_content: bool,
    content: Vec<Value>,
    retained_bytes: usize,
    part_count: usize,
}

impl GeminiSseMachine {
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        Self::new_for_upstream(model.clone(), model)
    }

    pub fn new_for_upstream(model: impl Into<String>, upstream_model: impl Into<String>) -> Self {
        let random_id = format!("{:016x}", rand::random::<u64>());
        Self {
            model: model.into(),
            upstream_model: upstream_model.into(),
            message_id: format!("msg_gemini_{random_id}"),
            started: false,
            terminal: TerminalState::Open,
            block_index: 0,
            active_block: None,
            saw_tool_use: false,
            input_tokens: 0,
            output_tokens: 0,
            last_finish_reason: None,
            accumulate_content: true,
            content: Vec::new(),
            retained_bytes: 0,
            part_count: 0,
        }
    }

    /// Construct a machine for incremental relay. Streamed text/reasoning is
    /// not retained as a second complete response copy.
    pub fn new_streaming(model: impl Into<String>) -> Self {
        let mut machine = Self::new(model);
        machine.accumulate_content = false;
        machine
    }

    pub fn new_streaming_for_upstream(
        model: impl Into<String>,
        upstream_model: impl Into<String>,
    ) -> Self {
        let mut machine = Self::new_for_upstream(model, upstream_model);
        machine.accumulate_content = false;
        machine
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Compatibility wrapper for trusted fixtures. Production transports use
    /// [`Self::process_chunk_checked`] so protocol failures remain typed.
    pub fn process_chunk(&mut self, chunk: &Value) -> Vec<SseEvent> {
        match self.process_chunk_checked(chunk) {
            Ok(mut events) => {
                if self.terminal == TerminalState::SuccessPending {
                    if let Ok(terminal) = self.transport_close_checked() {
                        events.extend(terminal);
                    }
                }
                events
            }
            Err(error) => vec![protocol_error_event(error.to_string())],
        }
    }

    /// Validate one direct Gemini response or one Code Assist wrapper and
    /// atomically apply its semantic meaning.
    pub fn process_chunk_checked(
        &mut self,
        chunk: &Value,
    ) -> Result<Vec<SseEvent>, GeminiSemanticError> {
        if self.terminal != TerminalState::Open {
            self.terminal = TerminalState::ProtocolFailed;
            return Err(GeminiSemanticError::protocol(
                "Gemini semantic data arrived after a terminal outcome",
            ));
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
                data: translate_gemini_error_val(&error_val),
            }]);
        }

        self.retained_bytes = self
            .retained_bytes
            .checked_add(checked.retained_bytes)
            .filter(|total| *total <= MAX_RETAINED_SEMANTIC_BYTES)
            .ok_or_else(|| {
                self.terminal = TerminalState::ProtocolFailed;
                GeminiSemanticError::protocol("Gemini retained semantic state exceeds limit")
            })?;
        self.part_count = self
            .part_count
            .checked_add(checked.part_count)
            .expect("candidate part count was prevalidated");

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

        for part in checked.parts {
            self.apply_part(part, &mut events);
        }
        if let Some(reason) = checked.finish_reason {
            self.last_finish_reason = Some(reason);
            self.terminal = TerminalState::SuccessPending;
        }
        Ok(events)
    }

    fn validate_chunk(&self, outer: &Value) -> Result<CheckedChunk, GeminiSemanticError> {
        let outer = outer
            .as_object()
            .ok_or_else(|| GeminiSemanticError::protocol("Gemini response must be an object"))?;
        let chunk = if let Some(response) = outer.get("response") {
            if ["candidates", "usageMetadata", "error"]
                .iter()
                .any(|key| outer.contains_key(*key))
            {
                return Err(GeminiSemanticError::protocol(
                    "Gemini response mixes wrapped and direct fields",
                ));
            }
            let response = response.as_object().ok_or_else(|| {
                GeminiSemanticError::protocol("Gemini response wrapper must contain an object")
            })?;
            if response.contains_key("response") {
                return Err(GeminiSemanticError::protocol(
                    "nested Gemini response wrappers are not supported",
                ));
            }
            response
        } else {
            outer
        };

        if let Some(error) = chunk.get("error") {
            let error = error.as_object().ok_or_else(|| {
                GeminiSemanticError::protocol("Gemini provider error must be an object")
            })?;
            for key in ["message", "status"] {
                if error.get(key).is_some_and(|value| !value.is_string()) {
                    return Err(GeminiSemanticError::protocol(format!(
                        "Gemini provider error {key} must be a string"
                    )));
                }
            }
            return Ok(CheckedChunk {
                parts: Vec::new(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                provider_error: Some(Value::Object(error.clone())),
                retained_bytes: 0,
                part_count: 0,
            });
        }

        let (input_tokens, output_tokens) = validate_usage(chunk.get("usageMetadata"))?;
        let mut parts = Vec::new();
        let mut finish_reason = None;
        let mut retained_bytes = 0usize;
        if let Some(candidates) = chunk.get("candidates") {
            let candidates = candidates.as_array().ok_or_else(|| {
                GeminiSemanticError::protocol("Gemini candidates must be an array")
            })?;
            if candidates.len() > 1 {
                return Err(GeminiSemanticError::protocol(
                    "multiple Gemini candidates have ambiguous ordering",
                ));
            }
            if let Some(candidate) = candidates.first() {
                let candidate = candidate.as_object().ok_or_else(|| {
                    GeminiSemanticError::protocol("Gemini candidate must be an object")
                })?;
                if let Some(reason) = candidate.get("finishReason") {
                    let reason = reason
                        .as_str()
                        .filter(|reason| !reason.is_empty())
                        .ok_or_else(|| {
                            GeminiSemanticError::protocol("Gemini finishReason must be non-empty")
                        })?;
                    if !matches!(reason, "STOP" | "MAX_TOKENS" | "SAFETY") {
                        return Err(GeminiSemanticError::protocol(format!(
                            "unsupported Gemini finishReason {reason}"
                        )));
                    }
                    finish_reason = Some(reason.to_string());
                }
                if let Some(content) = candidate.get("content") {
                    let content = content.as_object().ok_or_else(|| {
                        GeminiSemanticError::protocol("Gemini candidate content must be an object")
                    })?;
                    if content
                        .get("role")
                        .is_some_and(|role| role.as_str() != Some("model"))
                    {
                        return Err(GeminiSemanticError::protocol(
                            "Gemini candidate role must be model",
                        ));
                    }
                    if let Some(raw_parts) = content.get("parts") {
                        let raw_parts = raw_parts.as_array().ok_or_else(|| {
                            GeminiSemanticError::protocol("Gemini content parts must be an array")
                        })?;
                        if self
                            .part_count
                            .checked_add(raw_parts.len())
                            .is_none_or(|count| count > MAX_CONTENT_BLOCKS)
                        {
                            return Err(GeminiSemanticError::protocol(
                                "Gemini content block count exceeds limit",
                            ));
                        }
                        let mut function_call_seen = self.saw_tool_use;
                        for raw_part in raw_parts {
                            let is_function_call = raw_part.get("functionCall").is_some();
                            let require_signature = is_function_call && !function_call_seen;
                            let (part, cost) = self.validate_part(raw_part, require_signature)?;
                            if is_function_call {
                                function_call_seen = true;
                            }
                            retained_bytes = retained_bytes.checked_add(cost).ok_or_else(|| {
                                GeminiSemanticError::protocol(
                                    "Gemini retained semantic state exceeds limit",
                                )
                            })?;
                            parts.push(part);
                        }
                    }
                }
            }
        }
        let part_count = parts.len();
        Ok(CheckedChunk {
            parts,
            input_tokens,
            output_tokens,
            finish_reason,
            provider_error: None,
            retained_bytes,
            part_count,
        })
    }

    fn validate_part(
        &self,
        raw: &Value,
        require_signature: bool,
    ) -> Result<(CheckedPart, usize), GeminiSemanticError> {
        let part = raw
            .as_object()
            .ok_or_else(|| GeminiSemanticError::protocol("Gemini part must be an object"))?;
        if part.contains_key("thinking") {
            return Err(GeminiSemanticError::protocol(
                "ambiguous Gemini thinking compatibility shape",
            ));
        }
        if part
            .get("thought")
            .is_some_and(|thought| !thought.is_boolean())
        {
            return Err(GeminiSemanticError::protocol(
                "Gemini thought marker must be boolean",
            ));
        }
        if part.get("text").is_some_and(|text| !text.is_string()) {
            return Err(GeminiSemanticError::protocol(
                "Gemini part text must be a string",
            ));
        }
        let semantic_keys: Vec<_> = GEMINI_PART_SEMANTIC_KEYS
            .iter()
            .copied()
            .filter(|key| part.contains_key(*key))
            .collect();
        if semantic_keys.len() > 1 {
            return Err(GeminiSemanticError::protocol(format!(
                "Gemini part claims incompatible semantic kinds: {}",
                semantic_keys.join(", ")
            )));
        }
        if let Some(key) = UNSUPPORTED_GEMINI_PART_KEYS
            .iter()
            .find(|key| part.contains_key(**key))
        {
            return Err(GeminiSemanticError::protocol(format!(
                "unsupported Gemini Part kind {key}"
            )));
        }
        if part.contains_key("functionResponse") {
            return Err(GeminiSemanticError::protocol(
                "assistant-side Gemini functionResponse is unsupported",
            ));
        }
        let has_text = part.contains_key("text");
        let has_call = part.contains_key("functionCall");
        if part.contains_key("thought") && !has_text {
            return Err(GeminiSemanticError::protocol(
                "Gemini thought marker must accompany text",
            ));
        }
        if has_call {
            let call = part["functionCall"].as_object().ok_or_else(|| {
                GeminiSemanticError::protocol("Gemini functionCall must be an object")
            })?;
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    GeminiSemanticError::protocol("Gemini functionCall name must be non-blank")
                })?;
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return Err(GeminiSemanticError::protocol(
                    "Gemini functionCall args must be an object",
                ));
            }
            let signature = match part.get("thoughtSignature") {
                Some(value) => value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        GeminiSemanticError::protocol(
                            "Gemini thoughtSignature must be a non-empty string",
                        )
                    })?,
                None if self.upstream_model.starts_with("gemini-3") && require_signature => {
                    return Err(GeminiSemanticError::protocol(
                        "Gemini 3 functionCall is missing an authentic thoughtSignature",
                    ));
                }
                None => "",
            };
            if signature.len() > MAX_TOOL_SIGNATURE_BYTES {
                return Err(GeminiSemanticError::protocol(
                    "Gemini thoughtSignature exceeds limit",
                ));
            }
            let id = if signature.is_empty() {
                format!("call_{:012x}", rand::random::<u64>())
            } else {
                encode_tool_use_id(signature)
            };
            if id.len() > MAX_TOOL_USE_ID_BYTES {
                return Err(GeminiSemanticError::protocol(
                    "Gemini encoded tool-use id exceeds limit",
                ));
            }
            let cost = if self.accumulate_content {
                name.len()
                    .checked_add(id.len())
                    .and_then(|cost| {
                        serde_json::to_vec(&args)
                            .ok()
                            .and_then(|args| cost.checked_add(args.len()))
                    })
                    .ok_or_else(|| {
                        GeminiSemanticError::protocol(
                            "Gemini retained semantic state exceeds limit",
                        )
                    })?
            } else {
                id.len().checked_add(name.len()).ok_or_else(|| {
                    GeminiSemanticError::protocol("Gemini tool metadata exceeds limit")
                })?
            };
            return Ok((
                CheckedPart::Tool {
                    name: name.to_string(),
                    args,
                    id,
                },
                cost,
            ));
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if text.is_empty() {
                return Ok((CheckedPart::Metadata, 0));
            }
            let cost = if self.accumulate_content {
                text.len()
            } else {
                0
            };
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                return Ok((CheckedPart::Thinking(text.to_string()), cost));
            }
            return Ok((CheckedPart::Text(text.to_string()), cost));
        }
        if part.contains_key("thoughtSignature") {
            return Err(GeminiSemanticError::protocol(
                "Gemini thoughtSignature is not attached to a functionCall",
            ));
        }
        if GEMINI_PART_METADATA_KEYS
            .iter()
            .any(|key| part.contains_key(*key))
        {
            return Ok((CheckedPart::Metadata, 0));
        }
        // Unknown fields that do not claim a known semantic kind are retained
        // as forward-compatible metadata no-ops.
        Ok((CheckedPart::Metadata, 0))
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

    fn apply_part(&mut self, part: CheckedPart, events: &mut Vec<SseEvent>) {
        match part {
            CheckedPart::Thinking(text) => {
                self.ensure_active_block(ActiveBlockKind::Thinking, events);
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": self.active_block.as_ref().unwrap().index,
                        "delta": {
                            "type": "thinking_delta",
                            "thinking": text
                        }
                    }),
                });
                if self.accumulate_content {
                    let should_push = match self.content.last_mut() {
                        Some(last)
                            if last.get("type").and_then(Value::as_str) == Some("thinking") =>
                        {
                            if let Some(Value::String(s)) = last.get_mut("thinking") {
                                s.push_str(&text);
                            }
                            false
                        }
                        _ => true,
                    };
                    if should_push {
                        self.content.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": "gemini_thinking"
                        }));
                    }
                }
            }
            CheckedPart::Tool { name, args, id } => {
                self.saw_tool_use = true;
                self.close_active_block(events);
                let idx = self.block_index;
                events.push(SseEvent {
                    event: "content_block_start".to_string(),
                    data: json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {}
                        }
                    }),
                });
                let args_json_str = serde_json::to_string(&args).expect("validated JSON value");
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": args_json_str
                        }
                    }),
                });
                events.push(SseEvent {
                    event: "content_block_stop".to_string(),
                    data: json!({
                        "type": "content_block_stop",
                        "index": idx
                    }),
                });
                if self.accumulate_content {
                    self.content.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": args
                    }));
                }
                self.block_index += 1;
            }
            CheckedPart::Text(text) => {
                self.ensure_active_block(ActiveBlockKind::Text, events);
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": self.active_block.as_ref().unwrap().index,
                        "delta": {
                            "type": "text_delta",
                            "text": text
                        }
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
                        self.content.push(json!({
                            "type": "text",
                            "text": text
                        }));
                    }
                }
            }
            CheckedPart::Metadata => {}
        }
    }

    fn ensure_active_block(&mut self, kind: ActiveBlockKind, events: &mut Vec<SseEvent>) {
        let need_new = match &self.active_block {
            Some(active) => active.kind != kind,
            None => true,
        };

        if need_new {
            self.close_active_block(events);

            let idx = self.block_index;
            let block_json = match kind {
                ActiveBlockKind::Text => json!({
                    "type": "text",
                    "text": ""
                }),
                ActiveBlockKind::Thinking => json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": "gemini_thinking"
                }),
            };

            events.push(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": block_json
                }),
            });

            self.active_block = Some(ActiveBlock { index: idx, kind });
        }
    }

    fn close_active_block(&mut self, events: &mut Vec<SseEvent>) {
        if let Some(active) = self.active_block.take() {
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({
                    "type": "content_block_stop",
                    "index": active.index
                }),
            });
            self.block_index += 1;
        }
    }

    pub fn finish(&mut self, events: &mut Vec<SseEvent>) {
        if matches!(
            self.terminal,
            TerminalState::SuccessEmitted
                | TerminalState::ProviderFailed
                | TerminalState::ProtocolFailed
        ) {
            return;
        }
        match self.transport_close_checked() {
            Ok(terminal) => events.extend(terminal),
            Err(error) if self.terminal == TerminalState::ProtocolFailed => {
                events.push(protocol_error_event(error.to_string()));
            }
            Err(_) => {}
        }
    }

    pub fn finish_stream(
        &mut self,
        finish_reason: &str,
        events: &mut Vec<SseEvent>,
    ) -> Result<(), GeminiSemanticError> {
        match self.terminal {
            TerminalState::SuccessPending => {
                if self.last_finish_reason.as_deref() != Some(finish_reason) {
                    self.terminal = TerminalState::ProtocolFailed;
                    return Err(GeminiSemanticError::protocol(
                        "Gemini compatibility finish conflicts with the provider finish",
                    ));
                }
                events.extend(self.transport_close_checked()?);
                return Ok(());
            }
            TerminalState::SuccessEmitted
            | TerminalState::ProviderFailed
            | TerminalState::ProtocolFailed => return Ok(()),
            TerminalState::Open => {}
        }
        if !matches!(finish_reason, "STOP" | "MAX_TOKENS" | "SAFETY") {
            self.terminal = TerminalState::ProtocolFailed;
            return Err(GeminiSemanticError::protocol(format!(
                "unsupported Gemini finishReason {finish_reason}"
            )));
        }
        self.last_finish_reason = Some(finish_reason.to_string());
        self.terminal = TerminalState::SuccessPending;
        if !self.started {
            self.started = true;
            events.push(self.message_start_event());
        }
        events.extend(self.transport_close_checked()?);
        Ok(())
    }

    pub fn transport_close_checked(&mut self) -> Result<Vec<SseEvent>, GeminiSemanticError> {
        if self.terminal != TerminalState::SuccessPending {
            self.terminal = TerminalState::ProtocolFailed;
            return Err(GeminiSemanticError::protocol(
                "Gemini transport closed without one supported provider finish",
            ));
        }
        let mut events = Vec::new();
        self.close_active_block(&mut events);
        let stop_reason = self.stop_reason();
        events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
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
            data: json!({
                "type": "message_stop"
            }),
        });

        self.terminal = TerminalState::SuccessEmitted;
        Ok(events)
    }

    /// Return full final Anthropic response JSON for non-streaming consumers.
    pub fn final_json(&self) -> Value {
        self.final_json_checked().unwrap_or_else(|error| {
            translate_gemini_error_val(&json!({"message": error.to_string()}))
        })
    }

    pub fn final_json_checked(&self) -> Result<Value, GeminiSemanticError> {
        if self.terminal != TerminalState::SuccessEmitted || !self.accumulate_content {
            return Err(GeminiSemanticError::protocol(
                "Gemini final JSON requested before checked unary completion",
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
        if self.saw_tool_use {
            "tool_use"
        } else {
            match self.last_finish_reason.as_deref() {
                Some("MAX_TOKENS") => "max_tokens",
                Some("SAFETY") => "stop_sequence",
                _ => "end_turn",
            }
        }
    }
}

fn validate_usage(
    usage: Option<&Value>,
) -> Result<(Option<u64>, Option<u64>), GeminiSemanticError> {
    let Some(usage) = usage else {
        return Ok((None, None));
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| GeminiSemanticError::protocol("Gemini usageMetadata must be an object"))?;
    let parse = |key: &str| -> Result<Option<u64>, GeminiSemanticError> {
        match usage.get(key) {
            None => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                GeminiSemanticError::protocol(format!(
                    "Gemini usageMetadata.{key} must be a non-negative integer"
                ))
            }),
        }
    };
    let input = parse("promptTokenCount")?;
    let output = parse("candidatesTokenCount")?;
    let _ = parse("totalTokenCount")?;
    Ok((input, output))
}

fn protocol_error_event(message: String) -> SseEvent {
    SseEvent {
        event: "error".to_string(),
        data: json!({
            "type": "error",
            "error": {"type": "api_error", "message": message}
        }),
    }
}

fn encode_tool_use_id(signature: &str) -> String {
    format!(
        "{GEMINI_TOOL_USE_ID_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(signature)
    )
}

/// Translate Gemini error JSON payload to Anthropic error envelope.
pub fn translate_gemini_error_val(error: &Value) -> Value {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Gemini backend error");

    let status_str = error.get("status").and_then(Value::as_str).unwrap_or("");

    let error_type = if status_str == "RESOURCE_EXHAUSTED" || message.contains("quota") {
        "rate_limit_error"
    } else if status_str == "INVALID_ARGUMENT" {
        "invalid_request_error"
    } else {
        "api_error"
    };

    json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    })
}

/// Map HTTP status code and body string from Gemini into an [`AdapterError`].
pub fn map_gemini_error(status: StatusCode, body: &str) -> AdapterError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error_val = parsed.as_ref().and_then(|v| {
        if v.get("error").is_some() {
            v.get("error")
        } else if v.get("message").is_some() || v.get("status").is_some() {
            Some(v)
        } else {
            None
        }
    });

    let (error_type, message) = if let Some(err) = error_val {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or(body);
        let status_str = err.get("status").and_then(Value::as_str).unwrap_or("");
        let err_type =
            if status_str == "RESOURCE_EXHAUSTED" || status == StatusCode::TOO_MANY_REQUESTS {
                "rate_limit_error"
            } else {
                "api_error"
            };
        (err_type, msg.to_string())
    } else {
        ("api_error", body.to_string())
    };

    let error_body = json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    });

    AdapterError {
        message,
        response: Box::new((status, axum::Json(error_body)).into_response()),
        failure: Some(crate::adapters::AdapterFailure::UpstreamStatus(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_machine_emits_text_stream() {
        let mut machine = GeminiSseMachine::new("gemini-3-flash-preview");

        let chunk1 = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello "}],
                    "role": "model"
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 1
            }
        });

        let events1 = machine.process_chunk_checked(&chunk1).unwrap();
        assert_eq!(events1[0].event, "message_start");
        assert_eq!(events1[1].event, "content_block_start");
        assert_eq!(events1[2].event, "content_block_delta");
        assert_eq!(events1[2].data["delta"]["text"], "Hello ");

        let chunk2 = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "world!"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 3
            }
        });

        let mut events2 = machine.process_chunk_checked(&chunk2).unwrap();
        events2.extend(machine.transport_close_checked().unwrap());
        assert_eq!(events2[0].event, "content_block_delta");
        assert_eq!(events2[0].data["delta"]["text"], "world!");
        assert_eq!(events2[1].event, "content_block_stop");
        assert_eq!(events2[2].event, "message_delta");
        assert_eq!(events2[2].data["delta"]["stop_reason"], "end_turn");
        assert_eq!(events2[3].event, "message_stop");
    }

    #[test]
    fn sse_machine_emits_function_call() {
        let mut machine = GeminiSseMachine::new("gemini-3.1-pro-preview");

        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "get_weather",
                            "args": { "location": "Paris" }
                        },
                        "thoughtSignature": "weather-signature"
                    }],
                    "role": "model"
                },
                "finishReason": "STOP"
            }]
        });

        let mut events = machine.process_chunk_checked(&chunk).unwrap();
        events.extend(machine.transport_close_checked().unwrap());
        assert_eq!(events[0].event, "message_start");
        assert_eq!(events[1].event, "content_block_start");
        assert_eq!(events[1].data["content_block"]["type"], "tool_use");
        assert_eq!(events[1].data["content_block"]["name"], "get_weather");
        assert_eq!(events[2].event, "content_block_delta");
        assert_eq!(events[3].event, "content_block_stop");
        assert_eq!(events[4].event, "message_delta");
        assert_eq!(events[4].data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn translate_gemini_error_resource_exhausted() {
        let err_json = json!({
            "code": 429,
            "message": "Resource has been exhausted (e.g. check quota).",
            "status": "RESOURCE_EXHAUSTED"
        });

        let res = translate_gemini_error_val(&err_json);
        assert_eq!(res["type"], "error");
        assert_eq!(res["error"]["type"], "rate_limit_error");
        assert!(res["error"]["message"].to_string().contains("exhausted"));
    }
}
