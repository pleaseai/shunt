//! Streaming-response observability without buffering or changing body bytes.
//!
//! The observer wraps only `text/event-stream` responses, forwards every body
//! chunk as soon as it is polled, and incrementally inspects complete SSE frames.
//! Parsing is capped at 256 KiB per event; oversized events are ignored until
//! their boundary while forwarding continues unchanged. Token accounting is
//! intentionally streaming-only in this first version.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    http::{header::CONTENT_TYPE, Response, StatusCode},
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;

const MAX_EVENT_BYTES: usize = 256 * 1024;

/// Bytes of an SSE `event:` name kept for the "last observed event type"
/// diagnostic (#310). Upstream-controlled text, so it lives in a fixed-size
/// inline buffer: the observer's footprint cannot grow with it, and the
/// streaming path never allocates to record it.
const MAX_LAST_EVENT_BYTES: usize = 64;

/// `source()` links [`error_chain`] walks after an upstream body error's own
/// message.
const MAX_ERROR_SOURCES: usize = 4;

/// Injected by `adapters::responses::http::stream_response` as a standalone
/// SSE comment frame immediately before a synthesized completion, whenever
/// `AnthropicSseMachine::finish` only produced output because the upstream
/// connection ended before a real terminal/error event was seen (that
/// adapter is the only caller of `finish()` on the streaming path). SSE
/// comment lines (`:`-prefixed) are ignored by every conforming client per
/// the WHATWG EventSource spec, so real clients never see this — only this
/// observer's own frame parser does, letting `outcome` classify the result
/// as `UpstreamCut` instead of `Completed` despite the well-formed
/// `message_stop` that follows it.
pub(crate) const UPSTREAM_TRUNCATED_MARKER: &[u8] = b":shunt-upstream-truncated";

/// Client-facing SSE protocol used to interpret terminal and usage events.
#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Anthropic,
    Responses,
}

/// How the observed stream ended, as seen by [`ObservedStream`]'s poll and
/// drop paths. Distinguishing the first two is the point of #310: a clean end
/// with no terminal event and a failed body read both classify as
/// [`Outcome::UpstreamCut`], but they are different faults, and only the
/// second has an error message worth reporting.
#[derive(Debug)]
enum StreamEnd {
    /// The upstream body ended (`Poll::Ready(None)`).
    Eof,
    /// Reading the upstream body failed (`Poll::Ready(Some(Err(_)))`); carries
    /// the error rendered by [`error_chain`].
    TransportError(String),
    /// The wrapper was dropped while the upstream was still open — the client
    /// hung up.
    ClientDrop,
}

impl StreamEnd {
    /// Whether the upstream, rather than the client, ended the stream. Both
    /// [`Self::Eof`] and [`Self::TransportError`] are "natural" in this sense:
    /// the observer stopped because the upstream side stopped.
    fn natural(&self) -> bool {
        !matches!(self, Self::ClientDrop)
    }
}

