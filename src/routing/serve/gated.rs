//! The gated call: the first model call of an `escalation` or `advisor` drive,
//! which is not a judge call but the caller's own turn (ADR-0005 §3, §4,
//! issue #596).
//!
//! What this module guards is that a retained turn is *exactly* the turn the
//! caller would have received live, and that only a whole one is ever served.
//! Four things follow, and each is enforced here rather than left to the
//! drive:
//!
//! * **The caller's body, not a re-encoding.** libsy hands over a neutral
//!   request; the bytes dispatched are the client's own normalized body, so
//!   nothing the codec cannot represent is lost from the answer turn. The
//!   neutral request is used only to decide *that* a call is wanted.
//! * **The caller's dispatch.** The same failover chain, the same credential
//!   handling a live turn gets (the post-admission headers and the caller's
//!   real [`InboundContext`], so a passthrough target keeps the caller's own
//!   upstream credential), and `caller = "client"` in the request metrics. The
//!   chain is resolved through [`routing::resolve_target_chain`], which stamps
//!   the advertised router id as `Route.model` — so the adapter renders the id
//!   the caller asked for into `message_start.model` *before* a byte is
//!   captured, and a replay cannot hand Claude Code the executor's id to
//!   restore on `--resume` (issue #172).
//! * **The caller's mode.** A streaming caller's turn is made streaming and
//!   its rendered SSE frames are retained; a non-streaming caller's is made
//!   non-streaming and its single JSON message is retained. No SSE-to-JSON
//!   conversion exists or is needed.
//! * **Terminal or nothing.** A turn is [`GatedCapture::Retained`] only after
//!   an authoritative terminal marker: a complete `message_stop` frame and no
//!   `error` frame or upstream-truncation marker before it on a stream, one
//!   parseable `message` object not marked [`UpstreamTruncated`] on a JSON
//!   body. Anything short of that — a bound crossed, a transport broken
//!   before the marker, a `200` that simply stopped — is
//!   [`GatedCapture::Cut`], and a cut turn is never replayed. A stream's turn
//!   ends at its `message_stop` frame, as the live relay's does: the capture
//!   stops reading there and keeps exactly the bytes through it, so whatever
//!   follows — a keep-alive, a connection held open, a break — neither joins
//!   the replay nor holds the capture open until a bound cuts it.
//!
//! The capture is recorded in a slot the drive reads back, and the bytes
//! replayed are always those recorded ones. libsy's own buffered copy is used
//! only as the "serve the retained turn" signal: re-encoding it would be a
//! translation of the answer, not the answer.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode, Uri};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use switchyard_libsy::{CallModel, LibsyError};
use switchyard_protocol::{LlmClientError, LlmResponse, ModelId, Response};
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use super::bounds::{
    bound_stream, first_frame_len, frame_event, normalize_line_endings, BoundExceeded, GatedBounds,
};
use crate::config::CallBounds;
use crate::proxy::failover::{
    chain::{run_chain, ChainRequest, ChainSuccess},
    InboundContext,
};
use crate::proxy::ForwardError;
use crate::request::RequestBody;
use crate::routing;
use crate::server::AppState;
use crate::stream_metrics::{UpstreamTruncated, UPSTREAM_TRUNCATED_MARKER};

/// Everything the gated call needs from the admitted client request.
///
/// Borrowed whole from `proxy::failover::forward`, after `check_inbound_auth`:
/// `base_headers` and `inbound` are what that gate produced, which is what
/// makes the dispatch the caller's rather than the gateway's.
pub(crate) struct GatedRequest<'a> {
    pub state: &'a AppState,
    pub uri: &'a Uri,
    pub base_headers: &'a HeaderMap,
    pub inbound: &'a InboundContext,
    pub body: &'a RequestBody,
    /// The id as the client sent it, for the chain's response stamp.
    pub requested_model: &'a str,
    /// The advertised router id, stamped as `Route.model` on the gated chain.
    pub router_id: &'a str,
}

/// A whole, terminal turn, held until the verdict decides whether it is served.
pub(crate) struct RetainedTurn {
    pub status: StatusCode,
    /// The response headers as the chain stamped them (`x-gateway-upstream`,
    /// `x-gateway-model`, `x-gateway-upstream-model`); the router pair is
    /// stamped when the verdict is known.
    pub headers: HeaderMap,
    /// The rendered bytes the caller would have received live.
    pub body: Bytes,
    pub streaming: bool,
    /// The upstream that answered, for the stream observer on replay.
    pub provider: String,
    pub model: String,
    /// Captured through the committed streaming chain, whose safeguard
    /// synthesis already ran over these bytes, so the replay must not run it
    /// again. The stream observer still runs on the replay.
    pub synthesized: bool,
}

