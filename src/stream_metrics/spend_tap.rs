//! Served-response spend metering for `[server.spend]` (issue #728).
//!
//! A [`SpendTap`] rides along with one served `/v1/messages` response and adds
//! that response's priced usage to the principal's counters once, when the
//! body ends. It is passive in the same sense as the rest of this module: every
//! byte is forwarded exactly as it was polled, at the moment it was polled, and
//! nothing the tap does can fail or truncate the response it meters.
//!
//! - A streamed response is read by [`StreamSpend`], fed the complete SSE
//!   frames the observer already splits. Anthropic's `message_delta` usage is
//!   cumulative, so a later value replaces an earlier one per field. When the
//!   upstream's final output count never arrives — the stream was cut, the
//!   client hung up, or a translating adapter synthesized the end after a cut
//!   (the [`UPSTREAM_TRUNCATED_MARKER`]) — the output is billed at a floor of
//!   one token per [`CHARS_PER_TOKEN`] characters of delta text the client was
//!   sent.
//! - A non-streamed response is read by [`JsonSpendBody`], a bounded tee:
//!   it keeps at most [`MAX_JSON_TEE_BYTES`] of the body for an end-of-body
//!   `usage` parse, and bills a floor from the byte count when the body was
//!   larger than that, cut, or carried no readable `usage`.
//!
//! Only a `2xx` response is billed; an error the upstream answered with costs
//! nothing. A response metered elsewhere — the gated capture, for one — is
//! served with no tap at all, which is how a caller switches this hook off.

use std::{
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
};
use http_body::{Frame, SizeHint};
use serde::Deserialize;

use super::UPSTREAM_TRUNCATED_MARKER;
use crate::gateway::{
    spend::{meter::RequestUsage, pricing::PriceTable},
    store::GatewayStores,
};

/// Characters of delivered text billed as one output token when the upstream's
/// own count is missing — the reference gateway's estimate.
const CHARS_PER_TOKEN: u64 = 4;

/// Bytes of a non-streamed body kept for the end-of-body `usage` parse. A
/// Messages reply is bounded by `max_tokens`, so a real one is far below this;
/// past it the tee stops copying and bills the byte floor instead, keeping the
/// extra memory one response may pin to this constant.
pub(crate) const MAX_JSON_TEE_BYTES: usize = 4 * 1024 * 1024;

/// What the pricing table keys on: the upstream that actually served the
/// turn, the client-facing model, and the model string sent to that upstream.
#[derive(Clone, Debug)]
struct Target {
    provider: String,
    client_model: String,
    upstream_model: String,
}

/// The served-response spend hook for one request.
///
/// Cheap to clone (three `Arc`s); the clones share one [`Target`] slot, so a
/// committed stream can name its winner after the observer was built.
#[derive(Clone)]
pub(crate) struct SpendTap {
    stores: Arc<GatewayStores>,
    prices: Arc<PriceTable>,
    principal: Arc<str>,
    target: Arc<Mutex<Option<Target>>>,
}

impl SpendTap {
    /// The tap for a request, or `None` when nothing is metered: no
    /// `[server.spend]`, or no principal (an all-passthrough chain, where the
    /// caller pays the upstream with their own credential).
    pub(crate) fn for_request(
        state: &crate::server::AppState,
        principal: Option<&str>,
    ) -> Option<Self> {
        state.config.server.spend.as_ref()?;
        Some(Self {
            stores: state.gateway_stores.clone(),
            prices: state.spend_prices.clone(),
            principal: Arc::from(principal?),
            target: Arc::default(),
        })
    }

    /// Names the upstream that served the turn. Until this is called nothing
    /// is billed: a committed stream that never selected a winner served no
    /// upstream's output.
    pub(crate) fn set_target(&self, provider: &str, client_model: &str, upstream_model: &str) {
        *self.target.lock().unwrap_or_else(PoisonError::into_inner) = Some(Target {
            provider: provider.to_string(),
            client_model: client_model.to_string(),
            upstream_model: upstream_model.to_string(),
        });
    }