/// Render an upstream body error as one line: its own `Display`, then up to
/// [`MAX_ERROR_SOURCES`] `source()` messages joined by `": "`. Each layer's
/// own `Display` need not repeat its cause, so the chain is where the
/// diagnosis usually is. A cause whose message the line already ends with is
/// skipped, since a transparent wrapper — `axum::Error` around a body error is
/// exactly one — reports its source's message as its own.
/// `crate::observability` strips control characters and caps the length before
/// any of this reaches Sentry.
fn error_chain(error: &axum::Error) -> String {
    use std::fmt::Write;

    let mut rendered = error.to_string();
    let mut source = std::error::Error::source(error);
    for _ in 0..MAX_ERROR_SOURCES {
        let Some(cause) = source else { break };
        let text = cause.to_string();
        if !rendered.ends_with(&text) {
            let _ = write!(rendered, ": {text}");
        }
        source = cause.source();
    }
    rendered
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Completed,
    ErrorEvent,
    UpstreamCut,
    ClientDisconnect,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ErrorEvent => "error_event",
            Self::UpstreamCut => "upstream_cut",
            Self::ClientDisconnect => "client_disconnect",
        }
    }

    /// The [`crate::observability::StreamFailureKind`] counterpart to this
    /// outcome, for [`crate::observability::record_stream_failure`] — `None`
    /// for `Completed`/`ClientDisconnect`, which are not upstream failures (a
    /// natural end and a client hangup are not root-causeable events; see
    /// `ObserverState::finish`).
    fn as_stream_failure(self) -> Option<crate::observability::StreamFailureKind> {
        match self {
            Self::ErrorEvent => Some(crate::observability::StreamFailureKind::ErrorEvent),
            Self::UpstreamCut => Some(crate::observability::StreamFailureKind::UpstreamCut),
            Self::Completed | Self::ClientDisconnect => None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TokenUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
}

struct ObserverState {
    protocol: Protocol,
    provider: String,
    model: String,
    started_at: Instant,
    // The upstream response status the stream opened with. `finish` gates
    // `record_stream_failure` on this being 2xx: a non-2xx SSE response was
    // already recorded/captured at header time
    // (`observability::record_span_outcome` / `capture_upstream_outcome`), so
    // a mid-stream failure event on top of it would double-report the same
    // failure and contradicts the "answered 200 then failed" scope this
    // module documents (see the module docs).
    status: StatusCode,
    // Held for the lifetime of the stream so a mid-stream failure can still
    // record onto the request's own span (`crate::observability::record_stream_failure`)
    // — cloning a `tracing::Span` bumps its ref count, deferring `on_close`
    // (and the OTel/Sentry export it triggers) until this state is dropped.
    // See `crate::observability`'s module docs for why this is the only
    // reliable way to reach the span from here.
    span: tracing::Span,
    first_chunk_seen: bool,
    buffer: Vec<u8>,
    skipping_oversized: bool,
    skip_tail: [u8; 4],
    skip_tail_len: usize,
    terminal_seen: bool,
    error_seen: bool,
    /// Set once the [`UPSTREAM_TRUNCATED_MARKER`] frame is observed; forces
    /// `outcome` to classify the stream as `UpstreamCut` even though the
    /// synthesized completion that follows also sets `terminal_seen`.
    truncated_seen: bool,
    /// Complete SSE frames parsed so far, and body bytes forwarded so far —
    /// reported as Sentry event context when the stream fails (#310).
    /// `sse_events` counts every complete frame, keepalives and comment frames
    /// included; a single event larger than [`MAX_EVENT_BYTES`] is skipped
    /// past without being parsed, so it is not counted.
    sse_events: u64,
    bytes_forwarded: u64,
    /// The `event:` name of the last non-keepalive frame parsed, in a
    /// fixed-size inline buffer (see [`MAX_LAST_EVENT_BYTES`]).
    /// `last_event_len == 0` means none was seen.
    last_event: [u8; MAX_LAST_EVENT_BYTES],
    last_event_len: usize,
    /// Milliseconds from `started_at` to the first body chunk, recorded
    /// alongside the `shunt.ttft` histogram.
    ttft_ms: Option<u64>,
    tokens: TokenUsage,
    finished: bool,
}

impl ObserverState {
    fn new(
        protocol: Protocol,
        status: StatusCode,
        provider: String,
        model: String,
        started_at: Instant,
        span: tracing::Span,
    ) -> Self {
        Self {
            protocol,
            provider,
            model,
            started_at,
            status,
            span,
            first_chunk_seen: false,
            buffer: Vec::with_capacity(4096),
            skipping_oversized: false,
            skip_tail: [0; 4],
            skip_tail_len: 0,
            terminal_seen: false,
            error_seen: false,
            truncated_seen: false,
            sse_events: 0,
            bytes_forwarded: 0,
            last_event: [0; MAX_LAST_EVENT_BYTES],
            last_event_len: 0,
            ttft_ms: None,
            tokens: TokenUsage::default(),
            finished: false,
        }
    }

    fn observe_chunk(&mut self, chunk: &[u8]) {
        if !self.first_chunk_seen {
            self.first_chunk_seen = true;
            let ttft = self.started_at.elapsed();
            crate::metrics::record_ttft(&self.provider, &self.model, ttft.as_secs_f64() * 1000.0);
            self.ttft_ms = Some(millis(ttft));
        }
        self.bytes_forwarded = self.bytes_forwarded.saturating_add(chunk.len() as u64);
        self.push_bytes(chunk);
    }

    fn push_bytes(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.skipping_oversized {
                let consumed = self.skip_to_boundary(bytes);
                bytes = &bytes[consumed..];
                continue;
            }

            let room = MAX_EVENT_BYTES.saturating_sub(self.buffer.len());
            let take = room.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            self.parse_complete_frames();

            if self.buffer.len() == MAX_EVENT_BYTES && find_boundary(&self.buffer).is_none() {
                self.begin_oversized_skip();
            }
        }
    }

    fn parse_complete_frames(&mut self) {
        while let Some((boundary, delimiter_len)) = find_boundary(&self.buffer) {
            let end = boundary + delimiter_len;
            let (observation, event) = observe_frame(self.protocol, &self.buffer[..boundary]);
            // Copied out before anything else touches `self`: `event` borrows
            // the buffer this loop is about to drain.
            let last_event = event.map(inline_event_name);
            self.sse_events = self.sse_events.saturating_add(1);
            self.terminal_seen |= observation.terminal;
            self.error_seen |= observation.error;
            self.truncated_seen |= observation.truncated;
            if let Some((name, len)) = last_event {
                self.last_event = name;
                self.last_event_len = len;
            }
            merge_tokens(&mut self.tokens, observation.tokens);
            self.buffer.drain(..end);
        }
    }

    fn begin_oversized_skip(&mut self) {
        let retained = self.buffer.len().min(4);
        self.skip_tail_len = retained;
        self.skip_tail[..retained].copy_from_slice(&self.buffer[self.buffer.len() - retained..]);
        self.buffer.clear();
        self.skipping_oversized = true;
    }

    /// Consume bytes through the first boundary. The tiny byte loop is used only
    /// after a single event has exceeded the safety cap, never on the hot path.
    fn skip_to_boundary(&mut self, bytes: &[u8]) -> usize {
        for (index, &byte) in bytes.iter().enumerate() {
            if self.skip_tail_len < 4 {
                self.skip_tail[self.skip_tail_len] = byte;
                self.skip_tail_len += 1;
            } else {
                self.skip_tail.copy_within(1.., 0);
                self.skip_tail[3] = byte;
            }
            let tail = &self.skip_tail[..self.skip_tail_len];
            if tail.ends_with(b"\n\n") || tail.ends_with(b"\r\n\r\n") {
                self.skipping_oversized = false;
                self.skip_tail_len = 0;
                return index + 1;
            }
        }
        bytes.len()
    }

    fn outcome(&self, natural_end: bool) -> Outcome {
        if self.error_seen {
            Outcome::ErrorEvent
        } else if self.truncated_seen {
            // Checked ahead of `terminal_seen`: the adapter-synthesized
            // completion that follows the marker also sets `terminal_seen`,
            // but the marker's presence means this "terminal" event was
            // manufactured to keep the client stream well-formed after a
            // real upstream cut, not a genuine backend completion.
            Outcome::UpstreamCut
        } else if self.terminal_seen {
            Outcome::Completed
        } else if natural_end {
            Outcome::UpstreamCut
        } else {
            Outcome::ClientDisconnect
        }
    }

    /// Which flavor of cut this was, for the `cut_kind` Sentry tag. Only
    /// meaningful once [`Self::outcome`] has already classified the stream as
    /// [`Outcome::UpstreamCut`].
    ///
    /// The marker takes precedence for the same reason it does in `outcome`:
    /// when the adapter injected it, it had already detected the real cut and
    /// manufactured the terminal event that followed, so however *this*
    /// observer's copy of the stream ended is not the interesting fact.
    fn cut_kind(&self, end: &StreamEnd) -> crate::observability::CutKind {
        use crate::observability::CutKind;
        if self.truncated_seen {
            CutKind::Marker
        } else if matches!(end, StreamEnd::TransportError(_)) {
            CutKind::TransportError
        } else {
            CutKind::Eof
        }
    }

    /// Snapshot of what the observer saw, for the Sentry event (#310). Built
    /// on the failure path only, so a healthy stream never pays for it.
    fn failure_context(
        &self,
        failure: crate::observability::StreamFailureKind,
        end: &StreamEnd,
    ) -> crate::observability::StreamFailureContext {
        use crate::observability::{StreamFailureContext, StreamFailureKind};
        StreamFailureContext {
            cut_kind: matches!(failure, StreamFailureKind::UpstreamCut).then(|| self.cut_kind(end)),
            upstream_error: match end {
                StreamEnd::TransportError(error) => Some(error.clone()),
                StreamEnd::Eof | StreamEnd::ClientDrop => None,
            },
            sse_events: self.sse_events,
            bytes_forwarded: self.bytes_forwarded,
            last_event_type: (self.last_event_len > 0).then(|| {
                // Lossy because the inline buffer truncates at a byte, not a
                // `char`, boundary.
                String::from_utf8_lossy(&self.last_event[..self.last_event_len]).into_owned()
            }),
            elapsed_ms: millis(self.started_at.elapsed()),
            ttft_ms: self.ttft_ms,
        }
    }

    fn finish(&mut self, end: StreamEnd) {
        if self.finished {
            return;
        }
        self.finished = true;
        let outcome = self.outcome(end.natural());
        crate::metrics::record_stream_outcome(&self.provider, &self.model, outcome.as_str());
        // Only a stream that actually opened `200` can have "failed mid-stream"
        // in the sense this reports: a non-2xx response was already recorded
        // at header time (`record_span_outcome` / `capture_upstream_outcome`),
        // so reporting again here would double the Sentry signal for the same
        // upstream failure (see the `status` field doc comment).
        if self.status.is_success() {
            if let Some(failure) = outcome.as_stream_failure() {
                crate::observability::record_stream_failure(
                    &self.span,
                    &self.provider,
                    &self.model,
                    failure,
                    &self.failure_context(failure, &end),
                );
            }
        }
        for (kind, count) in [
            ("input", self.tokens.input),
            ("output", self.tokens.output),
            ("cache_read", self.tokens.cache_read),
            ("cache_creation", self.tokens.cache_creation),
        ] {
            if let Some(count) = count {
                crate::metrics::record_stream_tokens(&self.provider, &self.model, kind, count);
            }
        }
    }
}

