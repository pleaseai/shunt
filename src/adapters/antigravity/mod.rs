//! Antigravity CLI adapter (`agy`).
//!
//! `agy` is not a text-completion endpoint — it is a full agent with its own
//! tool set (file edits, shell, search, browser) and its own loop. This adapter
//! therefore runs it in agentic print mode and translates its
//! `--output-format stream-json` events back into Anthropic Messages SSE, so a
//! Gemini-routed turn can actually do work rather than only describe it.
//!
//! Consequences worth knowing before routing traffic here:
//!
//! - Tools are `agy`'s, not the caller's. `tools` supplied on the Messages
//!   request are not forwarded, and no `tool_use` block is ever returned; the
//!   CLI resolves its own tool calls internally and returns finished work.
//! - It runs with `--dangerously-skip-permissions`, because a print-mode run
//!   has no interactive channel to approve a permission prompt on. The
//!   workspace it may touch is bounded by [`resolve_workspace`].

pub mod models;
pub mod stream;

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::{convert::Infallible, path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    request::RequestBody,
    routing::Route,
    server::AppState,
};

use self::stream::{AgyEnd, Translator};

/// Wall-clock cap handed to `agy --print-timeout`.
///
/// The CLI's own default is 5 minutes, which truncates genuine multi-step
/// agent runs and surfaces to the caller as a turn that delivered nothing.
const PRINT_TIMEOUT: &str = "30m";

/// Environment override for the directory `agy` is allowed to work in.
const WORKSPACE_ENV: &str = "SHUNT_AGY_WORKSPACE";

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityAdapter;

impl Adapter for AntigravityAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
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

            let agy_bin = find_agy_binary().ok_or_else(|| AdapterError {
                message: "Antigravity CLI (agy) binary not found. Please install agy or set AGY_BIN environment variable.".to_string(),
                response: Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()),
                failure: None,
            })?;

            let workspace = resolve_workspace(request);
            let matrix = models::effort_matrix(&agy_bin).await;
            let effort =
                models::resolve_effort(matrix, &route.upstream_model, route.effort.as_deref());

            let mut cmd = Command::new(&agy_bin);
            cmd.arg("-p").arg(&prompt);
            cmd.arg("--model").arg(&route.upstream_model);
            if let Some(effort) = effort {
                cmd.arg("--effort").arg(effort);
            }
            cmd.arg("--output-format").arg("stream-json");
            // Print mode cannot service an interactive approval prompt.
            cmd.arg("--dangerously-skip-permissions");
            cmd.arg("--print-timeout").arg(PRINT_TIMEOUT);
            cmd.arg("--add-dir").arg(&workspace);
            // Without this the agent inherits the gateway process's directory
            // and operates on whatever tree shunt happened to be started in.
            cmd.current_dir(&workspace);
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = cmd.spawn().map_err(|err| AdapterError {
                message: format!("failed to execute agy CLI: {err}"),
                response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                failure: None,
            })?;

            let stdout = child.stdout.take().ok_or_else(|| AdapterError {
                message: "agy CLI produced no stdout pipe".to_string(),
                response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                failure: None,
            })?;

            let message_id = format!("msg_agy_{:016x}", rand::random::<u64>());
            let mut translator = Translator::new(&route.model, message_id);
            let mut lines = BufReader::new(stdout).lines();

            if is_streaming {
                let stream_state = (lines, translator, child, false);
                let sse_stream = futures_util::stream::unfold(
                    stream_state,
                    |(mut lines, mut translator, mut child, mut finished)| async move {
                        if finished {
                            return None;
                        }
                        loop {
                            match lines.next_line().await {
                                Ok(Some(line)) => {
                                    let chunk = translator.on_line(&line);
                                    if chunk.is_empty() {
                                        continue;
                                    }
                                    return Some((
                                        Ok::<_, Infallible>(axum::body::Bytes::from(chunk)),
                                        (lines, translator, child, finished),
                                    ));
                                }
                                // EOF or a broken pipe both mean the run is
                                // over; close the message either way so the
                                // client never hangs on an unterminated stream.
                                _ => {
                                    finished = true;
                                    let mut tail = String::new();
                                    let failure = match translator.end() {
                                        Some(AgyEnd::Failed(message)) => Some(message.clone()),
                                        _ => None,
                                    };
                                    if let Some(message) = failure {
                                        let message = format!("\n\n[agy error] {message}");
                                        tail.push_str(&translator.on_text(&message));
                                    }
                                    tail.push_str(&translator.finish());
                                    let _ = child.wait().await;
                                    return Some((
                                        Ok::<_, Infallible>(axum::body::Bytes::from(tail)),
                                        (lines, translator, child, finished),
                                    ));
                                }
                            }
                        }
                    },
                );

                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream; charset=utf-8")
                    .header("Cache-Control", "no-cache")
                    // `agy` can be silent for tens of seconds before its first
                    // event while the CLI boots and the model takes its first
                    // turn. The shared keepalive covers that gap; the ping
                    // frames the translator emits per tool step then report
                    // genuine progress on top of it.
                    .body(Body::from_stream(crate::keepalive::with_pings(
                        sse_stream,
                        std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds),
                    )))
                    .map_err(|err| AdapterError {
                        message: format!("failed to build SSE response: {err}"),
                        response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                        failure: None,
                    })?;
                return Ok((StatusCode::OK, response));
            }

            while let Ok(Some(line)) = lines.next_line().await {
                let _ = translator.on_line(&line);
            }
            let status = child.wait().await;

            if let Some(AgyEnd::Failed(message)) = translator.end() {
                return Err(AdapterError {
                    message: format!("agy CLI execution failed: {message}"),
                    response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                    failure: None,
                });
            }
            // No terminal `result` event means the CLI died before finishing.
            if translator.end().is_none() {
                let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                return Err(AdapterError {
                    message: format!("agy CLI exited without a result (status {code})"),
                    response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                    failure: None,
                });
            }

            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                axum::http::HeaderValue::from_static("application/json"),
            );
            let response =
                (StatusCode::OK, headers, axum::Json(translator.to_message())).into_response();
            Ok((StatusCode::OK, response))
        })
    }
}

/// Directory `agy` is launched in and granted via `--add-dir`.
///
/// Resolution order, first hit wins:
/// 1. `SHUNT_AGY_WORKSPACE`, for deployments that pin the workspace explicitly.
/// 2. A `working directory: <path>` line in the request's system prompt. Agent
///    harnesses state the caller's cwd there, and it is the only signal the
///    Messages protocol carries about where the caller actually is.
/// 3. The gateway's own directory, matching prior behaviour.
///
/// Candidates that are not existing directories are skipped, so a stale or
/// malformed hint degrades to the fallback instead of failing the run.
pub fn resolve_workspace(request: &Value) -> PathBuf {
    if let Some(dir) = std::env::var_os(WORKSPACE_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
    {
        return dir;
    }
    if let Some(dir) = system_prompt_text(request)
        .as_deref()
        .and_then(parse_working_directory)
        .filter(|path| path.is_dir())
    {
        return dir;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn system_prompt_text(request: &Value) -> Option<String> {
    let system = request.get("system")?;
    if let Some(text) = system.as_str() {
        return Some(text.to_string());
    }
    let blocks = system.as_array()?;
    let joined = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn parse_working_directory(system: &str) -> Option<PathBuf> {
    const NEEDLE: &str = "working directory:";
    system.lines().find_map(|line| {
        let lowered = line.to_ascii_lowercase();
        let start = lowered.find(NEEDLE)? + NEEDLE.len();
        let value = line[start..].trim().trim_matches('`');
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
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
