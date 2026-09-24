//! SSE framing for the early-commit streaming paths: the byte-stream parser
//! and the frame-buffer that turns it into [`ResponseEvent`]s. Split out of
//! `early_stream` to keep that module's transport orchestration under the
//! 500-line guideline.

use std::convert::Infallible;

use axum::body::Bytes;
use axum::http::StatusCode;
use futures_util::{stream, Stream, StreamExt, TryStreamExt};
use serde_json::Value;

use crate::model::responses::{sse, AnthropicSseMachine, ResponseEvent};
use crate::proxy::chain_stream::{LazyEnvelope, RelayBuild};

use super::error::{adapter_error_envelope, own_error, transport_error};

/// upstream data.
pub(super) fn parsed_events(
    bytes: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static {
    stream::unfold(
        (
            Box::pin(bytes),
            SseParser::default(),
            std::collections::VecDeque::<Result<ResponseEvent, Value>>::new(),
            false,
        ),
        |(mut bytes, mut parser, mut pending, mut done)| async move {
            if done {
                return None;
            }
            match next_parsed(&mut parser, &mut pending, &mut bytes).await {
                Some(item) => {
                    // A terminal item ends the stream: a consumer that polls
                    // past the error must not resume relaying upstream events
                    // behind it.
                    done = item.is_err();
                    Some((item, (bytes, parser, pending, done)))
                }
                None => None,
            }
        },
    )
}

/// One step of the shared byte→event pump behind [`parsed_events`] and the
/// single-route committed stream: drain a pending item first, then pull the
/// next transport chunk and parse it. A malformed frame or transport error
/// becomes the terminal error envelope. `None` means the upstream ended.
pub(super) async fn next_parsed<S>(
    parser: &mut SseParser,
    pending: &mut std::collections::VecDeque<Result<ResponseEvent, Value>>,
    bytes: &mut S,
) -> Option<Result<ResponseEvent, Value>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    loop {
        if let Some(item) = pending.pop_front() {
            return Some(item);
        }
        match bytes.next().await {
            Some(Ok(chunk)) => {
                let (events, malformed) = parser.push(&chunk);
                pending.extend(events.into_iter().map(Ok));
                if malformed {
                    pending.push_back(Err(malformed_frame_envelope().await));
                }
            }
            Some(Err(error)) => {
                let envelope =
                    adapter_error_envelope(transport_error(error.without_url().to_string())).await;
                return Some(Err(envelope));
            }
            None => return None,
        }
    }
}

/// The terminal envelope for an upstream frame whose `data` is present but not
/// valid JSON: a post-acceptance gateway failure (`own_error`, so the chain
/// never replays the turn) surfaced as one SSE `error` event.
async fn malformed_frame_envelope() -> Value {
    adapter_error_envelope(own_error(
        "upstream sent an SSE frame whose data is not valid JSON".to_string(),
    ))
    .await
}

/// Await a spawned tiktoken estimate under a wall-clock budget: the synthetic
/// `message_start` commits the response before any upstream byte and must not
/// wait on the estimator, so a saturated blocking pool or a pathological
/// input cannot delay the commit. `0` is a valid seed when the budget elapses
/// or the task failed.
pub(super) async fn bounded_input_estimate(
    handle: tokio::task::JoinHandle<u64>,
    budget: std::time::Duration,
) -> u64 {
    tokio::time::timeout(budget, handle)
        .await
        .unwrap_or_else(|_| Ok(0))
        .unwrap_or(0)
}

/// Translate parsed upstream events through the [`AnthropicSseMachine`] into
/// Anthropic SSE bytes. A producer error envelope becomes an SSE `error` event
/// and ends the stream; a producer that ends before a terminal event gets the
/// synthesized completion prefixed with the upstream-cut marker
/// (`stream_metrics::UPSTREAM_TRUNCATED_MARKER`), exactly like the
/// pre-early-commit relay.
pub(super) fn translated_stream(
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
    machine: AnthropicSseMachine,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    translated_core(
        events,
        move || Box::pin(async move { (machine, String::new()) }),
        |event, machine| machine.apply(event).into_iter().collect::<String>(),
    )
}

