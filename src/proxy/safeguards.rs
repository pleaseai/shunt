//! Synthesize Claude Code auto-mode `safeguard_results` on the inbound Messages
//! surface.
//!
//! Claude Code's auto mode asks the API to run its permission classifier
//! server-side on the session's own `/v1/messages` turns: the request carries
//! the `dangerous-tool-use-2026-09-03` beta token and a top-level `safeguards`
//! array, and a first-party response answers with a matching
//! `safeguard_results` array — top level when non-streaming, inside the
//! `message_delta` event when streaming.
//!
//! Only `api.anthropic.com` implements it. A route served by a translation
//! adapter (Responses/Codex, Cursor, Gemini, Antigravity) returns a
//! Messages-shaped response that carries no `safeguard_results` at all, and the
//! client reads that absence as "the gateway dropped it" — it then falls back to
//! its own billed classifier for the *rest of the session*. A per-tool-use
//! `unavailable` entry costs far less: the client classifies that one action
//! locally and keeps asking the server on the next turn.
//!
//! So this pass answers with the shape that keeps the session eligible — a
//! status-level `available` whose every observed `tool_use` id is
//! `{"type":"unavailable","reason":"error"}` — rather than a status-level
//! `disabled`, which would retire the server classifier for the session just as
//! the missing array does.
//!
//! The pass is gated on the inbound request carrying `safeguards`: without it
//! nothing is wrapped and the response keeps byte-for-byte passthrough. A
//! response that already carries `safeguard_results` (the first-party relay) is
//! forwarded verbatim — this never rewrites an upstream verdict, it only fills
//! a hole.
//!
//! See `docs/notes/auto-mode-server-classifier.md` for the measured wire
//! protocol.

use axum::{
    body::{Body, Bytes},
    http::header::{CONTENT_LENGTH, CONTENT_TYPE},
    response::Response,
};
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Map, Value};

