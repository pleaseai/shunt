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
    http::{header::CONTENT_TYPE, Response},
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;

const MAX_EVENT_BYTES: usize = 256 * 1024;

/// Client-facing SSE protocol used to interpret terminal and usage events.
#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Anthropic,
    Responses,
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
    first_chunk_seen: bool,
    buffer: Vec<u8>,
    skipping_oversized: bool,
    skip_tail: [u8; 4],
    skip_tail_len: usize,
    terminal_seen: bool,
    error_seen: bool,
    tokens: TokenUsage,
    finished: bool,
}

impl ObserverState {
    fn new(protocol: Protocol, provider: String, model: String, started_at: Instant) -> Self {
        Self {
            protocol,
            provider,
            model,
            started_at,
            first_chunk_seen: false,
            buffer: Vec::with_capacity(4096),
            skipping_oversized: false,
            skip_tail: [0; 4],
            skip_tail_len: 0,
            terminal_seen: false,
            error_seen: false,
            tokens: TokenUsage::default(),
            finished: false,
        }
    }

    fn observe_chunk(&mut self, chunk: &[u8]) {
        if !self.first_chunk_seen {
            self.first_chunk_seen = true;
            crate::metrics::record_ttft(
                &self.provider,
                &self.model,
                self.started_at.elapsed().as_secs_f64() * 1000.0,
            );
        }
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
            let observation = observe_frame(self.protocol, &self.buffer[..boundary]);
            self.terminal_seen |= observation.terminal;
            self.error_seen |= observation.error;
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
        } else if self.terminal_seen {
            Outcome::Completed
        } else if natural_end {
            Outcome::UpstreamCut
        } else {
            Outcome::ClientDisconnect
        }
    }

    fn finish(&mut self, natural_end: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        crate::metrics::record_stream_outcome(
            &self.provider,
            &self.model,
            self.outcome(natural_end).as_str(),
        );
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
    tokens: TokenUsage,
}

fn observe_frame(protocol: Protocol, frame: &[u8]) -> FrameObservation {
    let (event, data) = event_and_data(frame);
    if event == Some(b"ping") || data == Some(b"{\"type\": \"ping\"}") {
        return FrameObservation::default();
    }

    match protocol {
        Protocol::Anthropic => observe_anthropic(event, data),
        Protocol::Responses => observe_responses(event, data),
    }
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
    if event == Some(b"response.failed") {
        return FrameObservation {
            error: true,
            ..Default::default()
        };
    }
    if event != Some(b"response.completed") {
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
                self.state.finish(true);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.state.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        self.state.finish(false);
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
    let (parts, body) = response.into_parts();
    let observed = ObservedStream {
        upstream: body.into_data_stream().boxed(),
        state: ObserverState::new(protocol, provider, model, started_at),
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
