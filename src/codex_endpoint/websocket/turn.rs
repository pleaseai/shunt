//! Per-turn forwarding for inbound Responses WebSocket sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::ws::Message,
    http::{HeaderMap, HeaderName},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    adapters::responses::codex_continuation::{build_transcript, decide, StoredContinuation},
    codex_endpoint::{forward_turn, pool_sticky_key},
    server::AppState,
};

use crate::codex_endpoint::frame::{
    build_ws_error_frame, parse_payload_type, parse_sse_block, terminal_status_from_type,
    BoundedSseFrameBuffer, MAX_CLIENT_SSE_FRAME_BYTES,
};

/// Per-connection continuation captured from the last completed turn, so an
/// incremental `response.create` frame (`previous_response_id` + input delta)
/// can be expanded to the full input the HTTP upstream requires: the ChatGPT
/// HTTP backend rejects `previous_response_id` outright
/// (`Unsupported parameter: previous_response_id`), unlike the WebSocket
/// backend the client is speaking. Lives only for one socket, like the
/// client's own continuation state.
pub(super) type SharedContinuation = Arc<Mutex<Option<StoredContinuation>>>;

pub(super) struct TurnContext {
    pub(super) state: AppState,
    pub(super) model: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) headers: HeaderMap,
    pub(super) auth_header: Option<HeaderName>,
    pub(super) admin_header: Option<HeaderName>,
    pub(super) body: Bytes,
    pub(super) generation: u64,
    pub(super) current_generation: Arc<AtomicU64>,
    pub(super) out_tx: mpsc::Sender<(u64, Message)>,
    pub(super) continuation: SharedContinuation,
    /// The full logical input `body` carries after expansion, and the
    /// signature of its non-input fields; recorded against the upstream
    /// response so the next incremental frame can extend it.
    pub(super) logical_input: Vec<serde_json::Value>,
    pub(super) body_signature: String,
}

/// Prepare a `response.create` body for the HTTP upstream: drop
/// `previous_response_id` (rejected there) and, when it names this
/// connection's stored response, expand a delta-only `input` to the full
/// transcript. A frame whose input already extends the transcript (the
/// client sent the full history with the id as a hint) forwards unchanged
/// apart from the dropped field. Returns the full logical input the
/// prepared body carries.
pub(super) fn prepare_body(
    raw_json: &mut serde_json::Value,
    continuation: &SharedContinuation,
) -> Vec<serde_json::Value> {
    let Some(obj) = raw_json.as_object_mut() else {
        return Vec::new();
    };
    let previous = obj
        .remove("previous_response_id")
        .and_then(|value| value.as_str().map(ToOwned::to_owned));
    let input = obj
        .get("input")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(previous) = previous else {
        return input;
    };
    let stored = continuation.lock().ok().and_then(|guard| guard.clone());
    let Some(stored) = stored.filter(|stored| stored.response_id == previous) else {
        return input;
    };
    if decide(&stored, raw_json).is_some() {
        return input;
    }
    let mut full = stored.transcript.clone();
    full.extend(input.iter().cloned());
    if let Some(obj) = raw_json.as_object_mut() {
        obj.insert("input".to_string(), serde_json::Value::Array(full.clone()));
    }
    full
}

/// Record a terminal upstream event against the connection continuation: a
/// completed response becomes the prefix the next incremental frame extends;
/// a failed or incomplete one clears it, since the backend state the client
/// would name no longer matches anything shunt can expand from.
fn record_terminal(
    continuation: &SharedContinuation,
    logical_input: &[serde_json::Value],
    body_signature: &str,
    payload: &str,
    terminal: crate::codex_endpoint::frame::TerminalStatus,
) {
    use crate::codex_endpoint::frame::TerminalStatus;
    let Ok(mut guard) = continuation.lock() else {
        return;
    };
    if terminal != TerminalStatus::Completed {
        *guard = None;
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        *guard = None;
        return;
    };
    let Some(response) = value.get("response") else {
        *guard = None;
        return;
    };
    let Some(id) = response.get("id").and_then(serde_json::Value::as_str) else {
        *guard = None;
        return;
    };
    if id.is_empty() {
        *guard = None;
        return;
    }
    let output = response
        .get("output")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    *guard = Some(StoredContinuation {
        response_id: id.to_string(),
        signature: body_signature.to_string(),
        transcript: build_transcript(logical_input, &output),
        turn_state: None,
    });
}