/// One item of a pooled streaming turn: the winning account's attribution
/// frame, or a relayed Responses event.
pub(super) enum PoolEvent {
    /// The account that won the turn, emitted once before its first relayed
    /// frame. Replaces the `x-shunt-account` response header on the
    /// early-commit path, where headers go out before the winner is known.
    Account(String),
    Event(ResponseEvent),
}

/// One item of a pooled streaming turn: the winning account's attribution
/// frame, a relayed Responses event, or the classified pre-frame pool
/// exhaustion (the committed chain advances on it like the pre-commit loop,
/// §4).
pub(super) enum PoolItem {
    Event(PoolEvent),
    Exhausted {
        status: StatusCode,
        advance: bool,
        remember: bool,
        envelope: LazyEnvelope,
    },
}

/// Translate pooled items into client-facing bytes: `Account` becomes an
/// `event: account` frame whose data is the bare account name (mirroring the
/// `x-shunt-account` header value), `Event` goes through the machine, and a
/// pre-frame exhaustion becomes the terminal `error` event.
pub(super) fn pool_translated_stream(
    events: impl Stream<Item = Result<PoolItem, Value>> + Send + 'static,
    machine: impl FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (AnthropicSseMachine, String)> + Send>,
        > + Send
        + 'static,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    translated_core(
        events.then(|item| async move {
            match item {
                Ok(PoolItem::Event(event)) => Ok(event),
                Ok(PoolItem::Exhausted { envelope, .. }) => Err(envelope.resolve().await),
                Err(envelope) => Err(envelope),
            }
        }),
        machine,
        |item, machine| match item {
            PoolEvent::Account(name) => sse("account", &Value::String(name)),
            PoolEvent::Event(event) => machine.apply(event).into_iter().collect::<String>(),
        },
    )
}

/// A pooled chain attempt's machine build, raced against the pool's first
/// poll ([`pooled_first_poll`]): resolved when the build won the race, still
/// pending when the pool's first item won it.
pub(super) enum MachineBuild {
    Ready(Box<AnthropicSseMachine>, Vec<String>),
    Pending(
        std::pin::Pin<
            Box<dyn std::future::Future<Output = (AnthropicSseMachine, Vec<String>)> + Send>,
        >,
    ),
}

/// The pooled chain attempt's first poll, raced against its machine build.
/// `headers_at` is the first item's arrival instant — never a build-completion
/// instant — for the chain's header-latency sample.
pub(super) struct PoolFirstPoll<E> {
    pub(super) build: MachineBuild,
    pub(super) item: Option<Result<PoolItem, Value>>,
    pub(super) events: std::pin::Pin<Box<E>>,
    pub(super) headers_at: std::time::Instant,
}

/// Race the pooled chain attempt's machine build (which awaits the bounded
/// token estimate) against the pool's first poll so account admission,
/// credential resolution, and the upstream request overlap the estimate
/// instead of serializing behind it — the pooled-branch counterpart of
/// [`translated_core`]'s leading race. The pool's first item, when it wins,
/// is buffered, and only the winner arm consumes the build, so a pre-frame
/// failure never waits on the estimate.
pub(super) async fn pooled_first_poll<E>(
    mut events: std::pin::Pin<Box<E>>,
    build: impl std::future::Future<Output = (AnthropicSseMachine, Vec<String>)> + Send + 'static,
) -> PoolFirstPoll<E>
where
    E: Stream<Item = Result<PoolItem, Value>> + Send,
{
    let first = async move {
        let item = events.next().await;
        (item, events)
    };
    match futures_util::future::select(Box::pin(build), Box::pin(first)).await {
        futures_util::future::Either::Left(((machine, start), first)) => {
            let (item, events) = first.await;
            PoolFirstPoll {
                build: MachineBuild::Ready(Box::new(machine), start),
                item,
                events,
                headers_at: std::time::Instant::now(),
            }
        }
        futures_util::future::Either::Right(((item, events), build)) => PoolFirstPoll {
            build: MachineBuild::Pending(Box::pin(build)),
            item,
            events,
            headers_at: std::time::Instant::now(),
        },
    }
}

