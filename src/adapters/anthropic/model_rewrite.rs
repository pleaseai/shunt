//! Restore the client-facing model alias on relayed Anthropic responses.
//!
//! A discovery-alias route rewrites the outbound request `model` to
//! `upstream_model` (see [`super::normalize_upstream_model`]). The upstream —
//! which may be several hops away — then reports its *own* model id back: in the
//! `message_start` SSE event (streaming) or the top-level `model` field
//! (non-streaming). Left unchanged, that raw id reaches Claude Code, which
//! records it in the session transcript and cannot restore the model on
//! `--resume`, emitting a "could not be restored" warning (issue #172).
//!
//! These helpers rewrite that field back to the route alias. The rewrite is
//! keyed on the alias only — never on `upstream_model` — because in a multi-hop
//! chain the reported model equals neither the alias nor `upstream_model`. The
//! Responses adapter already preserves the alias by seeding its SSE machine with
//! `route.model`; this brings the native relay path to parity.

use axum::body::Bytes;
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;

/// Rewrite a non-streaming Messages response body's top-level `model` to
/// `alias` when it is present and differs. A non-JSON body, a body without a
/// `model` field (e.g. `count_tokens`), or one already equal to `alias` is
/// returned unchanged.
pub(super) fn rewrite_response_model(body: Vec<u8>, alias: &str) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    match object.get("model").and_then(Value::as_str) {
        Some(model) if model != alias => {
            object.insert("model".to_string(), Value::String(alias.to_string()));
            serde_json::to_vec(&value).unwrap_or(body)
        }
        _ => body,
    }
}

/// Rewrite the first `message_start` frame of an Anthropic SSE stream so its
/// `message.model` reports `alias`, forwarding every subsequent byte unchanged.
///
/// `alias` of `None` (route alias == `upstream_model`, nothing to restore)
/// forwards the whole stream untouched. Otherwise the first event is
/// accumulated across arbitrary chunk boundaries until an SSE frame boundary
/// (`\n\n` / `\r\n\r\n`), then emitted — rewritten if it was `message_start`,
/// verbatim otherwise — together with any bytes past that boundary. After the
/// first frame the stream is a straight passthrough, so only one small frame is
/// ever buffered.
pub(super) fn rewrite_first_model_stream<S, E>(
    upstream: S,
    alias: Option<String>,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    // `Some(buf)` while scanning for the first frame; `None` once the frame has
    // been emitted (or was never needed) and the stream is a plain passthrough.
    let pending: Option<Vec<u8>> = alias.as_ref().map(|_| Vec::new());
    let state = (Box::pin(upstream), pending, alias);
    stream::unfold(state, move |(mut upstream, pending, alias)| {
        Box::pin(async move {
            let Some(mut buf) = pending else {
                return upstream
                    .next()
                    .await
                    .map(|item| (item, (upstream, None, alias)));
            };
            loop {
                if let Some(end) = first_event_boundary(&buf) {
                    let rest = buf.split_off(end);
                    let out = emit_frame(buf, rest, alias.as_deref());
                    return Some((Ok(out), (upstream, None, alias)));
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        buf.extend_from_slice(&chunk);
                        continue;
                    }
                    // A transport error before the first frame completes is
                    // terminal: the partial `message_start` we buffered is
                    // unparseable to the client, so surface the error rather
                    // than forward a truncated frame.
                    Some(Err(error)) => return Some((Err(error), (upstream, None, alias))),
                    // Stream ended before a full frame: forward what we have so
                    // no bytes are dropped.
                    None => {
                        return if buf.is_empty() {
                            None
                        } else {
                            Some((Ok(Bytes::from(buf)), (upstream, None, alias)))
                        };
                    }
                }
            }
        })
    })
}

/// Byte index just past the first SSE event boundary (`\n\n` or `\r\n\r\n`),
/// whichever appears first, or `None` if the buffer holds no complete frame.
fn first_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Reassemble the first frame (rewritten when it is a differing `message_start`)
/// with the trailing `rest` bytes into a single chunk.
fn emit_frame(frame: Vec<u8>, rest: Vec<u8>, alias: Option<&str>) -> Bytes {
    let rewritten = alias
        .and_then(|alias| std::str::from_utf8(&frame).ok().zip(Some(alias)))
        .and_then(|(frame, alias)| rewrite_message_start_frame(frame, alias));
    let mut out = rewritten.map(String::into_bytes).unwrap_or(frame);
    out.extend_from_slice(&rest);
    Bytes::from(out)
}