/// The `(type, code, message)` for a non-success upstream body. The ChatGPT
/// backend answers in `{"detail": "…"}` shape, not the OpenAI error envelope;
/// surface that detail instead of masking it as a generic upstream failure.
fn upstream_error_parts(body: &serde_json::Value) -> (String, String, String) {
    if let Some(detail) = body.get("detail").and_then(serde_json::Value::as_str) {
        return (
            "api_error".to_string(),
            "upstream_error".to_string(),
            detail.to_string(),
        );
    }
    let detail = body.get("error").unwrap_or(body);
    let pick = |key: &str, fallback: &str| {
        detail
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    (
        pick("type", "api_error"),
        pick("code", "upstream_error"),
        pick("message", "Upstream request failed"),
    )
}

pub(super) async fn run_turn(context: TurnContext) {
    let TurnContext {
        state,
        model,
        session_id,
        headers,
        auth_header,
        admin_header,
        body,
        generation: turn_gen,
        current_generation,
        out_tx,
        continuation,
        logical_input,
        body_signature,
    } = context;
    let is_current = || current_generation.load(Ordering::Relaxed) == turn_gen;

    // A WebSocket connection may outlive several config reloads.  Refresh at
    // turn start (rather than once at upgrade time) so each live turn captures
    // one immutable runtime snapshot while later turns observe newer config.
    let state = state.refreshed();
    let Some(codex_endpoint) = state.config.server.codex_endpoint.as_ref() else {
        let err_frame = build_ws_error_frame(
            502,
            "api_error",
            "configuration_error",
            "codex endpoint is no longer configured",
            None,
        );
        let _ = out_tx
            .send((turn_gen, Message::Text(err_frame.into())))
            .await;
        return;
    };
    // Re-authenticate every turn against the refreshed config, and key the
    // pool with the client name *this* turn resolved: a reload can revoke the
    // token or move it to another client, and a pool key frozen at upgrade time
    // would keep pinning the old client's accounts.
    let inbound_client = match crate::codex_endpoint::authenticate_inbound(
        state.inbound_auth.as_deref(),
        &state.inbound_jwks,
        &headers,
        &codex_endpoint.provider,
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            // A JWT issuer key set that became unreachable mid-connection is a
            // `503`, like the upgrade gate answers, not a credential failure.
            let err_frame = if err.status() == axum::http::StatusCode::SERVICE_UNAVAILABLE {
                build_ws_error_frame(
                    503,
                    "api_error",
                    "api_error",
                    "cannot verify the presented credential: a configured JWT issuer's key set is unreachable",
                    None,
                )
            } else {
                build_ws_error_frame(
                    401,
                    "authentication_error",
                    "authentication_error",
                    "missing or invalid client token",
                    None,
                )
            };
            let _ = out_tx
                .send((turn_gen, Message::Text(err_frame.into())))
                .await;
            return;
        }
    };
    let pool_key = pool_sticky_key(inbound_client.as_deref(), session_id.clone());
    let max_request_bytes = state.config.server.limits.max_request_bytes;
    if body.len() > max_request_bytes {
        let err_frame = build_ws_error_frame(
            413,
            "invalid_request_error",
            "request_too_large",
            "request body exceeds the configured limit",
            None,
        );
        let _ = out_tx
            .send((turn_gen, Message::Text(err_frame.into())))
            .await;
        return;
    }
    let mut headers = headers;
    if let Some(header) = auth_header {
        headers.remove(header);
    }
    if let Some(auth) = state.inbound_auth.as_ref() {
        headers.remove(auth.header());
    }
    if let Some(admin_header) = admin_header {
        headers.remove(admin_header);
    }
    if let Some(admin_auth) = state.admin_auth.as_ref() {
        headers.remove(admin_auth.header());
    }
    let started_at = Instant::now();
    let dispatch_res = forward_turn(
        state, model, pool_key, session_id, headers, body, started_at,
    )
    .await;

    if !is_current() {
        return;
    }

    let (status, response) = match dispatch_res {
        Ok(ok) => ok,
        Err(err) => {
            let status_code = err.response.status();
            let err_frame = build_ws_error_frame(
                status_code.as_u16(),
                "api_error",
                "upstream_error",
                &err.message,
                None,
            );
            let _ = out_tx
                .send((turn_gen, Message::Text(err_frame.into())))
                .await;
            return;
        }
    };

    if !is_current() {
        return;
    }

    if !status.is_success() {
        let resp_headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap_or_default();
        let err_json = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let (err_type, code, message) =
            err_json
                .as_ref()
                .map(upstream_error_parts)
                .unwrap_or_else(|| {
                    (
                        "api_error".to_string(),
                        "upstream_error".to_string(),
                        "Upstream request failed".to_string(),
                    )
                });
        let err_frame = build_ws_error_frame(
            status.as_u16(),
            &err_type,
            &code,
            &message,
            Some(&resp_headers),
        );
        let _ = out_tx
            .send((turn_gen, Message::Text(err_frame.into())))
            .await;
        return;
    }

    let mut body_stream = response.into_body().into_data_stream();
    let mut framer = BoundedSseFrameBuffer::new(MAX_CLIENT_SSE_FRAME_BYTES);
    let mut terminal_seen = false;

    while let Some(chunk_res) = body_stream.next().await {
        if !is_current() {
            return;
        }
        let chunk = match chunk_res {
            Ok(c) => c,
            Err(err) => {
                let err_frame = build_ws_error_frame(
                    502,
                    "protocol_error",
                    "websocket_protocol_error",
                    &format!("Error reading upstream stream: {err}"),
                    None,
                );
                let _ = out_tx
                    .send((turn_gen, Message::Text(err_frame.into())))
                    .await;
                return;
            }
        };

        let frames = match framer.feed(&chunk) {
            Ok(f) => f,
            Err(err) => {
                let err_frame = build_ws_error_frame(
                    502,
                    "protocol_error",
                    "websocket_protocol_error",
                    &err.to_string(),
                    None,
                );
                let _ = out_tx
                    .send((turn_gen, Message::Text(err_frame.into())))
                    .await;
                return;
            }
        };

        for frame_bytes in frames {
            if !is_current() {
                return;
            }
            let s = match std::str::from_utf8(&frame_bytes) {
                Ok(s) => s,
                Err(_) => {
                    let err_frame = build_ws_error_frame(
                        502,
                        "protocol_error",
                        "websocket_protocol_error",
                        "Invalid UTF-8 in upstream SSE frame",
                        None,
                    );
                    let _ = out_tx
                        .send((turn_gen, Message::Text(err_frame.into())))
                        .await;
                    terminal_seen = true;
                    break;
                }
            };
            let payload = match parse_sse_block(s) {
                Some(p) => p,
                None => continue,
            };
            if payload == "[DONE]" {
                continue;
            }
            let p_type = match parse_payload_type(&payload) {
                Some(t) => t,
                None => {
                    let err_frame = build_ws_error_frame(
                        502,
                        "protocol_error",
                        "websocket_protocol_error",
                        "Invalid JSON payload in upstream SSE frame",
                        None,
                    );
                    let _ = out_tx
                        .send((turn_gen, Message::Text(err_frame.into())))
                        .await;
                    terminal_seen = true;
                    break;
                }
            };

            let terminal = terminal_status_from_type(&p_type);
            if let Some(terminal) = terminal {
                record_terminal(
                    &continuation,
                    &logical_input,
                    &body_signature,
                    &payload,
                    terminal,
                );
            }
            let _ = out_tx.send((turn_gen, Message::Text(payload.into()))).await;

            if terminal.is_some() {
                terminal_seen = true;
                break;
            }
        }

        if terminal_seen {
            break;
        }
    }

    if !terminal_seen && is_current() {
        match framer.finish() {
            Ok(Some(tail)) => {
                let s = match std::str::from_utf8(&tail) {
                    Ok(s) => s,
                    Err(_) => {
                        let err_frame = build_ws_error_frame(
                            502,
                            "protocol_error",
                            "websocket_protocol_error",
                            "Invalid UTF-8 in upstream SSE frame",
                            None,
                        );
                        let _ = out_tx
                            .send((turn_gen, Message::Text(err_frame.into())))
                            .await;
                        return;
                    }
                };
                if let Some(payload) = parse_sse_block(s) {
                    if payload != "[DONE]" {
                        if let Some(p_type) = parse_payload_type(&payload) {
                            if let Some(terminal) = terminal_status_from_type(&p_type) {
                                record_terminal(
                                    &continuation,
                                    &logical_input,
                                    &body_signature,
                                    &payload,
                                    terminal,
                                );
                                terminal_seen = true;
                            }
                            let _ = out_tx.send((turn_gen, Message::Text(payload.into()))).await;
                        } else {
                            let err_frame = build_ws_error_frame(
                                502,
                                "protocol_error",
                                "websocket_protocol_error",
                                "Invalid JSON payload in upstream SSE frame",
                                None,
                            );
                            let _ = out_tx
                                .send((turn_gen, Message::Text(err_frame.into())))
                                .await;
                            return;
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                let err_frame = build_ws_error_frame(
                    502,
                    "protocol_error",
                    "websocket_protocol_error",
                    &err.to_string(),
                    None,
                );
                let _ = out_tx
                    .send((turn_gen, Message::Text(err_frame.into())))
                    .await;
                return;
            }
        }
    }

    if !terminal_seen && is_current() {
        let err_frame = build_ws_error_frame(
            502,
            "protocol_error",
            "websocket_protocol_error",
            "Upstream stream ended before response terminal event",
            None,
        );
        let _ = out_tx
            .send((turn_gen, Message::Text(err_frame.into())))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::upstream_error_parts;
    use serde_json::json;

    #[test]
    fn upstream_detail_shape_is_surfaced_not_masked() {
        // The ChatGPT HTTP backend's actual rejection for a forwarded
        // `previous_response_id`; masking it hid the cause for a day.
        let body = json!({"detail": "Unsupported parameter: previous_response_id"});
        let (_, _, message) = upstream_error_parts(&body);
        assert_eq!(message, "Unsupported parameter: previous_response_id");
    }

    #[test]
    fn openai_error_shape_still_reads_nested_fields() {
        let body =
            json!({"error": {"type": "invalid_request_error", "code": "bad", "message": "nope"}});
        assert_eq!(
            upstream_error_parts(&body),
            (
                "invalid_request_error".to_string(),
                "bad".to_string(),
                "nope".to_string()
            )
        );
    }
}