    /// Prices `usage` on the target and adds it to the principal's counters.
    /// Never panics into the response: a failure inside the meter is caught,
    /// logged, and the response it was metering carries on.
    fn record(&self, usage: &RequestUsage) {
        if *usage == RequestUsage::default() {
            return;
        }
        let Some(target) = self
            .target
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let metered = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let meter = self.stores.spend.meter();
            let cost = meter.cost(
                &self.prices,
                &target.provider,
                &target.client_model,
                &target.upstream_model,
                usage,
            );
            meter.record(&self.principal, now, cost);
        }));
        if metered.is_err() {
            tracing::warn!(
                principal = %self.principal,
                provider = %target.provider,
                "spend meter failed; this response is not metered"
            );
        }
    }
}

/// The usage fields of an Anthropic usage block that cost money. Unknown
/// fields (`cache_creation`, `service_tier`, …) are ignored, and a field the
/// block omits or nulls leaves the running value alone.
#[derive(Debug, Default, Deserialize)]
struct UsageFields {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Default, Deserialize)]
struct ServerToolUse {
    web_search_requests: Option<u64>,
}

impl UsageFields {
    /// Overwrites each field of `usage` this block reports.
    fn apply_to(&self, usage: &mut RequestUsage) {
        let tokens = &mut usage.tokens;
        for (slot, value) in [
            (&mut tokens.input_tokens, self.input_tokens),
            (&mut tokens.output_tokens, self.output_tokens),
            (
                &mut tokens.cache_read_input_tokens,
                self.cache_read_input_tokens,
            ),
            (
                &mut tokens.cache_creation_input_tokens,
                self.cache_creation_input_tokens,
            ),
            (
                &mut usage.web_search_requests,
                self.server_tool_use
                    .as_ref()
                    .and_then(|tool| tool.web_search_requests),
            ),
        ] {
            if let Some(value) = value {
                *slot = value;
            }
        }
    }
}

#[derive(Deserialize)]
struct MessageStart {
    message: UsageHolder,
}

#[derive(Deserialize)]
struct UsageHolder {
    usage: Option<UsageFields>,
}

#[derive(Deserialize)]
struct BlockDelta<'a> {
    #[serde(borrow)]
    delta: DeltaText<'a>,
}

/// The delta fields that carry generated output: `text_delta`,
/// `input_json_delta`, and `thinking_delta`. Borrowed when the JSON string has
/// no escapes, so counting a delta usually allocates nothing.
#[derive(Deserialize)]
struct DeltaText<'a> {
    #[serde(borrow, default)]
    text: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    partial_json: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    thinking: Option<std::borrow::Cow<'a, str>>,
}

/// One billed token per [`CHARS_PER_TOKEN`] units, rounding up.
fn floor_tokens(units: u64) -> u64 {
    units.div_ceil(CHARS_PER_TOKEN)
}

/// Spend accounting over one streamed Anthropic response.
///
/// A frame larger than the observer's parse cap is skipped without being
/// seen here, so its text does not count toward the floor; its usage, which
/// rides on small frames, still does.
pub(super) struct StreamSpend {
    tap: SpendTap,
    usage: RequestUsage,
    /// Whether a `message_delta` reported `output_tokens`: the upstream's own
    /// final count, which the floor never overrides unless the stream was
    /// marked truncated.
    output_reported: bool,
    truncated: bool,
    delivered_chars: u64,
}

impl StreamSpend {
    pub(super) fn new(tap: SpendTap) -> Self {
        Self {
            tap,
            usage: RequestUsage::default(),
            output_reported: false,
            truncated: false,
            delivered_chars: 0,
        }
    }

