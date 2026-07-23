//! Gemini response and SSE stream translation -> Anthropic Messages SSE events.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{json, Value};

use crate::adapters::AdapterError;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBlockKind {
    Text,
    Thinking,
    ToolUse,
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
    message_id: String,
    started: bool,
    stopped: bool,
    block_index: usize,
    active_block: Option<ActiveBlock>,
    saw_tool_use: bool,
    input_tokens: u64,
    output_tokens: u64,
    last_finish_reason: Option<String>,
    accumulate_content: bool,
    content: Vec<Value>,
}

impl GeminiSseMachine {
    pub fn new(model: impl Into<String>) -> Self {
        let random_id = format!("{:016x}", rand::random::<u64>());
        Self {
            model: model.into(),
            message_id: format!("msg_gemini_{random_id}"),
            started: false,
            stopped: false,
            block_index: 0,
            active_block: None,
            saw_tool_use: false,
            input_tokens: 0,
            output_tokens: 0,
            last_finish_reason: None,
            accumulate_content: true,
            content: Vec::new(),
        }
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Process a parsed Gemini chunk (from SSE or non-streaming JSON)
    /// and return any Anthropic SSE events to emit.
    pub fn process_chunk(&mut self, chunk: &Value) -> Vec<SseEvent> {
        let chunk = if let Some(resp) = chunk.get("response") {
            resp
        } else {
            chunk
        };
        let mut events = Vec::new();

        // 0. Check for error payload in chunk
        if let Some(error_val) = chunk.get("error") {
            let anthropic_err = translate_gemini_error_val(error_val);
            events.push(SseEvent {
                event: "error".to_string(),
                data: anthropic_err,
            });
            return events;
        }

        // 1. Emit message_start if not yet started
        if !self.started {
            self.started = true;

            // Extract usage if present in first chunk
            if let Some(usage) = chunk.get("usageMetadata") {
                if let Some(prompt) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                    self.input_tokens = prompt;
                }
            }

            events.push(SseEvent {
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
                        "usage": {
                            "input_tokens": self.input_tokens,
                            "output_tokens": 0
                        }
                    }
                }),
            });
        }

        // Update usage metadata if present
        if let Some(usage) = chunk.get("usageMetadata") {
            if let Some(prompt) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                self.input_tokens = prompt;
            }
            if let Some(cand) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
                self.output_tokens = cand;
            }
        }

        // 2. Process candidate parts
        let mut finish_reason = None;

        if let Some(candidates) = chunk.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                if let Some(fr) = candidate.get("finishReason").and_then(Value::as_str) {
                    finish_reason = Some(fr.to_string());
                }

                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        self.process_part(part, &mut events);
                    }
                }
            }
        }

        // 3. Process finish reason if present
        if let Some(reason) = finish_reason {
            self.last_finish_reason = Some(reason.clone());
            self.finish_stream(&reason, &mut events);
        }

        events
    }

    fn process_part(&mut self, part: &Value, events: &mut Vec<SseEvent>) {
        // A. Thinking part
        if part.get("thought").and_then(Value::as_bool) == Some(true)
            || part.get("thinking").is_some()
        {
            let text = part
                .get("text")
                .or_else(|| part.get("thinking"))
                .and_then(Value::as_str)
                .unwrap_or("");

            if !text.is_empty() {
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
                            if let Some(existing_thinking) = last.get_mut("thinking") {
                                if let Some(s) = existing_thinking.as_str() {
                                    *existing_thinking = Value::String(format!("{}{}", s, text));
                                }
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
            return;
        }

        // B. Function Call part (Tool Use)
        if let Some(func_call) = part.get("functionCall") {
            self.saw_tool_use = true;

            // Close existing block if text/thinking
            if let Some(active) = &self.active_block {
                if active.kind != ActiveBlockKind::ToolUse {
                    self.close_active_block(events);
                }
            }

            let name = func_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool");
            let args = func_call.get("args").cloned().unwrap_or_else(|| json!({}));

            let tool_use_id = format!("call_{:012x}", rand::random::<u64>());
            let idx = self.block_index;

            events.push(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": name,
                        "input": {}
                    }
                }),
            });

            let args_json_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
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
                    "id": tool_use_id,
                    "name": name,
                    "input": args
                }));
            }

            self.block_index += 1;
            return;
        }

        // C. Standard Text part
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
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
                            if let Some(existing_text) = last.get_mut("text") {
                                if let Some(s) = existing_text.as_str() {
                                    *existing_text = Value::String(format!("{}{}", s, text));
                                }
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
                ActiveBlockKind::ToolUse => unreachable!(),
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
        let reason = self
            .last_finish_reason
            .clone()
            .unwrap_or_else(|| "STOP".to_string());
        self.finish_stream(&reason, events);
    }

    pub fn finish_stream(&mut self, finish_reason: &str, events: &mut Vec<SseEvent>) {
        if self.stopped {
            return;
        }

        self.close_active_block(events);

        let stop_reason = if self.saw_tool_use {
            "tool_use"
        } else {
            match finish_reason {
                "STOP" => "end_turn",
                "MAX_TOKENS" => "max_tokens",
                "SAFETY" => "stop_sequence",
                _ => "end_turn",
            }
        };

        events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
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

        self.stopped = true;
    }

    /// Return full final Anthropic response JSON for non-streaming consumers.
    pub fn final_json(&self) -> Value {
        let stop_reason = if self.saw_tool_use {
            "tool_use"
        } else {
            match self.last_finish_reason.as_deref() {
                Some("MAX_TOKENS") => "max_tokens",
                Some("SAFETY") => "stop_sequence",
                _ => "end_turn",
            }
        };

        json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "content": self.content,
            "model": self.model,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens
            }
        })
    }
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

        let events1 = machine.process_chunk(&chunk1);
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

        let events2 = machine.process_chunk(&chunk2);
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
                        }
                    }],
                    "role": "model"
                },
                "finishReason": "STOP"
            }]
        });

        let events = machine.process_chunk(&chunk);
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
