//! Inbound Responses WebSocket transport handler and session loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade},
        Extension, State,
    },
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::{
    codex_endpoint::{authenticate_inbound, extract_session_id, forward_turn, pool_sticky_key},
    concurrency::WebSocketPermit,
    error::ShuntError,
    server::AppState,
};

use super::frame::{
    build_warmup_completion_frames, build_ws_error_frame, parse_client_frame, parse_payload_type,
    parse_sse_block, terminal_status_from_type, BoundedSseFrameBuffer, ClientFrame,
    MAX_CLIENT_SSE_FRAME_BYTES,
};

struct TurnContext {
    state: AppState,
    model: Option<String>,
    pool_key: Option<String>,
    headers: HeaderMap,
    auth_header: Option<HeaderName>,
    body: Bytes,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    out_tx: mpsc::Sender<(u64, Message)>,
}

/// GET handler for inbound Responses WebSocket upgrades.
///
/// Enforces client token authentication before returning HTTP 101 Switching Protocols.
pub async fn get(
    State(state): State<AppState>,
    ws_permit: Option<Extension<WebSocketPermit>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(codex_endpoint) = &state.config.server.codex_endpoint else {
        return ShuntError::bad_gateway("codex endpoint is not configured".to_string())
            .into_response();
    };
    let provider = codex_endpoint.provider.clone();

    if state.inbound_auth.is_none() && !same_origin_or_non_browser(&headers) {
        return crate::error::into_openai_error_shape(
            ShuntError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "WebSocket upgrades from a different browser origin require inbound authentication",
            )
            .into_response(),
        )
        .await;
    }

    let inbound_client =
        match authenticate_inbound(state.inbound_auth.as_deref(), &headers, &provider) {
            Ok(client) => client,
            Err(err) => {
                return crate::error::into_openai_error_shape(err.into_response()).await;
            }
        };

    let mut ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };

    ws = ws.max_message_size(4 * 1024 * 1024);

    let session_id = extract_session_id(&headers);
    let pool_key = pool_sticky_key(inbound_client.as_deref(), session_id);
    let auth_header = state
        .inbound_auth
        .as_ref()
        .map(|auth| auth.header().clone());

    ws.on_upgrade(move |socket| async move {
        let _ws_permit = ws_permit
            .and_then(|Extension(permit)| permit.lock().ok().and_then(|mut permit| permit.take()));
        handle_socket(socket, state, pool_key, headers, auth_header).await;
    })
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    pool_key: Option<String>,
    handshake_headers: HeaderMap,
    auth_header: Option<HeaderName>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<(u64, Message)>(1);

    let current_generation = Arc::new(AtomicU64::new(0));
    let mut active_turn_task: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            Some((msg_gen, msg)) = out_rx.recv() => {
                if msg_gen == current_generation.load(Ordering::Relaxed) {
                    if let Err(err) = ws_tx.send(msg).await {
                        tracing::debug!(error = %err, "client websocket connection closed during send");
                        break;
                    }
                }
            }

            inbound = ws_rx.next() => {
                let msg = match inbound {
                    Some(Ok(msg)) => msg,
                    Some(Err(err)) => {
                        tracing::debug!(error = %err, "client websocket frame read error");
                        break;
                    }
                    None => break,
                };

                let client_frame = parse_client_frame(&msg);
                match client_frame {
                    ClientFrame::ResponseProcessed => {
                        tracing::trace!("received response.processed ack");
                    }
                    ClientFrame::IgnoredText => {
                        tracing::debug!("ignoring unparseable or unknown text frame");
                    }
                    ClientFrame::BinaryUnsupported => {
                        let err_frame = build_ws_error_frame(
                            400,
                            "invalid_request_error",
                            "unsupported_frame_type",
                            "Binary frames are not supported",
                            None,
                        );
                        let _ = ws_tx.send(Message::Text(err_frame.into())).await;
                        break;
                    }
                    ClientFrame::Close => break,
                    ClientFrame::ResponseCreate { generate, model, mut raw_json } => {
                        if let Some(prior) = active_turn_task.take() {
                            prior.abort();
                        }
                        let turn_gen = current_generation.fetch_add(1, Ordering::Relaxed) + 1;

                        if !generate {
                            let frames = build_warmup_completion_frames(model.as_deref());
                            let mut send_failed = false;
                            for f in frames {
                                if turn_gen == current_generation.load(Ordering::Relaxed) {
                                    if let Err(err) = ws_tx.send(Message::Text(f.into())).await {
                                        tracing::debug!(error = %err, "failed to send warmup frame");
                                        send_failed = true;
                                        break;
                                    }
                                }
                            }
                            if send_failed {
                                break;
                            }
                            continue;
                        }

                        if let Some(obj) = raw_json.as_object_mut() {
                            obj.remove("type");
                            // `generate` is a WebSocket-bridge control flag, not a
                            // Responses HTTP request field. The local warmup path
                            // consumes `false` above; an explicit `true` must be
                            // consumed here as well or the ChatGPT HTTP backend
                            // rejects the otherwise-valid live turn.
                            obj.remove("generate");
                            // A WebSocket turn is delivered as Responses events even when the
                            // client omitted or contradicted the HTTP transport flag.
                            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
                        }
                        let body_bytes = Bytes::from(serde_json::to_vec(&raw_json).unwrap_or_default());

                        let mut turn_headers = handshake_headers.clone();
                        turn_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                        turn_headers.remove(header::CONTENT_LENGTH);
                        turn_headers.remove(header::CONTENT_ENCODING);
                        turn_headers.remove(header::UPGRADE);
                        turn_headers.remove(header::CONNECTION);
                        turn_headers.remove("sec-websocket-key");
                        turn_headers.remove("sec-websocket-version");
                        turn_headers.remove("sec-websocket-extensions");
                        turn_headers.remove("sec-websocket-protocol");
                        if turn_headers.contains_key("openai-beta") {
                            turn_headers.insert(
                                "openai-beta",
                                "responses=experimental".parse().unwrap(),
                            );
                        }

                        let state_clone = state.clone();
                        let pool_key_clone = pool_key.clone();
                        let auth_header_clone = auth_header.clone();
                        let out_tx_clone = out_tx.clone();
                        let gen_clone = current_generation.clone();

                        active_turn_task = Some(tokio::spawn(async move {
                            run_turn(TurnContext {
                                state: state_clone,
                                model,
                                pool_key: pool_key_clone,
                                headers: turn_headers,
                                auth_header: auth_header_clone,
                                body: body_bytes,
                                generation: turn_gen,
                                current_generation: gen_clone,
                                out_tx: out_tx_clone,
                            }).await;
                        }));
                    }
                }
            }
        }
    }

    if let Some(prior) = active_turn_task {
        prior.abort();
    }
}

async fn run_turn(context: TurnContext) {
    let TurnContext {
        state,
        model,
        pool_key,
        headers,
        auth_header,
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
    if crate::codex_endpoint::authenticate_inbound(
        state.inbound_auth.as_deref(),
        &headers,
        &codex_endpoint.provider,
    )
    .is_err()
    {
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
    }
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
    let started_at = Instant::now();
    let dispatch_res = forward_turn(state, model, pool_key, headers, body, started_at).await;

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

fn same_origin_or_non_browser(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let Some(origin_host) = origin.host_str() else {
        return false;
    };
    let origin_port = origin.port_or_known_default();
    let host_without_port = host.rsplit_once(':').map_or(host, |(host, port)| {
        if port.parse::<u16>().is_ok() {
            host.trim_matches(['[', ']'])
        } else {
            host
        }
    });
    let host_port = host
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok());
    origin_host.eq_ignore_ascii_case(host_without_port)
        && origin_port
            == host_port.or_else(|| match origin.scheme() {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            })
}