/// Why a gated turn was discarded. Each is a turn the caller must not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutReason {
    /// One of the three `gated_*` bounds.
    Bound(BoundExceeded),
    /// The upstream connection broke part-way through the body.
    Transport,
    /// A `2xx` that ended without its terminal marker.
    Nonterminal,
}

impl std::fmt::Display for CutReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bound(bound) => write!(formatter, "the turn crossed {bound}"),
            Self::Transport => formatter.write_str("the upstream connection broke mid-turn"),
            Self::Nonterminal => formatter.write_str("the turn ended before its terminal marker"),
        }
    }
}

/// The transport error libsy is handed for a cut turn: it is what makes
/// escalation fall back to the strong tier (upstream catches exactly this
/// variant while buffering) and what an advisor propagates as a failed turn.
#[derive(Debug)]
struct GatedCut(CutReason);

impl std::fmt::Display for GatedCut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "gated turn discarded: {}", self.0)
    }
}

impl std::error::Error for GatedCut {}

/// An upstream refusal the gated call received, kept so it can be relayed to
/// the caller unchanged if the drive ends on it.
pub(crate) enum UpstreamFailure {
    /// A non-`2xx` the chain answered with and does not advance on — relayed
    /// as the client path relays one.
    Answered {
        status: StatusCode,
        response: axum::response::Response,
        /// The collected body, for libsy's typed error.
        body: String,
    },
    /// A chain failure the client path would have returned as its error.
    Failed {
        message: String,
        response: axum::response::Response,
    },
}

/// What the gated call produced.
pub(crate) enum GatedCapture {
    Retained(RetainedTurn),
    Cut(CutReason),
    UpstreamError(UpstreamFailure),
}

/// Serve the drive's first `CallModel` with the caller's own turn.
///
/// Records the capture before fulfilling the promise, so the drive can read it
/// back whatever libsy then does with the response. Returns `Err` only when
/// `respond` itself fails, as [`super::judge_call`] does.
pub(crate) async fn gated_call(
    request: &GatedRequest<'_>,
    call: CallModel,
    bounds: CallBounds,
    slot: &Mutex<Option<GatedCapture>>,
) -> switchyard_libsy::Result<()> {
    let Some(target) = call.models.first().cloned() else {
        return call.respond(Err(LibsyError::NoTargets));
    };
    let capture = capture(request, target.as_str(), bounds).await;
    let result = promise_result(&capture, &target);
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(capture);
    call.respond(result)
}

/// Dispatch and retain one gated turn under its three bounds.
///
/// The whole call — headers and body — is under `gated_max_duration`, awaited
/// in place and never spawned: dropping the future on elapse is what cancels
/// the upstream request.
async fn capture(request: &GatedRequest<'_>, target: &str, bounds: CallBounds) -> GatedCapture {
    let streaming = request
        .body
        .json()
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let routes = routing::resolve_target_chain(&request.state.config, target, request.router_id);
    let captured = tokio::time::timeout(bounds.gated_max_duration, async {
        // A streaming turn the live path would send through the committed
        // chain stream takes it here too, so a first route that fails before
        // its headers advances the chain rather than cutting the turn.
        let committed = if streaming {
            crate::proxy::failover::gated::committed_stream(request, &routes).await
        } else {
            None
        };
        let synthesized = committed.is_some();
        let outcome = match committed {
            Some(outcome) => outcome,
            None => {
                run_chain(ChainRequest {
                    state: request.state.clone(),
                    routes,
                    uri: request.uri,
                    base_headers: request.base_headers,
                    inbound: request.inbound,
                    body: request.body.clone(),
                    requested_model: request.requested_model,
                    router_stamp: None,
                    caller: "client",
                    // Bites only where an adapter reads a whole reply itself — the
                    // non-streaming paths. Every streaming path relays without holding
                    // the body (the Anthropic first-frame rewrite is bounded by its own
                    // 64 KiB ceiling), so the cap there falls to `bound_stream` below.
                    response_byte_cap: Some(bounds.gated_max_bytes),
                })
                .await
            }
        };
        let success = match outcome {
            Ok(success) => success,
            Err(error) => return chain_failure(error),
        };
        let gated = GatedBounds {
            max_bytes: bounds.gated_max_bytes,
            idle: bounds.gated_idle,
            max_duration: bounds.gated_max_duration,
        };
        if !success.status.is_success() {
            return relay_refusal(success, gated).await;
        }
        if streaming {
            retain_stream(success, gated, synthesized).await
        } else {
            retain_message(success, gated).await
        }
    })
    .await;
    captured.unwrap_or(GatedCapture::Cut(CutReason::Bound(BoundExceeded::Duration)))
}

