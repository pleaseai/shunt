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

use super::{floor_tokens, SpendTap, UsageHolder};
use crate::gateway::spend::meter::RequestUsage;

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
    fn settle(self) {
        self.tap.record(&json_usage(
            self.kept.as_deref(),
            self.bytes,
            self.truncated,
        ));
    }
}

/// A whole JSON message's billable usage: its `usage` block when `kept`
/// holds a body that parses with one, otherwise `ceil(bytes / 4)` output
/// tokens. A `truncated` message additionally raises the output to the floor
/// of its delivered content, since its output count was synthesized.
pub(super) fn json_usage(kept: Option<&[u8]>, bytes: u64, truncated: bool) -> RequestUsage {
    let parsed = kept
        .and_then(|kept| serde_json::from_slice::<UsageHolder>(kept).ok())
        .and_then(|holder| holder.usage);
    // The delivered-content floor, parsed at most once: a usage block without
    // an output count and a truncated message both want it.
    let wants_content_floor = truncated
        || parsed
            .as_ref()
            .is_some_and(|fields| fields.output_tokens.is_none());
    let content_floor =
        wants_content_floor.then(|| floor_tokens(kept.and_then(content_chars).unwrap_or(bytes)));
    let mut usage = RequestUsage::default();
    match parsed {
        Some(fields) => {
            fields.apply_to(&mut usage);
            // A usage block without an output count gets the same floor a
            // stream with no final count does.
            if fields.output_tokens.is_none() {
                usage.tokens.output_tokens = content_floor.unwrap_or_default();
            }
        }
        None => usage.tokens.output_tokens = floor_tokens(bytes),
    }
    if let Some(floor) = content_floor.filter(|_| truncated) {
        usage.tokens.output_tokens = usage.tokens.output_tokens.max(floor);
    }
    usage
}

/// Characters of generated content (text, thinking, tool input) in a whole
/// Anthropic message, or `None` when the body has no readable `content`.
fn content_chars(kept: &[u8]) -> Option<u64> {
    let value = serde_json::from_slice::<serde_json::Value>(kept).ok()?;
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