/// Largest SSE frame this pass parses. A real `message_delta` is a few hundred
/// bytes; anything past this bound is forwarded unmodified rather than
/// buffered, mirroring the model-rewrite accumulator's cap.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// The `type` strings of the request's top-level `safeguards` array, empty when
/// the client did not ask for server-side classification (the zero-cost path).
pub(crate) fn requested_types(request: &Value) -> Vec<String> {
    request
        .get("safeguards")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("type").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Fill in `safeguard_results` on a 2xx Messages response when the upstream did
/// not answer the request's `safeguards`. Anything else — no requested types, a
/// non-2xx status, a content type that is neither SSE nor JSON — is returned
/// untouched.
pub(crate) async fn synthesize(
    response: Response,
    types: &[String],
    max_body_bytes: usize,
) -> Response {
    if types.is_empty() || !response.status().is_success() {
        return response;
    }
    match essence(&response).as_deref() {
        Some("text/event-stream") => wrap_stream(response, types),
        Some("application/json") => {
            inject_json(response, types, max_body_bytes.max(MIN_SYNTHESIS_BYTES)).await
        }
        _ => response,
    }
}

/// Floor for the non-streaming synthesis buffer.
///
/// `max_body_bytes` reaches this module as `server.limits.max_request_bytes`,
/// which bounds what a *client* may upload. An operator who lowers it to
/// constrain uploads is not asking to stop answering `safeguards` on the
/// response path — but that is what it did: a relayed response carrying no
/// `safeguard_results` makes Claude Code retire the server classifier for the
/// rest of the session, so an unrelated config choice silently disabled the
/// feature this module exists to provide. Flooring the response budget
/// decouples the two; a limit raised past this is still honoured (#622 review).
const MIN_SYNTHESIS_BYTES: usize = 1024 * 1024;

/// The response's content type with parameters and case folded away.
fn essence(response: &Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
}

fn wrap_stream(response: Response, types: &[String]) -> Response {
    let (parts, body) = response.into_parts();
    let stream = transform_stream(body.into_data_stream(), types.to_vec());
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Frame-level state carried across the relayed SSE stream.
struct FrameState {
    /// Bytes since the last complete frame boundary. Never holds more than one
    /// incomplete frame: every complete frame is emitted as soon as its
    /// boundary arrives.
    carry: Vec<u8>,
    /// `content_block.id` of every `tool_use` block seen so far, in order.
    tool_ids: Vec<String>,
    /// Set while the frame being accumulated has passed [`MAX_FRAME_BYTES`] and
    /// its head was already flushed: its tail is not a frame of its own and
    /// must not be parsed.
    oversized: bool,
}

impl FrameState {
    fn new() -> Self {
        Self {
            carry: Vec::new(),
            tool_ids: Vec::new(),
            oversized: false,
        }
    }

    /// Rewrite the complete frames in `buffer`, forwarding every byte that is
    /// not a `message_delta` needing synthesis exactly as it arrived.
    fn transform(&mut self, buffer: &[u8], types: &[String]) -> Bytes {
        let mut out: Vec<u8> = Vec::with_capacity(buffer.len());
        let mut pos = 0;
        while pos < buffer.len() {
            let end = first_event_boundary(&buffer[pos..])
                .map(|offset| pos + offset)
                .unwrap_or(buffer.len());
            let frame = &buffer[pos..end];
            pos = end;
            if std::mem::take(&mut self.oversized) || frame.len() > MAX_FRAME_BYTES {
                out.extend_from_slice(frame);
                continue;
            }
            match std::str::from_utf8(frame)
                .ok()
                .and_then(|frame| rewrite_frame(frame, &mut self.tool_ids, types))
            {
                Some(rewritten) => {
                    tracing::debug!(
                        tool_uses = self.tool_ids.len(),
                        "synthesized safeguard_results on a relayed message_delta"
                    );
                    out.extend_from_slice(rewritten.as_bytes());
                }
                None => out.extend_from_slice(frame),
            }
        }
        Bytes::from(out)
    }
}

/// Wrap a relayed Anthropic SSE stream so its `message_delta` carries
/// `safeguard_results`. Only that one frame is ever re-serialized; every other
/// frame, ping and comment line is forwarded byte-for-byte, and nothing is
/// buffered past a frame boundary.
fn transform_stream<S, E>(
    upstream: S,
    types: Vec<String>,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let state = (Box::pin(upstream), FrameState::new(), types, false);
    stream::unfold(state, |(mut upstream, mut state, types, done)| {
        Box::pin(async move {
            if done {
                return None;
            }
            loop {
                if let Some(end) = complete_frames_end(&state.carry) {
                    let complete: Vec<u8> = state.carry.drain(..end).collect();
                    let out = state.transform(&complete, &types);
                    return Some((Ok(out), (upstream, state, types, false)));
                }
                if state.carry.len() > MAX_FRAME_BYTES {
                    // The frame in flight is larger than anything worth
                    // parsing: release what is buffered and mark its tail so
                    // the next boundary does not present it as a frame.
                    let out = Bytes::from(std::mem::take(&mut state.carry));
                    state.oversized = true;
                    return Some((Ok(out), (upstream, state, types, false)));
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => state.carry.extend_from_slice(&chunk),
                    Some(Err(error)) => return Some((Err(error), (upstream, state, types, true))),
                    None => {
                        // Upstream ended mid-frame: forward the partial bytes
                        // rather than drop them.
                        return if state.carry.is_empty() {
                            None
                        } else {
                            let out = Bytes::from(std::mem::take(&mut state.carry));
                            Some((Ok(out), (upstream, state, types, true)))
                        };
                    }
                }
            }
        })
    })
}

/// Byte index just past the first SSE event boundary (`\n\n` or `\r\n\r\n`),
/// whichever appears first, or `None` when the buffer holds no complete frame.
fn first_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Byte index just past the last complete frame in the buffer.
fn complete_frames_end(buf: &[u8]) -> Option<usize> {
    let mut end = None;
    let mut pos = 0;
    while let Some(offset) = first_event_boundary(&buf[pos..]) {
        pos += offset;
        end = Some(pos);
    }
    end
}

/// Record any `tool_use` id this frame starts, and inject `safeguard_results`
/// into a `message_delta` that lacks it. `Some` only when the frame was
/// actually rewritten; every other frame is forwarded by the caller untouched.
/// True for a line that carries part of the event's `data:` payload.
fn is_data_line(segment: &str) -> bool {
    segment.trim_end_matches(['\n', '\r']).starts_with("data:")
}

/// Join the event's `data:` lines into one payload, then rewrite it if it is
/// the turn's `message_delta`.
///
/// The SSE spec lets one event spread its payload over several `data:` lines,
/// joined with newlines. Parsing them one at a time leaves a split
/// `content_block_start` or `message_delta` unrecognized — on exactly the
/// non-first-party upstreams this module exists for — so the turn would relay
/// without `safeguard_results` and the client would retire the classifier for
/// the rest of the session (#622 review). The rewritten event folds those
/// lines into the single `data:` line the re-serialized JSON now occupies.
fn rewrite_frame(frame: &str, tool_ids: &mut Vec<String>, types: &[String]) -> Option<String> {
    let segments: Vec<&str> = frame.split_inclusive('\n').collect();
    let mut payload = String::new();
    let mut first_data = None;
    let mut ending = "";
    for (index, segment) in segments.iter().enumerate() {
        let content_len = segment.trim_end_matches(['\n', '\r']).len();
        let (content, line_ending) = segment.split_at(content_len);
        let Some(chunk) = content.strip_prefix("data:") else {
            continue;
        };
        if first_data.is_none() {
            first_data = Some(index);
        } else {
            payload.push('\n');
        }
        // The spec strips one optional space after the colon, not every run of
        // whitespace: a payload line's own leading spaces are part of it.
        payload.push_str(chunk.strip_prefix(' ').unwrap_or(chunk));
        ending = line_ending;
    }
    let first_data = first_data?;
    let mut value = serde_json::from_str::<Value>(&payload).ok()?;
    match value.get("type").and_then(Value::as_str)? {
        "content_block_start" => {
            record_tool_use(&value, tool_ids);
            None
        }
        // A first-party relay answers the request itself; its verdict is
        // forwarded verbatim, never re-serialized.
        "message_delta" if value.pointer("/delta/safeguard_results").is_none() => {
            let delta = value.get_mut("delta").and_then(Value::as_object_mut)?;
            delta.insert(
                "safeguard_results".to_string(),
                results_value(types, tool_ids),
            );
            let reserialized = serde_json::to_string(&value).ok()?;
            let mut out = String::with_capacity(frame.len() + reserialized.len());
            for (index, segment) in segments.iter().enumerate() {
                if index == first_data {
                    out.push_str("data: ");
                    out.push_str(&reserialized);
                    out.push_str(ending);
                } else if !is_data_line(segment) {
                    out.push_str(segment);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn record_tool_use(value: &Value, tool_ids: &mut Vec<String>) {
    if value.pointer("/content_block/type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let Some(id) = value.pointer("/content_block/id").and_then(Value::as_str) else {
        return;
    };
    if !tool_ids.iter().any(|seen| seen == id) {
        tool_ids.push(id.to_string());
    }
}

/// Buffer a JSON body and insert `safeguard_results`.
///
/// Reached only for an `application/json` response, never for `text/event-stream`
/// — so this never buffers an upstream SSE relay, whatever the request's
/// `stream` flag said. A JSON reply is not a stream: it has no frames to
/// forward incrementally, and the field cannot be inserted without the whole
/// body. A body past `max_body_bytes` is forwarded unmodified instead.
async fn inject_json(response: Response, types: &[String], max_body_bytes: usize) -> Response {
    let (mut parts, body) = response.into_parts();
    let mut data = body.into_data_stream();
    let mut collected: Vec<Bytes> = Vec::new();
    let mut total = 0usize;
    loop {
        match data.next().await {
            Some(Ok(chunk)) => {
                total = total.saturating_add(chunk.len());
                collected.push(chunk);
                if total > max_body_bytes {
                    // Forwarded unmodified, which the client reads as "this
                    // gateway does not answer safeguards" and acts on for the
                    // whole session — so say so once rather than degrading
                    // silently. `max_body_bytes` is the inbound request limit;
                    // an operator who lowered it to constrain uploads has no
                    // other signal that it also governs this path (#622 review).
                    tracing::warn!(
                        bytes = total,
                        max_body_bytes,
                        "relayed a message response past the synthesis budget, so \
                         safeguard_results was not added; the client will stop \
                         asking for the rest of the session"
                    );
                    let head = stream::iter(collected.into_iter().map(Ok::<_, axum::Error>));
                    return Response::from_parts(parts, Body::from_stream(head.chain(data)));
                }
            }
            Some(Err(error)) => {
                let head = stream::iter(collected.into_iter().map(Ok::<_, axum::Error>));
                let tail = stream::once(std::future::ready(Err(error)));
                return Response::from_parts(parts, Body::from_stream(head.chain(tail)));
            }
            None => break,
        }
    }
    let raw = collected.concat();
    let Some(injected) = inject_message_body(&raw, types) else {
        return Response::from_parts(parts, Body::from(raw));
    };
    tracing::debug!(
        bytes = injected.len(),
        "synthesized safeguard_results on a relayed non-streaming message"
    );
    // The body grew; the relayed length no longer describes it.
    parts.headers.remove(CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(injected))
}

/// Insert `safeguard_results` into a non-streaming Messages body. `None` — the
/// caller forwards the original bytes — for a non-JSON body, a body that is not
/// a `message`, or one that already answered.
fn inject_message_body(raw: &[u8], types: &[String]) -> Option<Bytes> {
    let mut value = serde_json::from_slice::<Value>(raw).ok()?;
    let object = value.as_object_mut()?;
    if object.get("type").and_then(Value::as_str) != Some("message")
        || object.contains_key("safeguard_results")
    {
        return None;
    }
    let tool_ids: Vec<String> = object
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|block| block.get("id").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    object.insert(
        "safeguard_results".to_string(),
        results_value(types, &tool_ids),
    );
    serde_json::to_vec(&value).ok().map(Bytes::from)
}

/// One `safeguard_results` entry per requested type, each reporting every
/// observed tool use as "could not evaluate". `tool_uses` is `{}` when the turn
/// produced no tool use — the shape a first-party response uses there too.
fn results_value(types: &[String], tool_ids: &[String]) -> Value {
    let tool_uses: Map<String, Value> = tool_ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                json!({"type": "unavailable", "reason": "error"}),
            )
        })
        .collect();
    Value::Array(
        types
            .iter()
            .map(|kind| {
                json!({
                    "type": kind,
                    "status": {"type": "available", "tool_uses": Value::Object(tool_uses.clone())},
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