/// A chain error: the byte cap biting inside an adapter is a gated bound, a
/// successful reply whose body broke after its headers is a turn cut before its
/// terminal marker — the same transport cut `retain_stream` and `collect_gated`
/// make of a broken body — and everything else is the upstream's failure to
/// relay.
fn chain_failure(error: ForwardError) -> GatedCapture {
    if error.body_too_large().is_some() {
        return GatedCapture::Cut(CutReason::Bound(BoundExceeded::MaxBytes));
    }
    if error.body_broke() {
        return GatedCapture::Cut(CutReason::Transport);
    }
    let message = error.message().to_string();
    GatedCapture::UpstreamError(UpstreamFailure::Failed {
        message,
        response: axum::response::IntoResponse::into_response(error),
    })
}

/// A non-`2xx` answer, collected under the gated cap and idle gap so libsy's
/// error carries the body and the caller can still be handed the same bytes.
/// A refusal that sends its headers and then stalls is cut at the idle gap,
/// not held until the wall-clock bound.
async fn relay_refusal(success: ChainSuccess, gated: GatedBounds) -> GatedCapture {
    let (parts, body) = success.response.into_parts();
    match collect_gated(body, gated).await {
        Ok(bytes) => GatedCapture::UpstreamError(UpstreamFailure::Answered {
            status: success.status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            response: axum::response::Response::from_parts(
                parts,
                axum::body::Body::from(Bytes::from(bytes)),
            ),
        }),
        Err(reason) => GatedCapture::Cut(reason),
    }
}

/// Collect a whole buffered body under the gated cap and idle gap.
///
/// The idle gap is measured between body chunks, and any chunk refreshes it: a
/// JSON body has no keep-alive frames to discount, so every byte is progress.
async fn collect_gated(body: axum::body::Body, gated: GatedBounds) -> Result<Vec<u8>, CutReason> {
    let mut data = body.into_data_stream();
    let mut collected = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(gated.idle, data.next()).await {
            Err(_) => return Err(CutReason::Bound(BoundExceeded::Idle)),
            Ok(None) => return Ok(collected),
            Ok(Some(Err(_))) => return Err(CutReason::Transport),
            Ok(Some(Ok(chunk))) => chunk,
        };
        if collected.len().saturating_add(chunk.len()) > gated.max_bytes {
            return Err(CutReason::Bound(BoundExceeded::MaxBytes));
        }
        collected.extend_from_slice(&chunk);
    }
}

