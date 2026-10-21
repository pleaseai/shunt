//! Per-turn forwarding for inbound Responses WebSocket sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::ws::Message,
    http::{HeaderMap, HeaderName},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    codex_endpoint::{forward_turn, inbound_attribution, pool_sticky_key},
    server::AppState,
};

use super::continuation::TurnRecorder;
use crate::codex_endpoint::frame::{
    build_ws_error_frame, parse_payload_type, parse_sse_block, terminal_status_from_type,
    BoundedSseFrameBuffer, MAX_CLIENT_SSE_FRAME_BYTES,
};

pub(super) struct TurnContext {
    /// The runtime snapshot this turn routes with, refreshed by the caller
    /// when the frame arrived.
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
    /// Records the upstream response against the connection continuation so
    /// the next incremental frame can extend it.
    pub(super) recorder: TurnRecorder,
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
        mut recorder,
    } = context;
    let is_current = || current_generation.load(Ordering::Relaxed) == turn_gen;

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
    // Classified before the credential headers are stripped below: a
    // `[server.auth]` or `[server.admin]` header may share a name with a client
    // marker (`originator`, `x-app`), and removing it first would erase that
    // marker. A WebSocket turn has no request span, so this records nothing
    // there; it still tags the turn's metric samples
    // (`docs/otlp-client-attribution.md`).
    let attribution = inbound_attribution(&headers);
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
        state,
        model,
        pool_key,
        session_id,
        headers,
        attribution,
        body,
        started_at,
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

            recorder.observe(&p_type, &payload);
            let terminal = terminal_status_from_type(&p_type);
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
                            recorder.observe(&p_type, &payload);
                            if terminal_status_from_type(&p_type).is_some() {
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
mod tests;