#[derive(Default)]
struct FrameObservation {
    terminal: bool,
    error: bool,
    /// Set only for the [`UPSTREAM_TRUNCATED_MARKER`] comment frame.
    truncated: bool,
    tokens: TokenUsage,
}

/// Observe one complete SSE frame. The second element is the frame's `event:`
/// name, for the "last observed event type" diagnostic (#310) — `None` for a
/// keepalive or the truncation marker, so the reported type stays on the last
/// content-bearing frame rather than drifting onto a ping.
fn observe_frame(protocol: Protocol, frame: &[u8]) -> (FrameObservation, Option<&[u8]>) {
    if frame == UPSTREAM_TRUNCATED_MARKER {
        return (
            FrameObservation {
                truncated: true,
                ..Default::default()
            },
            None,
        );
    }

    let (event, data) = event_and_data(frame);
    if event == Some(b"ping") || data == Some(b"{\"type\": \"ping\"}") {
        return (FrameObservation::default(), None);
    }

    let observation = match protocol {
        Protocol::Anthropic => observe_anthropic(event, data),
        Protocol::Responses => observe_responses(event, data),
    };
    (observation, event)
}

/// Copy an SSE event name into a fixed-size buffer, dropping anything past
/// [`MAX_LAST_EVENT_BYTES`]. Returns the buffer and how much of it is used.
fn inline_event_name(event: &[u8]) -> ([u8; MAX_LAST_EVENT_BYTES], usize) {
    let len = event.len().min(MAX_LAST_EVENT_BYTES);
    let mut name = [0; MAX_LAST_EVENT_BYTES];
    name[..len].copy_from_slice(&event[..len]);
    (name, len)
}