/// Retain a streaming turn's rendered frames under [`bound_stream`].
async fn retain_stream(
    success: ChainSuccess,
    gated: GatedBounds,
    synthesized: bool,
) -> GatedCapture {
    let (parts, body) = success.response.into_parts();
    // `bound_stream` takes unwrapped chunks on purpose (its closed bound set
    // has no transport member), so a transport error ends the source here and
    // is remembered as its own reason.
    let broke = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&broke);
    let source = body.into_data_stream().scan((), move |(), item| {
        futures_util::future::ready(match item {
            Ok(chunk) => Some(chunk),
            Err(_) => {
                noted.store(true, Ordering::Relaxed);
                None
            }
        })
    });
    // The byte cap is charged here, on the turn's own bytes, rather than in
    // `bound_stream`, which counts whole chunks: the chunk that completes
    // `message_stop` can carry bytes past it that are not part of the turn.
    let uncapped = GatedBounds {
        max_bytes: usize::MAX,
        ..gated
    };
    let mut bounded = std::pin::pin!(bound_stream(source, uncapped));
    let mut retained = Vec::new();
    let mut scan = TerminalScan::default();
    while let Some(item) = bounded.next().await {
        match item {
            Ok(chunk) => {
                scan.feed(&chunk);
                // The turn ends at its terminal frame, as the live relay ends
                // it: nothing after it is replayed, and nothing after it is
                // waited for.
                let end = scan.terminal_len();
                let turn = end.map_or(chunk.len(), |end| end - retained.len());
                if retained.len().saturating_add(turn) > gated.max_bytes {
                    return GatedCapture::Cut(CutReason::Bound(BoundExceeded::MaxBytes));
                }
                retained.extend_from_slice(&chunk[..turn]);
                if end.is_some() {
                    break;
                }
            }
            Err(exceeded) => return GatedCapture::Cut(CutReason::Bound(exceeded)),
        }
    }
    let winner = parts
        .extensions
        .get::<crate::proxy::chain_stream::ChainStreamWinner>();
    // A committed chain stream answers a chain that produced no turn — it ran
    // out, or an attempt failed terminally before its headers — with a `200`
    // and one `error` frame. That is the refusal the ordered loop would have
    // returned, not a cut: relay it as one, so escalation does not bill the
    // strong tier for it and the caller keeps the refusal's status. The
    // upstream's response headers (`retry-after`) are not carried through the
    // committed stream, so the relay has none.
    if let Some((status, envelope)) = winner.and_then(|winner| winner.refusal()) {
        return GatedCapture::UpstreamError(UpstreamFailure::Answered {
            status,
            body: envelope.to_string(),
            response: axum::response::IntoResponse::into_response((status, axum::Json(envelope))),
        });
    }
    if !scan.is_terminal() {
        return GatedCapture::Cut(if broke.load(Ordering::Relaxed) {
            CutReason::Transport
        } else {
            CutReason::Nonterminal
        });
    }
    // A committed chain stream names its winner only as the body is read; the
    // ordered loop knew it when it returned.
    let (provider, model) = winner.map_or((success.provider, success.model), |winner| winner.get());
    GatedCapture::Retained(RetainedTurn {
        status: success.status,
        headers: parts.headers,
        body: Bytes::from(retained),
        streaming: true,
        provider,
        model,
        synthesized,
    })
}

/// Retain a non-streaming turn's single JSON message, collected by
/// [`collect_gated`].
async fn retain_message(success: ChainSuccess, gated: GatedBounds) -> GatedCapture {
    let (parts, body) = success.response.into_parts();
    // A Responses target synthesizes a whole-looking message from an upstream
    // that ended before `response.completed`; only this mark tells it apart.
    if parts.extensions.get::<UpstreamTruncated>().is_some() {
        return GatedCapture::Cut(CutReason::Nonterminal);
    }
    let retained = match collect_gated(body, gated).await {
        Ok(retained) => retained,
        Err(reason) => return GatedCapture::Cut(reason),
    };
    if !is_single_message(&retained) {
        return GatedCapture::Cut(CutReason::Nonterminal);
    }
    GatedCapture::Retained(RetainedTurn {
        status: success.status,
        headers: parts.headers,
        body: Bytes::from(retained),
        streaming: false,
        provider: success.provider,
        model: success.model,
        synthesized: false,
    })
}

/// The non-streaming terminal marker: the body parses as one JSON object whose
/// `type` is `message`. A truncated body does not parse, and an error body —
/// which a `2xx` can still carry on a nonconforming upstream — is `error`.
pub(crate) fn is_single_message(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| value.get("type").and_then(Value::as_str) == Some("message"))
}

/// The streaming terminal marker, read frame by frame as the stream arrives.
///
/// A turn is terminal when a **complete** frame names `message_stop` and no
/// frame before it names `error` or is the Responses adapter's
/// [`UPSTREAM_TRUNCATED_MARKER`] — the comment frame it writes ahead of the
/// `message_stop` it synthesizes when the upstream ended before
/// `response.completed`, so that marker's stop closes a cut turn, not a
/// finished one. Frame-level, under the framing rule
/// `bound_stream` uses, so a CRLF stream and a `message_stop` split across
/// chunks read the same as the unsplit LF form, and a `content_block_delta`
/// whose text happens to say `message_stop` is content rather than the marker.
/// Only the partial trailing frame is carried between chunks, so the scan
/// holds a frame's worth of bytes, not the turn.
///
/// The scan ends at the first terminal frame — `message_stop`, or an `error`
/// frame, which leaves the turn unservable — and records where in the stream
/// that frame ended: the live relay (`proxy::chain_stream`) ends the client's
/// stream at the same frame, so nothing after it is part of the turn, and a
/// turn that already failed is not held open for a bound to cut it.
#[derive(Debug, Default)]
pub(crate) struct TerminalScan {
    remainder: Vec<u8>,
    /// Stream offset of the remainder's first byte.
    offset: usize,
    /// Stream offset just past the terminal frame, once one completed.
    end: Option<usize>,
    errored: bool,
    truncated: bool,
}

