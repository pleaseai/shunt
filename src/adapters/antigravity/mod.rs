//! Antigravity CLI adapter (`agy`).
//!
//! Translates incoming Anthropic Messages requests into `agy` CLI invocations (`agy -p "<prompt>"`),
//! allowing Gemini models to execute via Google's Antigravity gRPC backend with full capacity.

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    routing::Route,
    server::AppState,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityAdapter;

impl Adapter for AntigravityAdapter {
    fn forward<'a>(
        &'a self,
        _state: AppState,
        route: Route,
        _uri: &'a Uri,
        _headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            let request: Value = serde_json::from_slice(&body).map_err(|err| AdapterError {
                message: format!("invalid JSON in request: {err}"),
                response: Box::new(StatusCode::BAD_REQUEST.into_response()),
            })?;

            let prompt = extract_antigravity_prompt(&request);
            let is_streaming = request
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let agy_bin = find_agy_binary().ok_or_else(|| AdapterError {
                message: "Antigravity CLI (agy) binary not found. Please install agy or set AGY_BIN environment variable.".to_string(),
                response: Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()),
            })?;

            let mut cmd = tokio::process::Command::new(&agy_bin);
            cmd.arg("-p").arg(&prompt);
            cmd.arg("--model").arg(&route.upstream_model);

            // Add effort for 3.x models or explicitly set effort
            if route.upstream_model.contains("3.") || route.effort.is_some() {
                let effort = route.effort.as_deref().unwrap_or("medium");
                cmd.arg("--effort").arg(effort);
            }

            let output = cmd.output().await.map_err(|err| AdapterError {
                message: format!("failed to execute agy CLI: {err}"),
                response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            })?;

            if !output.status.success() {
                let err_msg = String::from_utf8_lossy(&output.stderr);
                return Err(AdapterError {
                    message: format!("agy CLI execution failed: {err_msg}"),
                    response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                });
            }

            let stdout_text = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if is_streaming {
                let sse_text = format_antigravity_sse(&route.model, &stdout_text);
                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream; charset=utf-8")
                    .header("Cache-Control", "no-cache")
                    .body(Body::from(sse_text))
                    .map_err(|err| AdapterError {
                        message: format!("failed to build SSE response: {err}"),
                        response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                    })?;
                Ok((StatusCode::OK, response))
            } else {
                let json_val = format_antigravity_json(&route.model, &stdout_text);
                let mut headers = HeaderMap::new();
                headers.insert(
                    "content-type",
                    axum::http::HeaderValue::from_static("application/json"),
                );
                let response = (StatusCode::OK, headers, axum::Json(json_val)).into_response();
                Ok((StatusCode::OK, response))
            }
        })
    }
}

pub fn extract_antigravity_prompt(request: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(sys) = request.get("system") {
        if let Some(s) = sys.as_str() {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        } else if let Some(arr) = sys.as_array() {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }

    if let Some(msgs) = request.get("messages").and_then(Value::as_array) {
        for msg in msgs {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            if let Some(content) = msg.get("content") {
                if let Some(t) = content.as_str() {
                    parts.push(format!("{role}: {t}"));
                } else if let Some(arr) = content.as_array() {
                    for b in arr {
                        if b.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                parts.push(format!("{role}: {t}"));
                            }
                        }
                    }
                }
            }
        }
    }

    parts.join("\n\n")
}

pub fn find_agy_binary() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("AGY_BIN") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".gemini/antigravity-cli/bin/agy");
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("agy");
            if p.is_file() {
                return Some(p);
            }
        }
    }

    None
}

pub fn format_antigravity_json(model: &str, text: &str) -> Value {
    let msg_id = format!("msg_agy_{:016x}", rand::random::<u64>());
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 1,
            "output_tokens": text.len() / 4
        }
    })
}

pub fn format_antigravity_sse(model: &str, text: &str) -> String {
    let msg_id = format!("msg_agy_{:016x}", rand::random::<u64>());
    let mut out = String::new();

    let msg_start = json!({
        "type": "message_start",
        "message": {
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": { "input_tokens": 1, "output_tokens": 0 }
        }
    });
    out.push_str(&format!("event: message_start\ndata: {}\n\n", msg_start));

    let block_start = json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "text", "text": "" }
    });
    out.push_str(&format!(
        "event: content_block_start\ndata: {}\n\n",
        block_start
    ));

    let delta = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": text }
    });
    out.push_str(&format!("event: content_block_delta\ndata: {}\n\n", delta));

    let block_stop = json!({
        "type": "content_block_stop",
        "index": 0
    });
    out.push_str(&format!(
        "event: content_block_stop\ndata: {}\n\n",
        block_stop
    ));

    let msg_delta = json!({
        "type": "message_delta",
        "delta": { "stop_reason": "end_turn", "stop_sequence": null },
        "usage": { "output_tokens": text.len() / 4 }
    });
    out.push_str(&format!("event: message_delta\ndata: {}\n\n", msg_delta));

    let msg_stop = json!({ "type": "message_stop" });
    out.push_str(&format!("event: message_stop\ndata: {}\n\n", msg_stop));

    out
}
