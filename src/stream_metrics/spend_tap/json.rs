//! The non-streamed spend tee: [`JsonSpendBody`] and the whole-message usage
//! parse it shares with [`SpendTap::bill_json`](super::SpendTap::bill_json).

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
};
use http_body::{Frame, SizeHint};

use super::{floor_tokens, responses, SpendTap, UsageHolder};
use crate::{gateway::spend::meter::RequestUsage, stream_metrics::Protocol};

/// Bytes of a non-streamed body kept for the end-of-body `usage` parse. A
/// Messages reply is bounded by `max_tokens`, so a real one is far below this;
/// past it the tee stops copying and bills the byte floor instead, keeping the
/// extra memory one response may pin to this constant.
pub(crate) const MAX_JSON_TEE_BYTES: usize = 4 * 1024 * 1024;

/// The non-streamed counterpart: a body wrapper that forwards every frame
/// unchanged — size hint and end-of-stream included, so the response keeps its
/// framing — while keeping a bounded copy for the `usage` parse.
pub(in crate::stream_metrics) struct JsonSpendBody {
    inner: Body,
    /// `None` once the response has been billed.
    spend: Option<JsonSpend>,
}

struct JsonSpend {
    tap: SpendTap,
    protocol: Protocol,
    /// The body so far; `None` once it outgrew `bound`.
    kept: Option<Vec<u8>>,
    bytes: u64,
    bound: usize,
    /// The adapter synthesized this message from an upstream that ended
    /// early, so its reported output count is not the real one.
    truncated: bool,
}

impl JsonSpendBody {
    pub(in crate::stream_metrics) fn new(inner: Body, tap: SpendTap, status: StatusCode) -> Self {
        Self::with_bound(inner, tap, status, MAX_JSON_TEE_BYTES)
    }

    pub(super) fn with_bound(inner: Body, tap: SpendTap, status: StatusCode, bound: usize) -> Self {
        // Only a `2xx` reply is ever billed, so any other status is not even
        // copied.
        Self {
            inner,
            spend: status.is_success().then(|| JsonSpend {
                tap,
                protocol: Protocol::Anthropic,
                kept: Some(Vec::new()),
                bytes: 0,
                bound,
                truncated: false,
            }),
        }
    }

    /// Marks the reply as synthesized after an upstream cut, so its output is
    /// billed at no less than the delivered-content floor.
    pub(in crate::stream_metrics) fn truncated(mut self, truncated: bool) -> Self {
        if let Some(spend) = self.spend.as_mut() {
            spend.truncated = truncated;
        }
        self
    }

    /// Reads the body as a Responses message instead: the top-level `usage`
    /// with its cached tokens split out of the input, and `output[]` text as
    /// the delivered content.
    pub(in crate::stream_metrics) fn protocol(mut self, protocol: Protocol) -> Self {
        if let Some(spend) = self.spend.as_mut() {
            spend.protocol = protocol;
        }
        self
    }

    fn settle(&mut self) {
        if let Some(spend) = self.spend.take() {
            spend.settle();
        }
    }
}

impl JsonSpend {
    fn observe(&mut self, chunk: &[u8]) {
        self.bytes = self.bytes.saturating_add(chunk.len() as u64);
        let Some(kept) = self.kept.as_mut() else {
            return;
        };
        if kept.len().saturating_add(chunk.len()) > self.bound {
            self.kept = None;
        } else {
            kept.extend_from_slice(chunk);
        }
    }

    /// A body that parses whole is complete however the server stopped
    /// polling it (a sized body may be dropped after its last frame without a
    /// final `Ready(None)`); a cut one does not parse and bills the floor.
    ///
    /// A Responses body whose usage did not report its input bills the
    /// request's estimated input, as an incomplete stream does — unless no
    /// byte was ever polled (a body dropped before the client read it), which
    /// stays unmetered.
    fn settle(self) {
        let (usage, input_reported) = json_usage_parts(
            self.protocol,
            self.kept.as_deref(),
            self.bytes,
            self.truncated,
        );
        let estimate =
            matches!(self.protocol, Protocol::Responses) && !input_reported && self.bytes > 0;
        self.tap.record_estimating_input(usage, estimate);
    }
}

/// A whole JSON message's billable usage: its `usage` block when `kept`
/// holds a body that parses with one, otherwise `ceil(bytes / 4)` output
/// tokens. A `truncated` message, or one reporting `output_tokens: 0`,
/// additionally raises the output to the floor of its delivered content: the
/// first count was synthesized, and the second is what a translating adapter
/// reports when an emulated stop sequence ended the turn before the
/// upstream's usage arrived. A nonzero count on an untruncated message is
/// billed as reported.
///
/// When the content is unreadable, the floor falls back to the byte count for
/// a truncated message or a usage block without an output count, but to zero
/// for a clean message reporting zero — with no content to count, a reported
/// zero stays zero rather than billing the whole body.
pub(super) fn json_usage(kept: Option<&[u8]>, bytes: u64, truncated: bool) -> RequestUsage {
    json_usage_for(Protocol::Anthropic, kept, bytes, truncated)
}