impl TerminalScan {
    /// Feed the next chunk of the retained stream. Nothing after a completed
    /// terminal frame is read.
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.end.is_some() {
            return;
        }
        self.remainder.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(len) = first_frame_len(&self.remainder[consumed..]) {
            let frame = &self.remainder[consumed..consumed + len];
            consumed += len;
            // An LF-only frame — every stream shunt talks to today — is read
            // in place, as `take_complete_frames` reads it.
            let normalized: Cow<'_, [u8]> = if frame.contains(&b'\r') {
                Cow::Owned(normalize_line_endings(frame))
            } else {
                Cow::Borrowed(frame)
            };
            if normalized.trim_ascii() == UPSTREAM_TRUNCATED_MARKER {
                self.truncated = true;
                continue;
            }
            let (stop, error) = match frame_event(&String::from_utf8_lossy(&normalized)) {
                Some("message_stop") => (true, false),
                Some("error") => (false, true),
                _ => (false, false),
            };
            if stop || error {
                self.errored = error;
                self.end = Some(self.offset + consumed);
                self.remainder = Vec::new();
                return;
            }
        }
        self.remainder.drain(..consumed);
        self.offset += consumed;
    }

    /// Whether what has been fed so far is a servable, finished turn.
    pub(crate) fn is_terminal(&self) -> bool {
        self.end.is_some() && !self.errored && !self.truncated
    }

    /// How many bytes of the stream the turn is — everything through its
    /// terminal frame — once that frame has completed.
    pub(crate) fn terminal_len(&self) -> Option<usize> {
        self.end
    }
}

/// libsy's view of the capture: the promise result the algorithm buffers.
fn promise_result(capture: &GatedCapture, target: &ModelId) -> switchyard_libsy::Result<Response> {
    let client_call = |source| LibsyError::ClientCall {
        target: target.clone(),
        source,
    };
    let llm_response = match capture {
        GatedCapture::Retained(turn) if turn.streaming => {
            let bytes = turn.body.to_vec();
            let stream = switchyard_translation::decode_stream(
                futures_util::stream::once(futures_util::future::ready(Ok(bytes))),
                WireFormat::AnthropicMessages,
            )
            .map_err(client_call)?;
            LlmResponse::Stream(stream)
        }
        GatedCapture::Retained(turn) => {
            let value: Value = serde_json::from_slice(&turn.body).map_err(|error| {
                client_call(LlmClientError::InvalidResponse {
                    source: Box::new(error),
                })
            })?;
            let decoded = TranslationEngine::default()
                .decode_response(
                    WireFormat::AnthropicMessages,
                    &value,
                    &TranslationPolicy::default(),
                )
                .map_err(|error| {
                    client_call(LlmClientError::ResponseTranslation(error.to_string()))
                })?;
            LlmResponse::Agg(decoded.response)
        }
        // A stream whose only item is a transport error, not an `Err` promise:
        // upstream's escalation classifier catches `Transport` only while
        // *buffering* the weak turn (and falls back to the strong tier), while
        // an `Err` from the call itself propagates and would fail the request.
        GatedCapture::Cut(reason) => {
            let error = LlmClientError::Transport {
                source: Box::new(GatedCut(*reason)),
            };
            LlmResponse::Stream(Box::pin(futures_util::stream::iter([Err(error)])))
        }
        GatedCapture::UpstreamError(failure) => {
            let (status, body) = match failure {
                UpstreamFailure::Answered { status, body, .. } => (*status, body.clone()),
                UpstreamFailure::Failed { message, response } => {
                    (response.status(), message.clone())
                }
            };
            return Err(client_call(LlmClientError::UpstreamHttp { status, body }));
        }
    };
    Ok(Response {
        llm_response,
        metadata: None,
        // The caller's own response headers travel with the retained turn, not
        // through libsy.
        upstream_headers: HeaderMap::new(),
    })
}

#[cfg(test)]
mod tests;