/// Build a pooled winner's relay from the raced machine build: `Ready`
/// constructs the relay now, `Pending` defers only the machine and the
/// synthetic start — the chain records the winner (attribution slots,
/// requests sample, span outcome) before awaiting the pending relay, so a
/// client disconnect during that await can no longer drop the attribution.
/// The buffered first item (the winning account's attribution frame) stays
/// the first relayed frame either way.
pub(super) fn pool_relay_build<E>(
    build: MachineBuild,
    event: PoolEvent,
    events: std::pin::Pin<Box<E>>,
) -> RelayBuild
where
    E: Stream<Item = Result<PoolItem, Value>> + Send + 'static,
{
    match build {
        MachineBuild::Ready(machine, start) => {
            let machine = *machine;
            RelayBuild::Ready {
                start: Some(Bytes::from(start.join(""))),
                frames: Box::pin(
                    pool_translated_stream(
                        futures_util::stream::iter([Ok(PoolItem::Event(event))]).chain(events),
                        move || Box::pin(async move { (machine, String::new()) }),
                    )
                    .map_err(|never| match never {}),
                ),
            }
        }
        MachineBuild::Pending(pending) => RelayBuild::Pending(Box::pin(async move {
            let (machine, start) = pending.await;
            let frames: crate::proxy::chain_stream::ClientFrames = Box::pin(
                pool_translated_stream(
                    futures_util::stream::iter([Ok(PoolItem::Event(event))]).chain(events),
                    move || Box::pin(async move { (machine, String::new()) }),
                )
                .map_err(|never| match never {}),
            );
            (Bytes::from(start.join("")), frames)
        })),
    }
}

/// How long the relay keeps reading a still-open upstream after a terminal
/// event: the upstream's EOF must be read so hyper pools the connection, but
/// nothing past the terminal may ever be forwarded. The drain runs in a
/// detached task after the outward stream has ended, so the client never
/// waits on it and the keepalive wrapper can never inject a ping past the
/// terminal while it runs.
const TERMINAL_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Detach the post-terminal drain: after the terminal frame the outward
/// stream ends, while this task keeps reading the still-open upstream under
/// the same budget so hyper can pool the connection. Detached because an
/// in-stream drain made the poll after the terminal frame stall — long
/// enough, with a short keepalive interval, for the surrounding `with_pings`
/// wrapper to inject a client-visible `ping` past the terminal. Shared with
/// the chain's raw Anthropic-kind relay (`proxy::chain_stream`), whose
/// frames stream has the same item shape.
pub(crate) fn spawn_terminal_drain<I>(
    events: std::pin::Pin<Box<dyn Stream<Item = Result<I, Value>> + Send>>,
) where
    I: Send + 'static,
{
    tokio::spawn(async move {
        let mut events = events;
        let _ = tokio::time::timeout(TERMINAL_DRAIN_BUDGET, async {
            while events.next().await.is_some() {}
        })
        .await;
    });
}

