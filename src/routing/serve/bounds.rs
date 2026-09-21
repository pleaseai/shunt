//! Byte, idle, and wall-clock fences for the bodies a driven router reads
//! (ADR-0005 §3, issue #594).
//!
//! What this module guards is the half of a bound a `.send()`-style timeout
//! cannot reach. An upstream that answers `200` and then stalls, or that keeps
//! a stream alive with nothing but SSE keep-alive pings, has committed headers
//! — so every deadline that stops at headers has already passed, and the call
//! would hang for as long as the upstream cared to hold it. Both collectors
//! here measure the **body**.
//!
//! [`collect_bounded`] is the judge side: one non-streaming reply, refused the
//! moment it passes its cap rather than after it is buffered.
//! [`bound_stream`] is the retained-turn side PR 6's gated `escalation` and
//! `advisor` turns will use — nothing gated exists yet, so it is wired to
//! nothing and unit-tested instead of dead-coded later.

use std::borrow::Cow;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
// `tokio::time::Instant`, not `std`'s: every deadline here is awaited through
// `tokio::time`, and mixing the two clocks makes the bounds untestable under a
// paused runtime clock — the std clock keeps advancing while tokio's does not.
use tokio::time::Instant;

/// A body that passed [`collect_bounded`]'s cap. Carries no partial body:
/// the point of the cap is that the bytes past it are never held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Oversized {
    /// The cap that was crossed, for the log line that reports it.
    pub(crate) max_bytes: usize,
}

impl std::fmt::Display for Oversized {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "response body exceeded {} bytes", self.max_bytes)
    }
}

impl std::error::Error for Oversized {}

/// Why [`collect_bounded`] did not produce a whole body.
#[derive(Debug)]
pub(crate) enum CollectError {
    /// The body passed `max_bytes`.
    Oversized(Oversized),
    /// The body stream failed part-way through. What had been read is a
    /// truncated reply rather than the upstream's answer, so it is dropped
    /// instead of returned.
    Transport(axum::Error),
}

/// Read a whole body, refusing it the moment it passes `max_bytes`.
///
/// Stops *reading* on the crossing rather than buffering the body and checking
/// afterwards: an unbounded reply is exactly the case the cap exists for, so
/// collecting it first would spend the memory the bound is meant to deny.
pub(crate) async fn collect_bounded(
    body: axum::body::Body,
    max_bytes: usize,
) -> Result<Bytes, CollectError> {
    let mut data = body.into_data_stream();
    // Sized against the cap, but not *to* it: a judge reply is a few hundred
    // bytes and `max_bytes` is the ceiling for the pathological one, so
    // allocating the whole cap up front would spend 64 KiB on every call to
    // save a dozen amortized grows on none of them.
    let mut collected = Vec::with_capacity(max_bytes.min(4096));
    let mut total = 0usize;
    while let Some(chunk) = data.next().await {
        // A transport error truncates the body, and the truncation is not
        // the caller's to diagnose from the bytes. Returning what was read
        // hands back a partial reply whose JSON parse fails, so the call is
        // recorded as `invalid_reply` — the judge answered with something
        // malformed — when what actually happened is that the connection
        // broke. Carried out as its own variant so the caller names the fault
        // that occurred; treating it as oversized would name the wrong bound.
        let chunk = chunk.map_err(CollectError::Transport)?;
        total = total.saturating_add(chunk.len());
        if total > max_bytes {
            return Err(CollectError::Oversized(Oversized { max_bytes }));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

/// The three bounds a retained (gated) turn runs under (ADR-0005 §3).
///
/// Constructed by nothing yet: the gated lane is PR 6 (`escalation`,
/// `advisor`). It ships here, with the rest of the bound set it belongs to and
/// with its own tests, rather than arriving later as an untested prerequisite.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GatedBounds {
    /// Bytes retained before the turn is discarded.
    pub(crate) max_bytes: usize,
    /// Gap allowed between two chunks that carry content.
    pub(crate) idle: Duration,
    /// Wall-clock ceiling, measured from the first poll.
    pub(crate) max_duration: Duration,
}

/// Which bound a gated turn crossed. A closed set: each variant is a distinct
/// operational failure an operator tunes with a distinct key, and collapsing
/// them into one "bound exceeded" would leave the log naming no key.
///
/// Unused until PR 6 wires the gated lane; see [`GatedBounds`].
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundExceeded {
    /// `gated_max_bytes`.
    MaxBytes,
    /// `gated_idle_ms` elapsed with no content chunk.
    Idle,
    /// `gated_max_duration_ms` elapsed.
    Duration,
}

impl std::fmt::Display for BoundExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MaxBytes => "gated_max_bytes",
            Self::Idle => "gated_idle_ms",
            Self::Duration => "gated_max_duration_ms",
        })
    }
}