/// Rewrite `message.model` to `alias` in a single `message_start` SSE frame.
/// Returns `Some(frame)` only when the frame is a `message_start` event whose
/// model differs; any other frame (or an unparseable one) yields `None` so the
/// caller forwards the original bytes untouched.
fn rewrite_message_start_frame(frame: &str, alias: &str) -> Option<String> {
    let mut out = String::with_capacity(frame.len() + alias.len());
    let mut rewritten = false;
    for segment in frame.split_inclusive('\n') {
        let content_len = segment.trim_end_matches(['\n', '\r']).len();
        let (content, ending) = segment.split_at(content_len);
        if let Some(payload) = content.strip_prefix("data:") {
            if let Ok(mut value) = serde_json::from_str::<Value>(payload.trim_start()) {
                let is_start = value.get("type").and_then(Value::as_str) == Some("message_start");
                if is_start {
                    if let Some(model) = value.pointer_mut("/message/model") {
                        if model.as_str() != Some(alias) {
                            *model = Value::String(alias.to_string());
                            if let Ok(reserialized) = serde_json::to_string(&value) {
                                out.push_str("data: ");
                                out.push_str(&reserialized);
                                out.push_str(ending);
                                rewritten = true;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        out.push_str(segment);
    }
    rewritten.then_some(out)
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use futures_util::{stream, StreamExt};

    use super::{rewrite_first_model_stream, rewrite_message_start_frame, rewrite_response_model};

    type Item = Result<Bytes, std::convert::Infallible>;

    fn chunk(text: &str) -> Item {
        Ok(Bytes::from(text.to_owned()))
    }

    async fn collect(items: Vec<Item>, alias: Option<&str>) -> String {
        let upstream = stream::iter(items);
        let out: Vec<_> = rewrite_first_model_stream(upstream, alias.map(str::to_owned))
            .collect()
            .await;
        out.into_iter()
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn response_model_rewritten_when_differs() {
        let body = br#"{"id":"msg_1","model":"kimi-k2.7-code","role":"assistant"}"#.to_vec();
        let out = rewrite_response_model(body, "claude-go-kimi-k2.7-code-via-litellm");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "claude-go-kimi-k2.7-code-via-litellm");
        // Sibling fields survive.
        assert_eq!(value["id"], "msg_1");
    }

    #[test]
    fn response_model_untouched_when_matches() {
        let body = br#"{"model":"claude-alias","x":1}"#.to_vec();
        let original = body.clone();
        assert_eq!(rewrite_response_model(body, "claude-alias"), original);
    }

    #[test]
    fn response_model_untouched_when_absent_or_non_json() {
        // count_tokens-style body (no model field) and a non-JSON body both pass.
        let no_model = br#"{"input_tokens":42}"#.to_vec();
        assert_eq!(
            rewrite_response_model(no_model.clone(), "claude-alias"),
            no_model
        );
        let not_json = b"not json".to_vec();
        assert_eq!(
            rewrite_response_model(not_json.clone(), "claude-alias"),
            not_json
        );
    }

    #[test]
    fn frame_rewrites_message_start_model() {
        let frame = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"kimi-k2.7-code\",\"role\":\"assistant\"}}\n\n";
        let out = rewrite_message_start_frame(frame, "claude-alias").expect("model differs");
        assert!(out.starts_with("event: message_start\n"));
        assert!(out.ends_with("\n\n"));
        let data = out
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(value["message"]["model"], "claude-alias");
        assert_eq!(value["message"]["role"], "assistant");
    }

    #[test]
    fn frame_leaves_non_message_start_untouched() {
        let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n";
        assert!(rewrite_message_start_frame(frame, "claude-alias").is_none());
    }

    #[test]
    fn frame_leaves_matching_model_untouched() {
        let frame = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-alias\"}}\n\n";
        assert!(rewrite_message_start_frame(frame, "claude-alias").is_none());
    }

    #[tokio::test]
    async fn stream_none_alias_is_verbatim_passthrough() {
        let frame = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"kimi-k2.7-code\"}}\n\n";
        let out = collect(vec![chunk(frame)], None).await;
        assert_eq!(out, frame, "None alias must not touch the bytes");
    }

    #[tokio::test]
    async fn stream_rewrites_first_frame_single_chunk() {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"kimi-k2.7-code\"}}\n\n";
        let delta = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"model\":\"kimi-k2.7-code\"}\n\n";
        let out = collect(vec![chunk(start), chunk(delta)], Some("claude-alias")).await;
        assert!(out.contains("\"model\":\"claude-alias\""));
        // Only message_start is rewritten; a later frame that happens to carry
        // the raw id is left alone (it is not the model-of-record).
        assert!(out.contains("\"content_block_delta\",\"model\":\"kimi-k2.7-code\""));
    }

    #[tokio::test]
    async fn stream_rewrites_first_frame_split_across_chunks() {
        // The message_start frame arrives in three pieces, the boundary itself
        // straddling the last split — the accumulator must stitch it together.
        let out = collect(
            vec![
                chunk("event: message_start\ndata: {\"type\":\"message"),
                chunk("_start\",\"message\":{\"model\":\"kimi-k2.7-code\"}}\n"),
                chunk("\nevent: ping\ndata: {\"type\":\"ping\"}\n\n"),
            ],
            Some("claude-alias"),
        )
        .await;
        assert!(out.contains("\"model\":\"claude-alias\""));
        assert!(out.contains("event: ping"));
        assert!(!out.contains("kimi-k2.7-code"));
    }

    #[tokio::test]
    async fn stream_forwards_partial_frame_when_upstream_ends_early() {
        // Upstream dies before the first frame boundary: forward the partial
        // bytes rather than drop them.
        let partial = "event: message_start\ndata: {\"type\":\"message_start\"";
        let out = collect(vec![chunk(partial)], Some("claude-alias")).await;
        assert_eq!(out, partial);
    }
}