/// The shared translation loop behind [`translated_stream`] and
/// [`pool_translated_stream`]: a producer error envelope becomes an SSE
/// `error` event and ends the stream; a producer that ends before a terminal
/// event gets the synthesized completion prefixed with the upstream-cut
/// marker (`stream_metrics::UPSTREAM_TRUNCATED_MARKER`), exactly like the
/// pre-early-commit relay.
pub(super) fn translated_core<I, F, M>(
    events: impl Stream<Item = Result<I, Value>> + Send + 'static,
    machine: M,
    map: F,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static
where
    I: Send + 'static,
    F: Fn(I, &mut AnthropicSseMachine) -> String + Send + 'static,
    M: FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (AnthropicSseMachine, String)> + Send>,
        > + Send
        + 'static,
{
    type Events<I> = std::pin::Pin<Box<dyn Stream<Item = Result<I, Value>> + Send>>;
    type FirstPoll<I> = (Option<Result<I, Value>>, Events<I>);

    /// The producer is polled live, or its first poll is in flight (the
    /// machine build won the race) or already resolved (the producer won it);
    /// either way the first-poll future hands the stream back. `Done` means
    /// the terminal frame was yielded and the post-terminal drain owns the
    /// producer; the outward stream ends at the next poll.
    enum Producer<I> {
        Live(Events<I>),
        First(std::pin::Pin<Box<dyn std::future::Future<Output = FirstPoll<I>> + Send>>),
        Done,
    }

    stream::unfold(
        (
            Producer::Live(Box::pin(events)),
            None::<AnthropicSseMachine>,
            false,
            map,
            Some(machine),
        ),
        move |(mut producer, mut machine, mut finished, map, mut factory)| {
            async move {
                if finished {
                    // The terminal frame already yielded: the outward stream
                    // ends here. The post-terminal drain runs detached (see
                    // the terminal arm), so this poll returns immediately
                    // and the keepalive wrapper can never inject a frame
                    // past the terminal while the drain is still waiting.
                    return None;
                }
                // The leading phase: race the machine build against the
                // producer's first poll so the upstream dispatch overlaps the
                // bounded estimate instead of serializing behind it (the
                // factory may wait for the estimate; keepalive pings cover
                // the wait), then emit the leading bytes, the synthetic
                // start, before any relayed frame. The producer's item, when
                // it won the race, is buffered until after the start.
                if let Some(factory) = factory.take() {
                    let Producer::Live(mut events) = producer else {
                        unreachable!("the leading phase runs once");
                    };
                    let build = factory();
                    let first = async move {
                        let item = events.next().await;
                        (item, events)
                    };
                    let build = Box::pin(build);
                    let first = Box::pin(first);
                    let (built, leading, producer) =
                        match futures_util::future::select(build, first).await {
                            futures_util::future::Either::Left(((built, leading), first)) => {
                                (built, leading, Producer::First(first))
                            }
                            futures_util::future::Either::Right(((item, events), build)) => {
                                let (built, leading) = build.await;
                                (
                                    built,
                                    leading,
                                    Producer::First(Box::pin(async move { (item, events) })),
                                )
                            }
                        };
                    return Some((
                        Ok(Bytes::from(leading)),
                        (producer, Some(built), false, map, None),
                    ));
                }
                loop {
                    let mut active = machine.take().expect("machine factory ran");
                    let item = match producer {
                        Producer::Live(mut events) => {
                            let item = events.next().await;
                            producer = Producer::Live(events);
                            item
                        }
                        Producer::First(pending) => {
                            let (item, events) = pending.await;
                            producer = Producer::Live(events);
                            item
                        }
                        Producer::Done => {
                            unreachable!("Done is set only with `finished`, which returns above");
                        }
                    };
                    match item {
                        Some(Ok(item)) => {
                            let data = map(item, &mut active);
                            if !data.is_empty() {
                                // A terminal event ends the relay even when
                                // the upstream keeps the connection open: no
                                // further frame can produce client-visible
                                // output. The drain for the still-open
                                // upstream (connection pooling) is detached
                                // here, so the next poll ends the outward
                                // stream immediately.
                                let finished = active.is_stopped();
                                if finished {
                                    let Producer::Live(events) = producer else {
                                        unreachable!(
                                            "a terminal item implies the producer is live"
                                        );
                                    };
                                    // An emulated stop sequence (issue #605)
                                    // means the upstream is still mid-turn and
                                    // will keep generating text nobody will
                                    // see: drop `events` outright so the byte
                                    // stream is dropped and the upstream
                                    // connection is aborted. Any other
                                    // terminal (a real `response.completed`/
                                    // `done`/`incomplete`, or a backend
                                    // `error`/`response.failed`) means the
                                    // upstream already finished, so keep
                                    // draining it to EOF here so the
                                    // connection still pools — aborting on an
                                    // ordinary terminal would cost every
                                    // normal turn its pooled connection.
                                    if active.hit_stop_sequence() {
                                        drop(events);
                                    } else {
                                        spawn_terminal_drain(events);
                                    }
                                    return Some((
                                        Ok(Bytes::from(data)),
                                        (Producer::Done, Some(active), finished, map, None),
                                    ));
                                }
                                return Some((
                                    Ok(Bytes::from(data)),
                                    (producer, Some(active), false, map, None),
                                ));
                            }
                            if active.is_stopped() {
                                // Unreachable for the current maps (every
                                // stopping event emits); drain for pooling
                                // and end, defensively — except an emulated
                                // stop sequence, which drops the events the
                                // same way the reachable arm above does, so
                                // the upstream is still aborted rather than
                                // drained.
                                let Producer::Live(events) = producer else {
                                    unreachable!("a live producer precedes its item");
                                };
                                if active.hit_stop_sequence() {
                                    drop(events);
                                } else {
                                    spawn_terminal_drain(events);
                                }
                                return None;
                            }
                            machine = Some(active);
                            continue;
                        }
                        Some(Err(envelope)) => {
                            // A producer error after a terminal event must not
                            // append an `error` frame to a completed turn: the
                            // upstream already finished cleanly, the transport
                            // merely misbehaved afterwards. End the stream.
                            if active.is_stopped() {
                                return None;
                            }
                            return Some((
                                Ok(Bytes::from(sse("error", &envelope))),
                                (producer, Some(active), true, map, None),
                            ));
                        }
                        None => {
                            let data = active.finish().join("");
                            finished = true;
                            if data.is_empty() {
                                return None;
                            }
                            let mut marked = Vec::with_capacity(
                                crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER.len()
                                    + 2
                                    + data.len(),
                            );
                            marked.extend_from_slice(
                                crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER,
                            );
                            marked.extend_from_slice(b"\n\n");
                            marked.extend_from_slice(data.as_bytes());
                            return Some((
                                Ok(Bytes::from(marked)),
                                (producer, Some(active), finished, map, None),
                            ));
                        }
                    }
                }
            }
        },
    )
}

/// Frame-buffers the upstream SSE byte stream. Buffering raw bytes — rather than
/// decoding each transport chunk with `from_utf8_lossy` — keeps a multi-byte
/// UTF-8 code point intact when it straddles a chunk boundary: the incomplete
/// trailing bytes stay in the buffer until the next chunk completes them. Frame
/// boundaries are the ASCII `\n\n` or `\r\n\r\n` (the SSE spec permits CRLF
/// line endings, and `model_rewrite.rs` accepts both for the same reason);
/// neither can fall inside a multi-byte sequence, so every extracted frame is
/// already complete UTF-8. CRLF frames are normalized to LF before parsing.
#[derive(Default)]
pub(super) struct SseParser {
    buffer: Vec<u8>,
    scan_from: usize,
}

impl SseParser {
    /// Feed one transport chunk. Returns every event the chunk completed, plus
    /// whether a complete frame carried data that is not valid JSON: the stream
    /// then ends with a terminal SSE `error` event instead of relaying a
    /// synthesized completion over a corrupted upstream.
    pub(super) fn push(&mut self, chunk: &[u8]) -> (Vec<ResponseEvent>, bool) {
        self.buffer.extend_from_slice(chunk);

        // One scan collects every complete frame's content end and terminator
        // length, so the valid frames in front of an invalid one are decoded
        // and relayed before the stream flags the malformed frame.
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut scan = self.scan_from;
        while scan < self.buffer.len() {
            if self.buffer[scan..].starts_with(b"\n\n") {
                frames.push((scan, 2));
                scan += 2;
            } else if self.buffer[scan..].starts_with(b"\r\n\r\n") {
                frames.push((scan, 4));
                scan += 4;
            } else {
                scan += 1;
            }
        }

        if frames.is_empty() {
            // The final bytes may be a prefix of a frame terminator, so scan
            // them again after the next chunk arrives. Everything before has
            // already been ruled out.
            self.scan_from = self.buffer.len().saturating_sub(3);
            return (Vec::new(), false);
        }

        // Decode each completed frame independently, then compact the buffer
        // once. Front-draining each frame shifts the same trailing bytes over
        // and over when one transport chunk contains many SSE events. The
        // decode is strict: invalid UTF-8 must surface as the terminal
        // malformed-frame error, never as lossily-replaced content — and must
        // not drop the valid frames that preceded the bad one.
        let mut events = Vec::new();
        let mut malformed = false;
        let mut consume_end = 0;
        let mut frame_start = 0;
        for (content_end, terminator_len) in frames {
            let frame_end = content_end + terminator_len;
            consume_end = frame_end;
            let raw = match std::str::from_utf8(&self.buffer[frame_start..content_end]) {
                Ok(raw) => raw,
                Err(_) => {
                    malformed = true;
                    break;
                }
            };
            frame_start = frame_end;
            let normalized = if raw.contains('\r') {
                std::borrow::Cow::Owned(raw.replace("\r\n", "\n"))
            } else {
                std::borrow::Cow::Borrowed(raw)
            };
            match crate::model::responses::parse_sse_frame(&normalized) {
                None => {}
                Some(Ok(event)) => events.push(event),
                Some(Err(_)) => {
                    malformed = true;
                    break;
                }
            }
        }
        self.buffer.drain(..consume_end);
        self.scan_from = self.buffer.len().saturating_sub(3);
        (events, malformed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::to_bytes;
    use futures_util::StreamExt;
    use serde_json::json;

    use super::{translated_stream, TERMINAL_DRAIN_BUDGET};
    use crate::adapters::responses::context::RelayOptions;
    use crate::keepalive::with_pings;
    use crate::model::responses::{AnthropicSseMachine, ResponseEvent};

    fn machine() -> AnthropicSseMachine {
        RelayOptions {
            model: "gpt-5.2-codex".to_string(),
            thinking_enabled: false,
            tool_search_native: false,
            stop_sequences: Vec::new(),
        }
        .machine()
        .with_input_estimate(0)
        .without_content_accumulation()
    }

    /// After the terminal frame the outward stream must end immediately:
    /// with a keepalive interval shorter than the drain budget, a poll that
    /// waits out the drain lets the wrapper inject a client-visible `ping`
    /// past `message_stop`.
    #[tokio::test]
    async fn a_keepalive_ping_never_follows_the_terminal_frame() {
        let events = futures_util::stream::iter([
            Ok(ResponseEvent {
                event: Some("response.created".to_string()),
                data: json!({"response": {"id": "resp_1"}}),
            }),
            Ok(ResponseEvent {
                event: Some("response.completed".to_string()),
                data: json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            }),
        ])
        // The upstream stays open past the terminal, like the
        // prompt-closing connections the drain exists for.
        .chain(futures_util::stream::pending::<
            Result<ResponseEvent, serde_json::Value>,
        >());
        let output = with_pings(
            translated_stream(events, machine()),
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        let bytes = to_bytes(axum::body::Body::from_stream(output), usize::MAX)
            .await
            .expect("body is readable");
        let elapsed = started.elapsed();
        let text = String::from_utf8_lossy(&bytes);
        let stop_at = text
            .rfind("event: message_stop")
            .expect("terminal frame present");
        assert!(
            !text[stop_at..].contains("event: ping"),
            "no keepalive frame may follow the terminal frame, got: {text}"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "the outward stream ends at the terminal frame instead of waiting out the {TERMINAL_DRAIN_BUDGET:?} drain, took {elapsed:?}"
        );
    }

    /// The detached drain keeps reading the still-open upstream to EOF after
    /// the outward stream ends — the connection-pooling purpose the budget
    /// exists for. A producer tail that reports its first poll proves the
    /// drain ran, detached from the relay.
    #[tokio::test]
    async fn the_detached_drain_reads_the_upstream_tail() {
        use std::sync::Arc;
        let drained = Arc::new(tokio::sync::Notify::new());
        let flag = drained.clone();
        let tail = futures_util::stream::unfold((), move |()| {
            let flag = flag.clone();
            async move {
                flag.notify_one();
                futures_util::future::pending::<()>().await;
                None::<(Result<ResponseEvent, serde_json::Value>, ())>
            }
        });
        let events = futures_util::stream::iter([
            Ok(ResponseEvent {
                event: Some("response.created".to_string()),
                data: json!({"response": {"id": "resp_1"}}),
            }),
            Ok(ResponseEvent {
                event: Some("response.completed".to_string()),
                data: json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            }),
        ])
        .chain(tail);
        let output = translated_stream(events, machine());
        let bytes = to_bytes(axum::body::Body::from_stream(output), usize::MAX)
            .await
            .expect("body is readable");
        assert!(String::from_utf8_lossy(&bytes).contains("event: message_stop"));
        tokio::time::timeout(Duration::from_secs(1), drained.notified())
            .await
            .expect("the detached drain polls the upstream past the terminal frame");
    }
}