/// Whole milliseconds, saturating rather than wrapping for an
/// implausibly long-lived stream.
fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn observe_anthropic(event: Option<&[u8]>, data: Option<&[u8]>) -> FrameObservation {
    if event == Some(b"error") {
        return FrameObservation {
            error: true,
            ..Default::default()
        };
    }
    if event == Some(b"message_stop") {
        return FrameObservation {
            terminal: true,
            ..Default::default()
        };
    }
    if !matches!(event, Some(b"message_start") | Some(b"message_delta")) {
        return FrameObservation::default();
    }
    let Some(value) = data.and_then(|data| serde_json::from_slice::<Value>(data).ok()) else {
        return FrameObservation::default();
    };
    let usage = if event == Some(b"message_start") {
        value.pointer("/message/usage")
    } else {
        value.get("usage")
    };
    let mut tokens = TokenUsage::default();
    if let Some(usage) = usage {
        update_tokens(&mut tokens, usage, true);
    }
    FrameObservation {
        tokens,
        ..Default::default()
    }
}

fn observe_responses(event: Option<&[u8]>, data: Option<&[u8]>) -> FrameObservation {
    if data == Some(b"[DONE]") {
        return FrameObservation {
            terminal: true,
            ..Default::default()
        };
    }
    // Mirrors the Responses translator's own error handling
    // (`model::responses::AnthropicSseMachine::apply`, `"error" | "response.failed"`):
    // both event names terminate the backend stream with an error.
    if matches!(event, Some(b"error") | Some(b"response.failed")) {
        return FrameObservation {
            error: true,
            ..Default::default()
        };
    }
    // Mirrors the translator's terminal handling (`"response.completed" |
    // "response.done"`, see also `docs/m1-responses-translation.md`): both
    // names carry the full `response` + `usage` and end the stream normally.
    // `response.incomplete` is terminal too, matching the terminal set the
    // WebSocket transport already uses
    // (`adapters::responses::codex_ws::TERMINAL_EVENTS`,
    // `docs/m7-codex-websocket.md`): the backend explicitly concluded the
    // stream, just with truncated content, not a transport cut — classifying
    // it as `UpstreamCut` would misreport a genuine (if incomplete) response
    // as a mid-stream failure.
    if !matches!(
        event,
        Some(b"response.completed") | Some(b"response.done") | Some(b"response.incomplete")
    ) {
        return FrameObservation::default();
    }
    let mut tokens = TokenUsage::default();
    if let Some(usage) = data
        .and_then(|data| serde_json::from_slice::<Value>(data).ok())
        .and_then(|value| value.pointer("/response/usage").cloned())
    {
        update_tokens(&mut tokens, &usage, false);
    }
    FrameObservation {
        terminal: true,
        tokens,
        ..Default::default()
    }
}

