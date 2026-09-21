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

/// Ceiling on the first frame the rewriter will buffer before giving up and
/// passing the stream through untouched.
///
/// Callers do not name this directly — they ask [`first_frame_ceiling`] for the
/// bound that applies to them.
const MAX_FIRST_FRAME_BYTES: usize = 64 * 1024;

/// The first-frame scan ceiling for a caller holding `response_byte_cap`.
///
/// Extracted from its call sites purely so the composition is testable. The
/// choice is invisible end-to-end: with a cap below [`MAX_FIRST_FRAME_BYTES`],
/// an inverted `min` — or passing the constant unconditionally — still refuses
/// the same oversized replies with the same `oversized` outcome in the same
/// time, because the only thing that differs is peak allocation *inside* the
/// scan, which no outcome exposes. A unit test on this function is therefore
/// the only place that regression can be caught.
///
/// `None` is the client path, which is bounded by the constant alone.
pub(super) fn first_frame_ceiling(response_byte_cap: Option<usize>) -> usize {
    response_byte_cap.map_or(MAX_FIRST_FRAME_BYTES, |cap| cap.min(MAX_FIRST_FRAME_BYTES))
}

/// Rewrite a non-streaming Messages response body's top-level `model` to
/// `alias` when it is present and differs. A non-JSON body, a body without a
/// `model` field (e.g. `count_tokens`), or one already equal to `alias` is
/// returned unchanged.
pub(super) fn rewrite_response_model(body: Bytes, alias: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    match object.get("model").and_then(Value::as_str) {
        Some(model) if model != alias => {
            object.insert("model".to_string(), Value::String(alias.to_string()));
            serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
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
///
/// `max_first_frame` is that buffer's ceiling. It exists as a parameter rather
/// than as the [`MAX_FIRST_FRAME_BYTES`] constant alone because an internal
/// judge call runs under `judge_max_response_bytes`, and a cap *below* 64 KiB
/// would otherwise not bound this scan at all: the rewriter would buffer to the
/// constant and only then hand the bytes to `routing::serve::collect_bounded`
/// to be refused. Callers pass `min(cap, MAX_FIRST_FRAME_BYTES)`; a caller with
/// no cap passes the constant.
///
/// The ceiling bounds the *buffer*, not the peak: the chunk that crosses it is
/// appended before the check, because the bail-out forwards every byte it has
/// read and dropping the remainder of a chunk to stay under the line would lose
/// stream content. Peak is therefore the ceiling plus one upstream chunk, freed
/// as soon as the bail-out emits.
pub(super) fn rewrite_first_model_stream<S, E>(
    upstream: S,
    alias: Option<String>,
    max_first_frame: usize,
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
                    let out = alias
                        .as_deref()
                        .and_then(|alias| std::str::from_utf8(&buf[..end]).ok().zip(Some(alias)))
                        .and_then(|(frame, alias)| rewrite_message_start_frame(frame, alias))
                        .map(|rewritten| {
                            let mut out = rewritten.into_bytes();
                            out.extend_from_slice(&buf[end..]);
                            Bytes::from(out)
                        })
                        .unwrap_or_else(|| Bytes::from(buf));
                    return Some((Ok(out), (upstream, None, alias)));
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        buf.extend_from_slice(&chunk);
                        if buf.len() > max_first_frame
                            && first_event_boundary(&buf).is_none_or(|end| end > max_first_frame)
                        {
                            return Some((Ok(Bytes::from(buf)), (upstream, None, alias)));
                        }
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

    use super::{
        first_frame_ceiling, rewrite_first_model_stream, rewrite_message_start_frame,
        rewrite_response_model, MAX_FIRST_FRAME_BYTES,
    };

    type Item = Result<Bytes, std::convert::Infallible>;

    fn chunk(text: &str) -> Item {
        Ok(Bytes::from(text.to_owned()))
    }

    async fn collect(items: Vec<Item>, alias: Option<&str>) -> String {
        collect_within(items, alias, MAX_FIRST_FRAME_BYTES).await
    }

    async fn collect_within(
        items: Vec<Item>,
        alias: Option<&str>,
        max_first_frame: usize,
    ) -> String {
        let upstream = stream::iter(items);
        let out: Vec<_> =
            rewrite_first_model_stream(upstream, alias.map(str::to_owned), max_first_frame)
                .collect()
                .await;
        out.into_iter()
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn response_model_rewritten_when_differs() {
        let body =
            Bytes::from_static(br#"{"id":"msg_1","model":"kimi-k2.7-code","role":"assistant"}"#);
        let out = rewrite_response_model(body, "claude-go-kimi-k2.7-code-via-litellm");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "claude-go-kimi-k2.7-code-via-litellm");
        // Sibling fields survive.
        assert_eq!(value["id"], "msg_1");
    }

    #[test]
    fn response_model_untouched_when_matches() {
        let body = Bytes::from_static(br#"{"model":"claude-alias","x":1}"#);
        let original = body.clone();
        assert_eq!(rewrite_response_model(body, "claude-alias"), original);
    }

    #[test]
    fn response_model_untouched_when_absent_or_non_json() {
        // count_tokens-style body (no model field) and a non-JSON body both pass.
        let no_model = Bytes::from_static(br#"{"input_tokens":42}"#);
        assert_eq!(
            rewrite_response_model(no_model.clone(), "claude-alias"),
            no_model
        );
        let not_json = Bytes::from_static(b"not json");
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
    async fn stream_rewrites_small_first_frame_in_oversized_chunk() {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"upstream\"}}\n\n";
        let trailing = "x".repeat(MAX_FIRST_FRAME_BYTES);
        let out = collect(
            vec![chunk(&format!("{start}{trailing}"))],
            Some("claude-alias"),
        )
        .await;
        assert!(out.contains("\"model\":\"claude-alias\""));
        assert!(out.ends_with(&trailing));
    }

    #[tokio::test]
    async fn stream_stops_buffering_when_first_frame_exceeds_limit() {
        let oversized = "x".repeat(MAX_FIRST_FRAME_BYTES + 1);
        let out = collect(vec![chunk(&oversized)], Some("claude-alias")).await;
        assert_eq!(out, oversized);
    }

    /// The ceiling composition, which nothing downstream can check.
    ///
    /// Each arm pins a regression that compiles and passes every other test in
    /// this file: an inverted `min` (the cap arm would return the constant),
    /// and an unconditional constant (same). The `None` arm is the client path,
    /// which must keep the constant it has always had.
    #[test]
    fn the_first_frame_ceiling_takes_whichever_bound_is_smaller() {
        assert_eq!(
            first_frame_ceiling(None),
            MAX_FIRST_FRAME_BYTES,
            "the client path is bounded by the constant alone"
        );
        assert_eq!(
            first_frame_ceiling(Some(1024)),
            1024,
            "a cap below the constant is the binding one"
        );
        assert_eq!(
            first_frame_ceiling(Some(MAX_FIRST_FRAME_BYTES * 2)),
            MAX_FIRST_FRAME_BYTES,
            "a cap above the constant must not raise the scan ceiling"
        );
    }

    /// A caller under a byte cap smaller than [`MAX_FIRST_FRAME_BYTES`] bounds
    /// the scan at *its* cap, not at the constant.
    ///
    /// An internal judge call runs under `judge_max_response_bytes`. With the
    /// constant hardcoded, a 1 KiB cap still let a nonconforming SSE reply
    /// buffer to 64 KiB here before `collect_bounded` could refuse it — the
    /// configured limit bounded the refusal, not the allocation. Non-vacuity:
    /// restore the constant inside the loop and this goes red, because the
    /// frame boundary at 4 KiB is under 64 KiB and so gets rewritten.
    #[tokio::test]
    async fn a_caller_cap_below_the_constant_bounds_the_scan() {
        let filler = "x".repeat(4096);
        let start = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"model\":\"upstream\",\"pad\":\"{filler}\"}}}}\n\n"
        );

        // Under the constant the frame is found and rewritten...
        let unbounded = collect_within(
            vec![chunk(&start)],
            Some("claude-alias"),
            MAX_FIRST_FRAME_BYTES,
        )
        .await;
        assert!(
            unbounded.contains("\"model\":\"claude-alias\""),
            "the control: a 4 KiB frame is well inside the 64 KiB constant"
        );

        // ...but a 1 KiB caller cap gives up before buffering that far, and
        // passes the bytes through untouched for the collector to refuse.
        let bounded = collect_within(vec![chunk(&start)], Some("claude-alias"), 1024).await;
        assert_eq!(
            bounded, start,
            "a cap below the constant must stop the scan at the cap"
        );
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
