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
    error::ShuntError,
    request::RequestBody,
    routing::Route,
    server::AppState,
};

/// `--effort` values the Antigravity CLI accepts per model, with the default to
/// send when the route configures none: `(model, supported, default)`.
///
/// `agy` validates `--effort` in two stages. Its flag parser accepts
/// `low|medium|high` for every model, and only then does the model reject a
/// value it does not offer — `gemini-3.1-pro` exposes a two-position slider
/// (`low`/`high`) and fails the whole invocation on `medium`, which the CLI
/// reports as `gemini-3.1-pro has no "medium" effort`. Probing the flag parser
/// therefore does not reveal the real per-model set; each row below was
/// verified by invoking the model itself against Antigravity CLI 1.1.8.
///
/// Table-driven so a new model is one row rather than another branch
/// (AGENTS.md: prefer table-driven config additions over hardcoded provider
/// logic). A model absent from this table is left to `agy`'s own default.
const ANTIGRAVITY_EFFORTS: &[(&str, &[&str], &str)] = &[
    ("gemini-3.1-pro", &["low", "high"], "high"),
    ("gemini-3.6-flash", &["low", "medium", "high"], "medium"),
    ("gemini-3.5-flash", &["low", "medium", "high"], "medium"),
    ("gemini-3-flash", &["low", "medium", "high"], "medium"),
];

/// Resolve the `--effort` argument for a route.
///
/// `Ok(None)` means send no `--effort` flag at all. An unknown model with no
/// configured effort lands there deliberately: guessing a value the model may
/// not accept is what made every `gemini-3.1-pro` request fail, so an
/// unrecognised model defers to `agy` instead of to us.
///
/// `Err` carries an operator-facing message for a configured effort the model
/// does not offer. Catching it here turns what `agy` would report as an opaque
/// upstream failure into a 400 that names the valid values.
fn resolve_effort(model: &str, configured: Option<&str>) -> Result<Option<String>, String> {
    let known = ANTIGRAVITY_EFFORTS
        .iter()
        .find(|(candidate, _, _)| *candidate == model);
    match (known, configured) {
        (Some((_, supported, _)), Some(effort)) if !supported.contains(&effort) => Err(format!(
            "model {model} does not support effort \"{effort}\" (supported: {})",
            supported.join(", ")
        )),
        (_, Some(effort)) => Ok(Some(effort.to_string())),
        (Some((_, _, default)), None) => Ok(Some((*default).to_string())),
        (None, None) => Ok(None),
    }
}

/// Shape a missing `agy` binary as an Anthropic-form error.
///
/// Worth naming the search path explicitly: a service manager commonly runs
/// shunt under a restricted `PATH`. Homebrew's `brew services` unit sets
/// `PATH=/opt/homebrew/bin:/opt/homebrew/sbin:/usr/bin:/bin:/usr/sbin:/sbin`,
/// which excludes `~/.local/bin` — the default install location for `agy` — so
/// a provider that works in a shell returns 503 under the service with no
/// indication why. `AGY_BIN` is the fix, and the message has to say so.
fn agy_not_found() -> AdapterError {
    let message = "Antigravity CLI (agy) not found on PATH, in ~/.gemini/antigravity-cli/bin, or at $AGY_BIN. \
         Install agy, or set AGY_BIN to its absolute path — a service manager \
         (for example `brew services`) may run shunt with a PATH that excludes it."
        .to_string();
    AdapterError {
        response: Box::new(
            ShuntError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                message.clone(),
            )
            .into_response(),
        ),
        message,
        failure: None,
    }
}

/// Cap on the upstream detail echoed back to the client, in bytes.
const AGY_STDERR_LIMIT: usize = 2000;

/// Shape a failed `agy` invocation as an Anthropic-form error carrying the
/// CLI's own diagnosis.
///
/// The previous code built this message and then paired it with a bare
/// `StatusCode::BAD_GATEWAY`, so the client received a 502 with an empty body
/// while the actual cause — often a precise, actionable line like
/// `gemini-3.1-pro has no "medium" effort` — was discarded at the wire.
/// AGENTS.md requires gateway-owned errors in the Anthropic error shape.
fn agy_failure(detail: &str) -> AdapterError {
    let detail = detail.trim();
    let mut detail = detail
        .char_indices()
        .take_while(|(index, _)| *index < AGY_STDERR_LIMIT)
        .map(|(_, character)| character)
        .collect::<String>();
    if detail.is_empty() {
        detail.push_str("no output");
    }
    let message = format!("Antigravity CLI (agy) failed: {detail}");
    AdapterError {
        response: Box::new(
            ShuntError::new(StatusCode::BAD_GATEWAY, "api_error", message.clone()).into_response(),
        ),
        message,
        failure: None,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityAdapter;

impl Adapter for AntigravityAdapter {
    fn forward<'a>(
        &'a self,
        _state: AppState,
        route: Route,
        _uri: &'a Uri,
        _headers: &'a HeaderMap,
        body: RequestBody,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            let request = body.json();
            let prompt = extract_antigravity_prompt(request);
            let is_streaming = request
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let agy_bin = find_agy_binary().ok_or_else(agy_not_found)?;

            let mut cmd = tokio::process::Command::new(&agy_bin);
            cmd.arg("-p").arg(&prompt);
            cmd.arg("--model").arg(&route.upstream_model);

            // Effort is resolved from a per-model table rather than guessed:
            // the models disagree on which values they accept, and sending an
            // unsupported one fails the whole invocation.
            match resolve_effort(&route.upstream_model, route.effort.as_deref()) {
                Ok(Some(effort)) => {
                    cmd.arg("--effort").arg(effort);
                }
                Ok(None) => {}
                Err(message) => {
                    return Err(AdapterError {
                        response: Box::new(
                            ShuntError::new(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                message.clone(),
                            )
                            .into_response(),
                        ),
                        message,
                        failure: None,
                    });
                }
            }

            let output = cmd.output().await.map_err(|err| {
                agy_failure(&format!("could not execute {}: {err}", agy_bin.display()))
            })?;

            if !output.status.success() {
                return Err(agy_failure(&String::from_utf8_lossy(&output.stderr)));
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
                        failure: None,
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
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    parts.push(format!("{role}: {t}"));
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                                parts.push(format!("{role} tool_use {name}: {input}"));
                            }
                            Some("tool_result") => {
                                let content = b
                                    .get("content")
                                    .map(ToString::to_string)
                                    .unwrap_or_default();
                                parts.push(format!("{role} tool_result: {content}"));
                            }
                            Some("image") => parts.push(format!("{role}: [image omitted]")),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    parts.join("\n\n")
}

pub fn find_agy_binary() -> Option<PathBuf> {
    static CACHE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHE.get_or_init(find_agy_binary_uncached).clone()
}

fn find_agy_binary_uncached() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("AGY_BIN") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Some(p);
        }
    }

    if let Some(home) = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
    {
        let p = home.join(".gemini/antigravity-cli/bin/agy");
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

#[cfg(test)]
mod tests;
