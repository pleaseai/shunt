//! Inbound Responses WebSocket transport handler and session loop.

mod turn;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    codex_endpoint::{authenticate_inbound, extract_session_id},
    concurrency::WebSocketPermit,
    error::ShuntError,
    server::AppState,
};

use super::frame::{
    build_warmup_completion_frames, build_ws_error_frame, parse_client_frame, ClientFrame,
};
use turn::{run_turn, TurnContext};

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
        return crate::error::into_openai_error_shape(
            ShuntError::bad_gateway("codex endpoint is not configured".to_string()).into_response(),
        )
        .await;
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

    if let Err(err) = authenticate_inbound(state.inbound_auth.as_deref(), &headers, &provider) {
        return crate::error::into_openai_error_shape(err.into_response()).await;
    }

    let mut ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => {
            // Axum's rejection body is plain text; carry its status and text
            // into the OpenAI Responses envelope like every other pre-upgrade
            // failure on this endpoint.
            return crate::error::into_openai_error_shape(
                ShuntError::new(
                    rejection.status(),
                    "invalid_request_error",
                    rejection.body_text(),
                )
                .into_response(),
            )
            .await;
        }
    };

    ws = ws.max_message_size(4 * 1024 * 1024);

    let session_id = extract_session_id(&headers);
    let auth_header = state
        .inbound_auth
        .as_ref()
        .map(|auth| auth.header().clone());
    let admin_header = state.admin_auth.as_ref().map(|auth| auth.header().clone());

    ws.on_upgrade(move |socket| async move {
        let _ws_permit = ws_permit
            .and_then(|Extension(permit)| permit.lock().ok().and_then(|mut permit| permit.take()));
        handle_socket(
            socket,
            state,
            session_id,
            headers,
            auth_header,
            admin_header,
        )
        .await;
    })
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    session_id: Option<String>,
    handshake_headers: HeaderMap,
    auth_header: Option<HeaderName>,
    admin_header: Option<HeaderName>,
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
                        http_openai_beta(&mut turn_headers);

                        let state_clone = state.clone();
                        let session_id_clone = session_id.clone();
                        let auth_header_clone = auth_header.clone();
                        let admin_header_clone = admin_header.clone();
                        let out_tx_clone = out_tx.clone();
                        let gen_clone = current_generation.clone();

                        active_turn_task = Some(tokio::spawn(async move {
                            run_turn(TurnContext {
                                state: state_clone,
                                model,
                                session_id: session_id_clone,
                                headers: turn_headers,
                                auth_header: auth_header_clone,
                                admin_header: admin_header_clone,
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

/// Turn the handshake's `OpenAI-Beta` into the value an HTTP Responses turn
/// carries: drop the WebSocket-only `responses_websockets=…` selector, keep every
/// other beta the client asked for, and make sure the HTTP `responses=` selector
/// is present. Repeated header fields are merged, since a plain `get` would read
/// only the first. A handshake without `OpenAI-Beta` is left without one.
fn http_openai_beta(headers: &mut HeaderMap) {
    let mut betas: Vec<String> = headers
        .get_all("openai-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|beta| !beta.is_empty() && !beta.starts_with("responses_websockets"))
        .map(ToOwned::to_owned)
        .collect();
    if !headers.contains_key("openai-beta") {
        return;
    }
    if !betas.iter().any(|beta| beta.starts_with("responses=")) {
        betas.push("responses=experimental".to_string());
    }
    match betas.join(", ").parse() {
        Ok(value) => {
            headers.insert("openai-beta", value);
        }
        Err(_) => {
            headers.remove("openai-beta");
        }
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
    // Split a trailing `:port` only when it parses; a port-less IPv6 literal
    // (`[::1]`) keeps its whole text as the host name.
    let (host_without_port, host_port) = host
        .rsplit_once(':')
        .and_then(|(name, port)| Some((name, Some(port.parse::<u16>().ok()?))))
        .unwrap_or((host, None));
    // `Url::host_str` keeps an IPv6 literal's brackets and so does the Host
    // header; compare both unbracketed so either spelling matches.
    let unbracket = |host: &str| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned()
    };
    unbracket(origin_host).eq_ignore_ascii_case(&unbracket(host_without_port))
        && origin_port
            == host_port.or_else(|| match origin.scheme() {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            })
}

#[cfg(test)]
mod tests;