/// [`json_usage`] for a body of either protocol. A Responses body differs in
/// where the `usage` and the content live and in how the cached tokens are
/// billed (see [`responses`]); the floor rules are the same.
pub(super) fn json_usage_for(
    protocol: Protocol,
    kept: Option<&[u8]>,
    bytes: u64,
    truncated: bool,
) -> RequestUsage {
    json_usage_parts(protocol, kept, bytes, truncated).0
}

/// [`json_usage_for`], plus whether the body's usage block reported the input.
fn json_usage_parts(
    protocol: Protocol,
    kept: Option<&[u8]>,
    bytes: u64,
    truncated: bool,
) -> (RequestUsage, bool) {
    let mut usage = RequestUsage::default();
    let mut input_reported = false;
    // `Some(reported)` when the body carried a usage block; `reported` is its
    // own output count, if it had one.
    let parsed = kept.and_then(|kept| usage_block(protocol, kept, &mut usage, &mut input_reported));
    let reported_zero = parsed == Some(Some(0));
    let missing_output = parsed == Some(None);
    let raise_to_floor = truncated || reported_zero;
    // The delivered-content floor, parsed at most once: a usage block without
    // an output count, a reported zero, and a truncated message all want it.
    let wants_content_floor = raise_to_floor || missing_output;
    let unreadable_content = if truncated || missing_output {
        bytes
    } else {
        0
    };
    let content_floor = wants_content_floor.then(|| {
        floor_tokens(
            kept.and_then(|kept| content_chars(protocol, kept))
                .unwrap_or(unreadable_content),
        )
    });
    match parsed {
        // A usage block without an output count gets the same floor a
        // stream with no final count does.
        Some(None) => usage.tokens.output_tokens = content_floor.unwrap_or_default(),
        Some(Some(_)) => {}
        None => usage.tokens.output_tokens = floor_tokens(bytes),
    }
    if let Some(floor) = content_floor.filter(|_| raise_to_floor) {
        usage.tokens.output_tokens = usage.tokens.output_tokens.max(floor);
    }
    (usage, input_reported)
}

/// Applies the body's `usage` block to `usage`. `None` when the body has no
/// parsable block; otherwise the block's own output count.
fn usage_block(
    protocol: Protocol,
    kept: &[u8],
    usage: &mut RequestUsage,
    input_reported: &mut bool,
) -> Option<Option<u64>> {
    if matches!(protocol, Protocol::Responses) {
        let fields = serde_json::from_slice::<responses::ResponseHolder>(kept)
            .ok()?
            .usage?;
        *input_reported = fields.reports_input();
        let reported = fields.apply_to(usage);
        return Some(reported.then_some(usage.tokens.output_tokens));
    }
    let fields = serde_json::from_slice::<UsageHolder>(kept).ok()?.usage?;
    fields.apply_to(usage);
    Some(fields.output_tokens)
}

/// Characters of generated content in a whole message, or `None` when the body
/// has none readable. Anthropic: text, thinking and tool input in `content`.
/// Responses: the `output_text` parts of the `message` items in `output[]`.
fn content_chars(protocol: Protocol, kept: &[u8]) -> Option<u64> {
    let value = serde_json::from_slice::<serde_json::Value>(kept).ok()?;
    if matches!(protocol, Protocol::Responses) {
        return responses_content_chars(&value);
    }
    let blocks = value.get("content")?.as_array()?;
    Some(
        blocks
            .iter()
            .map(|block| {
                let text = |key: &str| {
                    block
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, |text| text.chars().count() as u64)
                };
                let input = block
                    .get("input")
                    .map_or(0, |input| input.to_string().chars().count() as u64);
                text("text") + text("thinking") + input
            })
            .sum(),
    )
}

fn responses_content_chars(value: &serde_json::Value) -> Option<u64> {
    fn kind(value: &serde_json::Value) -> Option<&str> {
        value.get("type").and_then(serde_json::Value::as_str)
    }
    let items = value.get("output")?.as_array()?;
    Some(
        items
            .iter()
            .filter(|item| kind(item) == Some("message"))
            .filter_map(|item| item.get("content")?.as_array())
            .flatten()
            .filter(|part| kind(part) == Some("output_text"))
            .filter_map(|part| part.get("text")?.as_str())
            .map(|text| text.chars().count() as u64)
            .sum(),
    )
}

impl http_body::Body for JsonSpendBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(spend)) = (frame.data_ref(), self.spend.as_mut()) {
                    spend.observe(data);
                }
            }
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => self.settle(),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for JsonSpendBody {
    fn drop(&mut self) {
        self.settle();
    }
}