/// Wrap a body stream in the three gated bounds, ending it with the bound it
/// crossed.
///
/// Takes already-unwrapped chunks rather than a `Result` stream because
/// [`BoundExceeded`] is deliberately closed: an upstream transport failure is
/// not a bound the operator configured, and folding it in here would put a
/// fourth, untunable reason behind a name that promises three. The caller ends
/// the stream on a transport error before it reaches this wrapper.
///
/// The idle timer is **not reset by SSE ping frames**. A keep-alive is the
/// upstream saying the socket is alive, not that the turn is progressing, so an
/// endless ping stream is exactly what this bound is for; resetting on one
/// would make the idle gap unreachable (ADR-0005 §3 names the endless-ping
/// stall as a required test).
///
/// What refreshes the timer is a **completed content frame**, tracked across
/// chunk boundaries: frames are reassembled from a carried remainder, so a
/// stream that splits every `event: ping\n\n` mid-line is classified on the
/// frames it actually sent rather than on whatever happened to land in one
/// chunk. A chunk that completes no frame therefore refreshes nothing — a half
/// arrived frame is not yet evidence of content, and treating it as such is
/// precisely what lets a ping split in two disarm the bound. The consequence
/// is that the idle gap measures the interval between *delivered* content
/// frames, so a single frame must arrive in full inside it.
///
/// Called by nothing yet; see [`GatedBounds`].
#[allow(dead_code)]
pub(crate) fn bound_stream<S>(
    stream: S,
    gated: GatedBounds,
) -> impl Stream<Item = Result<Bytes, BoundExceeded>>
where
    S: Stream<Item = Bytes> + Send + 'static,
{
    struct State<S> {
        stream: std::pin::Pin<Box<S>>,
        gated: GatedBounds,
        /// Wall-clock origin, taken on the first poll rather than at
        /// construction: the caller may build the wrapper before it is awaited,
        /// and the bound is on the turn, not on the value's lifetime.
        started_at: Option<Instant>,
        /// When the idle gap runs out, refreshed only by a completed content
        /// frame.
        idle_deadline: Option<Instant>,
        /// Bytes of a frame that arrived without its terminator, carried to the
        /// next chunk. Without it a frame split mid-line is invisible to the
        /// classifier on both halves. Bounded by `max_bytes` along with
        /// everything else, since it can never hold more than the stream sent.
        remainder: Vec<u8>,
        total: usize,
        finished: bool,
    }

    futures_util::stream::unfold(
        State {
            stream: Box::pin(stream),
            gated,
            started_at: None,
            idle_deadline: None,
            remainder: Vec::new(),
            total: 0,
            finished: false,
        },
        |mut state| async move {
            if state.finished {
                return None;
            }
            let now = Instant::now();
            let started_at = *state.started_at.get_or_insert(now);
            let idle_deadline = *state
                .idle_deadline
                .get_or_insert_with(|| now + state.gated.idle);
            let hard_deadline = started_at + state.gated.max_duration;
            // Whichever fence comes first decides how long this poll may wait,
            // so a stream that never yields again still ends at the earlier of
            // the two rather than at the idle gap alone.
            let deadline = idle_deadline.min(hard_deadline);
            let next = tokio::time::timeout_at(deadline, state.stream.next()).await;
            let chunk = match next {
                Ok(Some(chunk)) => chunk,
                // The source ended on its own; no bound was crossed.
                Ok(None) => return None,
                Err(_) => {
                    state.finished = true;
                    let exceeded = if hard_deadline <= idle_deadline {
                        BoundExceeded::Duration
                    } else {
                        BoundExceeded::Idle
                    };
                    return Some((Err(exceeded), state));
                }
            };
            if Instant::now() >= hard_deadline {
                state.finished = true;
                return Some((Err(BoundExceeded::Duration), state));
            }
            state.total = state.total.saturating_add(chunk.len());
            if state.total > state.gated.max_bytes {
                state.finished = true;
                return Some((Err(BoundExceeded::MaxBytes), state));
            }
            // Framing is decided on the stream, not on the chunk: the
            // remainder carries a partial frame forward so the classifier sees
            // whole frames however the upstream chose to split them.
            state.remainder.extend_from_slice(&chunk);
            let frames = take_complete_frames(&mut state.remainder);
            if frames.iter().any(|frame| !is_ping_frame(frame)) {
                state.idle_deadline = Some(Instant::now() + state.gated.idle);
            }
            Some((Ok(chunk), state))
        },
    )
}

