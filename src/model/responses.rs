use axum::http::StatusCode;
use serde_json::{json, Value};

pub use crate::model::responses_request::translate_request;

#[derive(Debug, Clone)]
pub struct ResponseEvent {
    pub event: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone)]
struct OpenBlock {
    index: usize,
    kind: BlockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
}

#[derive(Debug, Clone)]
pub struct AnthropicSseMachine {
    id: String,
    model: String,
    started: bool,
    stopped: bool,
    index: usize,
    open: Option<OpenBlock>,
    saw_tool: bool,
    output_tokens: u64,
    content: Vec<Value>,
    text_buffer: String,
    tool_buffer: Option<ToolBuffer>,
}

#[derive(Debug, Clone)]
struct ToolBuffer {
    id: String,
    name: String,
    json: String,
}

impl AnthropicSseMachine {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: "msg_responses".to_string(),
            model: model.into(),
            started: false,
            stopped: false,
            index: 0,
            open: None,
            saw_tool: false,
            output_tokens: 0,
            content: Vec::new(),
            text_buffer: String::new(),
            tool_buffer: None,
        }
    }

    pub fn apply(&mut self, event: ResponseEvent) -> Vec<String> {
        if self.stopped {
            return Vec::new();
        }
        let name = event.event.as_deref().unwrap_or("");
        match name {
            "response.created" | "response.in_progress" => self.start(&event.data),
            "response.output_item.added" => self.output_item_added(&event.data),
            "response.output_text.delta" => self.text_delta(&event.data),
            "response.output_text.done" | "response.content_part.done" => {
                self.close_current(BlockKind::Text)
            }
            "response.function_call_arguments.delta" => self.arguments_delta(&event.data),
            "response.function_call_arguments.done" | "response.output_item.done" => {
                self.close_current(BlockKind::Tool)
            }
            "response.completed" | "response.done" => self.complete(&event.data),
            "error" | "response.failed" => {
                self.stopped = true;
                vec![sse(
                    "error",
                    &map_error_value(&event.data, StatusCode::BAD_GATEWAY),
                )]
            }
            _ => Vec::new(),
        }
    }

    pub fn finish(&mut self) -> Vec<String> {
        if self.stopped {
            return Vec::new();
        }
        let mut out = self.close_any();
        out.extend(self.stop_events("end_turn"));
        out
    }

    pub fn final_json(&mut self) -> Value {
        if !self.stopped {
            let _ = self.finish();
        }
        json!({
            "id": self.id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": self.content,
            "stop_reason": if self.saw_tool { "tool_use" } else { "end_turn" },
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": self.output_tokens},
        })
    }

    fn start(&mut self, data: &Value) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if let Some(id) = data.pointer("/response/id").or_else(|| data.get("id")) {
            if let Some(id) = id.as_str() {
                self.id = id.to_string();
            }
        }
        vec![
            sse(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }),
            ),
            sse("ping", &json!({"type": "ping"})),
        ]
    }

    fn output_item_added(&mut self, data: &Value) -> Vec<String> {
        let item = data.get("item").unwrap_or(data);
        match item.get("type").and_then(Value::as_str) {
            Some("message") => self.open_text(),
            Some("function_call") => self.open_tool(item),
            _ => Vec::new(),
        }
    }

    fn open_text(&mut self) -> Vec<String> {
        let mut out = self.close_any();
        self.open = Some(OpenBlock {
            index: self.index,
            kind: BlockKind::Text,
        });
        self.text_buffer.clear();
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        out
    }

    fn open_tool(&mut self, item: &Value) -> Vec<String> {
        let mut out = self.close_any();
        let id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.saw_tool = true;
        self.tool_buffer = Some(ToolBuffer {
            id: id.clone(),
            name: name.clone(),
            json: String::new(),
        });
        self.open = Some(OpenBlock {
            index: self.index,
            kind: BlockKind::Tool,
        });
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
            }),
        ));
        out
    }

    fn text_delta(&mut self, data: &Value) -> Vec<String> {
        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        self.text_buffer.push_str(delta);
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "text_delta", "text": delta}
            }),
        )]
    }

    fn arguments_delta(&mut self, data: &Value) -> Vec<String> {
        let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
        if let Some(tool) = &mut self.tool_buffer {
            tool.json.push_str(delta);
        }
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.open_index(),
                "delta": {"type": "input_json_delta", "partial_json": delta}
            }),
        )]
    }

    fn close_current(&mut self, expected: BlockKind) -> Vec<String> {
        if self.open.as_ref().map(|block| block.kind) != Some(expected) {
            return Vec::new();
        }
        self.close_any()
    }

    fn close_any(&mut self) -> Vec<String> {
        let Some(open) = self.open.take() else {
            return Vec::new();
        };
        match open.kind {
            BlockKind::Text => {
                self.content
                    .push(json!({"type": "text", "text": self.text_buffer}));
                self.text_buffer.clear();
            }
            BlockKind::Tool => {
                if let Some(tool) = self.tool_buffer.take() {
                    let input = serde_json::from_str(&tool.json).unwrap_or_else(|_| json!({}));
                    self.content.push(json!({
                        "type": "tool_use",
                        "id": tool.id,
                        "name": tool.name,
                        "input": input
                    }));
                }
            }
        }
        self.index += 1;
        vec![sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": open.index}),
        )]
    }

    fn complete(&mut self, data: &Value) -> Vec<String> {
        self.read_usage(data);
        let mut out = self.close_any();
        let stop_reason = if self.saw_tool {
            "tool_use"
        } else {
            "end_turn"
        };
        out.extend(self.stop_events(stop_reason));
        out
    }

    fn stop_events(&mut self, stop_reason: &str) -> Vec<String> {
        self.stopped = true;
        vec![
            sse(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                    "usage": {"output_tokens": self.output_tokens}
                }),
            ),
            sse("message_stop", &json!({"type": "message_stop"})),
        ]
    }

    fn read_usage(&mut self, data: &Value) {
        let usage = data
            .pointer("/response/usage")
            .or_else(|| data.get("usage"));
        if let Some(tokens) = usage
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_u64)
        {
            self.output_tokens = tokens;
        }
    }

    fn open_index(&self) -> usize {
        self.open
            .as_ref()
            .map(|block| block.index)
            .unwrap_or(self.index)
    }
}

pub fn parse_sse_events(input: &str) -> Vec<ResponseEvent> {
    input
        .split("\n\n")
        .filter_map(|frame| {
            let mut event = None;
            let mut data = Vec::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.trim_start());
                }
            }
            let data = data.join("\n");
            if data.is_empty() || data == "[DONE]" {
                return None;
            }
            serde_json::from_str(&data)
                .ok()
                .map(|data| ResponseEvent { event, data })
        })
        .collect()
}

pub fn map_error_value(value: &Value, status: StatusCode) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(status),
            "message": error_message(value)
        }
    })
}

pub fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        _ => "api_error",
    }
}

fn error_message(value: &Value) -> String {
    // OpenAI Responses errors use {"error":{"message":...}} or {"message":...};
    // the ChatGPT Codex backend uses {"detail":...}. Surface whichever is present
    // so the client sees the real reason (e.g. "The 'X' model is not supported").
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("upstream request failed")
        .to_string()
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}
