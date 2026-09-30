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
    codex_endpoint::{forward_turn, pool_sticky_key},
    server::AppState,
};

use crate::codex_endpoint::frame::{
    build_ws_error_frame, parse_payload_type, parse_sse_block, terminal_status_from_type,
    BoundedSseFrameBuffer, MAX_CLIENT_SSE_FRAME_BYTES,
};

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
    let Ok(inbound_client) = crate::codex_endpoint::authenticate_inbound(
        state.inbound_auth.as_deref(),
        &headers,
        &codex_endpoint.provider,
    ) else {
        let err_frame = build_ws_error_frame(
            401,
            "authentication_error",
            "authentication_error",
            "missing or invalid client token",
            None,
        );
        let _ = out_tx
            .send((turn_gen, Message::Text(err_frame.into())))
            .await;
        return;
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
        let (err_type, code, message) = if let Some(val) = &err_json {
            let detail = val.get("error").unwrap_or(val);
            let t = detail
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("api_error");
            let c = detail
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("upstream_error");
            let m = detail
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Upstream request failed");
            (t, c, m)
        } else {
            ("api_error", "upstream_error", "Upstream request failed")
        };
        let err_frame = build_ws_error_frame(
            status.as_u16(),
            err_type,
            code,
            message,
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

            let _ = out_tx.send((turn_gen, Message::Text(payload.into()))).await;

            if terminal_status_from_type(&p_type).is_some() {
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
                            let _ = out_tx.send((turn_gen, Message::Text(payload.into()))).await;
                            if terminal_status_from_type(&p_type).is_some() {
                                terminal_seen = true;
                            }
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