/// Pull every **complete** SSE frame out of `buffer`, leaving a trailing
/// partial frame behind for the next chunk to finish.
///
/// Stateful on purpose. A chunk boundary is the upstream's choice, not a
/// framing event, so classifying a bare chunk decides the idle bound on where
/// the network happened to split: `event: pin` + `g\n\n` and `event: ping\n` +
/// `\n` are both a single ping frame, and neither half is one on its own.
/// Carrying the remainder is what makes the two indistinguishable from the
/// unsplit frame.
///
/// Line endings are normalized first. SSE terminates a line with CRLF, LF, or a
/// bare CR, so the blank line that ends a frame is not always a literal `\n\n` —
/// under CRLF it is `\r\n\r\n`, which contains no `\n\n` at all. Splitting the
/// raw text would then collapse everything into one frame, and a ping arriving
/// *beside* real content would read as a keep-alive and leave the idle bound
/// armed against a stream that was making progress. The allocation is taken
/// only when the buffer actually holds a `\r`, so an LF-only stream — every one
/// shunt talks to today — copies nothing.
///
/// All of that runs on **bytes**, and the buffer is only ever decoded from the
/// last blank line backwards. A chunk boundary falls wherever the network put
/// it, so the remainder routinely ends mid-character; decoding the buffer to
/// frame it would have to answer for that partial character, and both answers
/// are wrong. Refusing to frame until it completes stalls every frame already
/// in the buffer behind it, and decoding lossily writes a replacement
/// character over the partial one — which is then carried forward, so the
/// continuation bytes in the next chunk land after a character that can never
/// be completed. Neither question arises here: `\r` and `\n` are ASCII, and no
/// byte of a multi-byte UTF-8 character is, so scanning bytes cannot split
/// one.
fn take_complete_frames(buffer: &mut Vec<u8>) -> Vec<String> {
    // A trailing CR is held back unread: it may be the first half of a CRLF
    // whose LF is in the next chunk, and normalizing it now would invent a
    // frame terminator the upstream never sent.
    let held_cr = buffer.last() == Some(&b'\r');
    let scan = &buffer[..buffer.len() - usize::from(held_cr)];
    let normalized: Cow<'_, [u8]> = if scan.contains(&b'\r') {
        Cow::Owned(normalize_line_endings(scan))
    } else {
        Cow::Borrowed(scan)
    };
    // Only what precedes the last blank line is complete; the rest is a frame
    // still arriving.
    let Some(terminator) = normalized.windows(2).rposition(|pair| pair == b"\n\n") else {
        return Vec::new();
    };
    let end = terminator + 2;
    // The decode reaches the complete frames and stops there. A malformed
    // sequence inside one has no exact reading to preserve, and replacing it
    // costs nothing that survives the call: these frames are classified and
    // dropped, never written back to the buffer.
    let frames: Vec<String> = String::from_utf8_lossy(&normalized[..end])
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let mut leftover = normalized[end..].to_vec();
    if held_cr {
        leftover.push(b'\r');
    }
    *buffer = leftover;
    frames
}

/// CRLF and bare CR to LF, on bytes.
///
/// Byte-level for the reason [`take_complete_frames`] is: a partial multi-byte
/// character at the end of the buffer is the normal case, not an error, and
/// neither line ending can be part of one.
fn normalize_line_endings(scan: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(scan.len());
    let mut index = 0;
    while index < scan.len() {
        if scan[index] == b'\r' {
            out.push(b'\n');
            // Consume the LF of a CRLF pair; a bare CR consumes only itself.
            index += usize::from(scan.get(index + 1) == Some(&b'\n'));
        } else {
            out.push(scan[index]);
        }
        index += 1;
    }
    out
}

/// Whether one complete SSE frame is a keep-alive.
///
/// Frame-level, not substring-level: a frame counts as a ping only when its own
/// `event:` line names `ping`, so a real `content_block_delta` whose text
/// happens to mention the word does not disarm the idle bound.
fn is_ping_frame(frame: &str) -> bool {
    frame.lines().any(|line| {
        line.strip_prefix("event:")
            .is_some_and(|event| event.trim() == "ping")
    })
}