fn merge_tokens(target: &mut TokenUsage, observed: TokenUsage) {
    for (target, observed) in [
        (&mut target.input, observed.input),
        (&mut target.output, observed.output),
        (&mut target.cache_read, observed.cache_read),
        (&mut target.cache_creation, observed.cache_creation),
    ] {
        if observed.is_some() {
            *target = observed;
        }
    }
}

struct ObservedStream {
    upstream: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    state: ObserverState,
}

impl Stream for ObservedStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.upstream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.state.observe_chunk(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                // The error is forwarded to the client unchanged; its message
                // is also recorded, since a failed body read and a clean end
                // with no terminal event are indistinguishable downstream
                // (#310).
                self.state
                    .finish(StreamEnd::TransportError(error_chain(&error)));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.state.finish(StreamEnd::Eof);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        self.state.finish(StreamEnd::ClientDrop);
    }
}

/// Wrap an SSE response body in the streaming observer. Non-SSE responses are
/// returned untouched. Response headers and body bytes are preserved.
pub fn observe_response(
    response: Response<Body>,
    protocol: Protocol,
    provider: String,
    model: String,
    started_at: Instant,
) -> Response<Body> {
    if !is_sse(&response) {
        return response;
    }
    let status = response.status();
    // Captured here, synchronously inside the caller's `.instrument(span)`
    // future (`proxy::post` / `codex_endpoint::post`), so this resolves to
    // the request's own span — by the time the stream is actually polled,
    // that future has already returned and `tracing::Span::current()` would
    // no longer find it. See `crate::observability`'s module docs.
    let span = tracing::Span::current();
    let (parts, body) = response.into_parts();
    let observed = ObservedStream {
        upstream: body.into_data_stream().boxed(),
        state: ObserverState::new(protocol, status, provider, model, started_at, span),
    };
    Response::from_parts(parts, Body::from_stream(observed))
}

fn is_sse(response: &Response<Body>) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

fn find_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn event_and_data(frame: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
    let mut event = None;
    let mut data = None;
    for raw_line in frame.split(|&byte| byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if let Some(value) = line.strip_prefix(b"event:") {
            event = Some(value.strip_prefix(b" ").unwrap_or(value));
        } else if data.is_none() {
            if let Some(value) = line.strip_prefix(b"data:") {
                data = Some(value.strip_prefix(b" ").unwrap_or(value));
            }
        }
    }
    (event, data)
}

fn update_tokens(tokens: &mut TokenUsage, usage: &Value, anthropic: bool) {
    set_u64(&mut tokens.input, usage.get("input_tokens"));
    set_u64(&mut tokens.output, usage.get("output_tokens"));
    if anthropic {
        set_u64(&mut tokens.cache_read, usage.get("cache_read_input_tokens"));
        set_u64(
            &mut tokens.cache_creation,
            usage.get("cache_creation_input_tokens"),
        );
    } else {
        set_u64(
            &mut tokens.cache_read,
            usage.pointer("/input_tokens_details/cached_tokens"),
        );
    }
}

fn set_u64(target: &mut Option<u64>, value: Option<&Value>) {
    if let Some(value) = value.and_then(Value::as_u64) {
        *target = Some(value);
    }
}

#[cfg(test)]
mod tests;