    /// Reads one complete SSE frame. Malformed JSON is skipped: the frame was
    /// still relayed, and the floor covers output whose usage never parsed.
    pub(super) fn observe_frame(&mut self, frame: &[u8]) {
        if frame == UPSTREAM_TRUNCATED_MARKER {
            self.truncated = true;
            return;
        }
        let (event, data) = super::event_and_data(frame);
        let Some(data) = data else {
            return;
        };
        match event {
            Some(b"content_block_delta") => {
                if let Ok(frame) = serde_json::from_slice::<BlockDelta<'_>>(data) {
                    let delta = frame.delta;
                    let chars = [delta.text, delta.partial_json, delta.thinking]
                        .iter()
                        .flatten()
                        .map(|text| text.chars().count() as u64)
                        .sum::<u64>();
                    self.delivered_chars = self.delivered_chars.saturating_add(chars);
                }
            }
            Some(b"message_start") => {
                if let Ok(MessageStart {
                    message: UsageHolder { usage: Some(usage) },
                }) = serde_json::from_slice(data)
                {
                    usage.apply_to(&mut self.usage);
                }
            }
            Some(b"message_delta") => {
                if let Ok(UsageHolder { usage: Some(usage) }) = serde_json::from_slice(data) {
                    self.output_reported |= usage.output_tokens.is_some();
                    usage.apply_to(&mut self.usage);
                }
            }
            _ => {}
        }
    }

    /// What this stream costs: the reported usage, with the output raised to
    /// the delivered-text floor when the final count is missing or was
    /// synthesized after a cut.
    fn billable(&self) -> RequestUsage {
        let mut usage = self.usage;
        if !self.output_reported || self.truncated {
            let floor = floor_tokens(self.delivered_chars);
            usage.tokens.output_tokens = usage.tokens.output_tokens.max(floor);
        }
        usage
    }

    /// Bills the stream once, however it ended. `status` is the status the
    /// response opened with; a non-`2xx` stream is not billed.
    pub(super) fn settle(self, status: StatusCode) {
        if status.is_success() {
            self.tap.record(&self.billable());
        }
    }
}

/// The non-streamed counterpart: a body wrapper that forwards every frame
/// unchanged — size hint and end-of-stream included, so the response keeps its
/// framing — while keeping a bounded copy for the `usage` parse.
pub(super) struct JsonSpendBody {
    inner: Body,
    /// `None` once the response has been billed.
    spend: Option<JsonSpend>,
}

struct JsonSpend {
    tap: SpendTap,
    status: StatusCode,
    kept: Vec<u8>,
    bytes: u64,
    bound: usize,
    overflowed: bool,
}

impl JsonSpendBody {
    pub(super) fn new(inner: Body, tap: SpendTap, status: StatusCode) -> Self {
        Self::with_bound(inner, tap, status, MAX_JSON_TEE_BYTES)
    }

    fn with_bound(inner: Body, tap: SpendTap, status: StatusCode, bound: usize) -> Self {
        Self {
            inner,
            spend: Some(JsonSpend {
                tap,
                status,
                kept: Vec::new(),
                bytes: 0,
                bound,
                overflowed: false,
            }),
        }
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
        if self.overflowed {
            return;
        }
        if self.kept.len().saturating_add(chunk.len()) > self.bound {
            self.overflowed = true;
            self.kept = Vec::new();
        } else {
            self.kept.extend_from_slice(chunk);
        }
    }

    /// A body that parses whole is complete however the server stopped
    /// polling it (a sized body may be dropped after its last frame without a
    /// final `Ready(None)`); a cut one does not parse and bills the floor.
    fn settle(self) {
        if !self.status.is_success() {
            return;
        }
        let parsed = (!self.overflowed)
            .then(|| serde_json::from_slice::<UsageHolder>(&self.kept).ok())
            .flatten()
            .and_then(|holder| holder.usage);
        let usage = match parsed {
            Some(fields) => {
                let mut usage = RequestUsage::default();
                fields.apply_to(&mut usage);
                usage
            }
            None => {
                let mut usage = RequestUsage::default();
                usage.tokens.output_tokens = floor_tokens(self.bytes);
                usage
            }
        };
        self.tap.record(&usage);
    }
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

#[cfg(test)]
mod tests;
