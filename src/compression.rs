//! zstd compression for Responses request bodies (issue #285).
//!
//! The Codex CLI zstd-compresses its Responses **request** body whenever it
//! talks to the ChatGPT backend (`codex-rs/http-client/src/request.rs`:
//! `zstd::stream::encode_all(.., 3)` plus a `Content-Encoding: zstd` header).
//! shunt reaches the same backend, so this module gives both directions of that
//! wire format:
//!
//! * outbound — [`compress_request_body`] prepares the body shunt sends upstream
//!   on the ChatGPT/Codex flavor (see `Config::responses_request_compression`).
//! * inbound — [`decode_zstd_and_parse`] decodes a compressed body the Codex CLI
//!   sent to `[server.codex_endpoint]` and extracts a caller-defined label from
//!   it in the same bounded blocking work, so neither the decoded body nor a
//!   parse over it ever crosses back to the async executor on its own. The
//!   passthrough still forwards the original bytes verbatim.
//!   [`decode_zstd_within`] is the same decode without the fused parse — kept
//!   for the outbound round-trip tests, which just need the decoded bytes back.
//!
//! Compression is CPU-bound at every size, so [`compress_request_body`] always
//! runs it on Tokio's blocking pool under bounded admission rather than on the
//! async executor (same discipline as Cursor's framing/gzip work,
//! `adapters::cursor::offload`). Its doc comment has the measurements showing why
//! no inline fast path is worth having.
//!
//! Decoding is different: zstd's expansion ratio is unbounded, so gating
//! offload on the *compressed* input size (as a naive mirror of the outbound
//! path would) lets a tiny, highly-redundant body force an unbounded-looking
//! inline decode with no admission control — issue #291's decompression-bomb
//! finding (64 MiB of zeros compresses to ~2 KiB, a ~32,000x ratio, comfortably
//! under any input-size inline threshold). [`decode_zstd_within`] instead keys
//! inline eligibility on a cheap compressed-size pre-filter
//! ([`INLINE_ZSTD_INPUT_BYTES`]) *and* a bounded probe of the *decoded* output
//! ([`INLINE_ZSTD_OUTPUT_BYTES`]), and separately bounds worst-case decode work
//! to a multiple of what the peer actually uploaded ([`MAX_DECODE_RATIO`]).

use axum::http::{header::CONTENT_ENCODING, HeaderMap};
use bytes::Bytes;

/// The zstd level codex compresses Responses request bodies at
/// (`zstd::stream::encode_all(.., 3)`), which is also zstd's own default: near
/// gzip-level ratios at several hundred MB/s.
const ZSTD_LEVEL: i32 = 3;

/// Typical zstd-3 expansion ratio for Responses-request JSON: a live probe on
/// this path measured 2988 -> 251 bytes, ~12x (the same measurement
/// [`MAX_DECODE_RATIO`]'s headroom is judged against). This is not itself a
/// safety bound — [`MAX_DECODE_RATIO`] is — it only keeps
/// [`INLINE_ZSTD_INPUT_BYTES`] honest against [`INLINE_ZSTD_OUTPUT_BYTES`] (see
/// the assertion below) so the two constants cannot silently drift back out of
/// alignment the way they did before this fix (issue #291 follow-up: a 4 KiB
/// pre-filter against a 32 KiB output cap meant every compressed body between
/// ~2.7 and 4 KiB was admitted to the inline probe only to fail it, burning
/// ~80 µs on the async worker before redoing the whole decode off-thread).
const TYPICAL_ZSTD_RATIO: usize = 12;

/// Compressed-size pre-filter for the body a **decode** may attempt inline
/// (mirrors `adapters::cursor::connect::INLINE_GZIP_FRAME_BYTES`). Unlike
/// compression, a compressed body's size does not bound its decoded size — see
/// the module doc — so this alone cannot be the inline/offload gate; it is only
/// a cheap early-out so an obviously large frame skips straight to the bounded
/// probe's allocation. [`INLINE_ZSTD_OUTPUT_BYTES`] is the bound that actually
/// keeps inline work small.
///
/// Derived from [`INLINE_ZSTD_OUTPUT_BYTES`] and [`TYPICAL_ZSTD_RATIO`] rather
/// than chosen independently: at the ratio, 2 KiB decodes to ~24 KiB, leaving
/// margin inside the 32 KiB output cap, so a body admitted here has a
/// realistic chance of actually fitting the inline probe instead of being
/// doomed to fall through it. Kept as a plain literal for readability; the
/// assertion below enforces the relationship at compile time.
pub(crate) const INLINE_ZSTD_INPUT_BYTES: usize = 2 * 1024;

const _: () = assert!(
    INLINE_ZSTD_INPUT_BYTES * TYPICAL_ZSTD_RATIO <= INLINE_ZSTD_OUTPUT_BYTES,
    "INLINE_ZSTD_INPUT_BYTES must stay small enough that a typical-ratio decode fits INLINE_ZSTD_OUTPUT_BYTES"
);

/// Maximum complete **decoded** output [`decode_zstd_within`] will accept
/// inline; the bounded probe reads one byte past this to detect a larger body
/// without allocating for it. Measured medians from `measure_inline_zstd_budgets`
/// on representative Responses-request JSON: 8 KiB ~48-67 µs, 16 KiB ~65-69 µs,
/// 32 KiB ~64-105 µs (straddling, but still consistently below), 64 KiB ~104-
/// 115 µs (consistently past). 32 KiB is the largest size that reliably stays
/// inside Tokio's ~100 µs blocking-work budget.
pub(crate) const INLINE_ZSTD_OUTPUT_BYTES: usize = 32 * 1024;

/// The maximum multiple of a compressed body's own size its decoded output may
/// be. zstd's maximum theoretical expansion ratio is enormous, so an absolute
/// decode cap alone still lets a small compressed body cost an arbitrarily
/// large multiple of its own size to decode (issue #291: 64 MiB of zeros
/// compresses to ~2 KiB, a ~32,000x ratio). Real zstd-3 Responses JSON lands
/// around [`TYPICAL_ZSTD_RATIO`] in practice; 64x is generous headroom for a
/// legitimately redundant agentic history while still bounding worst-case
/// decode work to a multiple of what the peer actually uploaded, rather than
/// an absolute size unrelated to it.
pub(crate) const MAX_DECODE_RATIO: usize = 64;

/// Bodies smaller than this are sent uncompressed: at a few hundred bytes the
/// frame header plus a poor ratio on a short, low-redundancy body can leave the
/// request no smaller, and the backend accepts both encodings. Every real turn
/// (instructions + tool schemas + history) is far above this, so the gate only
/// spares degenerate bodies.
pub(crate) const MIN_COMPRESS_BYTES: usize = 1024;

/// Admission slots for zstd request **compression** (outbound). Split from
/// [`decode_slots`] per this module's own invariant (`offload`: "each work class
/// owns its own semaphore so a burst in one class cannot starve another") —
/// before the ratio-bound decode fix (issue #291) most inbound decodes stayed
/// under the inline threshold and rarely offloaded, so sharing one pool barely
/// mattered; after it, nearly every inbound decode offloads, and an unrelated
/// decode burst would otherwise delay this turn's outbound compression.
///
/// A permit bounds one in-progress task and that task's working set, not total
/// resident memory: queued inputs and completed outputs remain resident outside
/// the permit.
fn compress_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(crate::offload::cpu_sized_semaphore)
}

/// Admission slots for inbound zstd **decode** (the `[server.codex_endpoint]`
/// `model` label path). See [`compress_slots`] for why this is a separate pool.
fn decode_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(crate::offload::cpu_sized_semaphore)
}

/// How a request body is encoded, as far as reading a label out of it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEncoding {
    /// No `content-encoding` header, or the explicit `identity` coding: the body
    /// can be parsed as it arrived.
    Identity,
    /// `content-encoding: zstd` — decode with [`decode_zstd_within`] first.
    Zstd,
    /// Some other content coding. shunt never asks a client for one and does not
    /// decode it; a caller that needs the body's contents has to give up.
    Other,
}

/// Classify a request's `content-encoding` for label extraction. Only a single
/// coding is recognized: a stacked list (`gzip, zstd`) is [`BodyEncoding::Other`],
/// since decoding it would mean applying the codings in reverse order and no
/// client shunt serves sends one.
pub(crate) fn body_encoding(headers: &HeaderMap) -> BodyEncoding {
    let Some(encoding) = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return BodyEncoding::Identity;
    };
    let encoding = encoding.trim();
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        BodyEncoding::Identity
    } else if encoding.eq_ignore_ascii_case("zstd") {
        BodyEncoding::Zstd
    } else {
        BodyEncoding::Other
    }
}

/// zstd-compress an upstream request body, or `Ok(None)` when it is below
/// [`MIN_COMPRESS_BYTES`] and is better sent as-is.
///
/// Compression always runs on the blocking pool, at every size — there is no
/// inline fast path. The fixed cost below is a property of the one-shot
/// `zstd::stream::encode_all` API this function uses, **not of zstd itself**:
/// `encode_all` builds a fresh level-3 encoder (a ~2 MiB window plus hash
/// tables) on every call, and that setup dominates any body small enough to be
/// an inline candidate. `zstd::bulk::Compressor` reuses its context across
/// calls instead, and measured (`measure_inline_zstd_budgets`, same harness,
/// same sizes) at 1 KiB ~5.8 µs, 2 KiB ~7.8 µs, 4 KiB ~11.9 µs, 8 KiB
/// ~20.4 µs, 16 KiB ~41.5 µs, 32 KiB ~69.3 µs, 64 KiB ~128.3 µs, 128 KiB
/// ~227.9 µs — the ~90 µs fixed floor below is almost entirely context setup:
/// a reused context brings the 1 KiB case down to ~6 µs, over an order of
/// magnitude less. That was measured and **deliberately not adopted**, on
/// cost/benefit rather than feasibility: admission here is already capped at
/// 2–16 concurrent slots by [`compress_slots`], so a pooled context would need
/// ~16 of them at most, which is affordable. The reason to skip it is that this
/// runs *once per turn* against a multi-second LLM turn, so saving ~80 µs of
/// blocking-pool CPU is immaterial end to end, and `Compressor` is not `Sync` —
/// adopting it means introducing thread-local or pooled mutable state to save
/// microseconds. Worth revisiting only if compression ever moves onto a
/// per-chunk path, where the fixed cost would be paid repeatedly. The
/// measurement is recorded here to correct the framing below: the fixed cost
/// belongs to the one-shot API, not to zstd itself.
///
/// Measured medians from `measure_inline_zstd_budgets` (one-shot `encode_all`,
/// `cargo test --release -- --ignored --nocapture measure_inline_zstd_budgets`)
/// on representative Responses-request JSON: 1 KiB ~86.8 µs, 2 KiB ~86.0 µs,
/// 4 KiB ~93.1 µs, 8 KiB ~115.1 µs, 16 KiB ~128.7 µs, 32 KiB ~190.9 µs, 64 KiB
/// ~230.2 µs, 128 KiB ~428.5 µs. 128x the bytes costs only ~5x the time —
/// roughly 85 µs fixed plus ~2.7 µs/KiB (~370 MB/s marginal).
///
/// So the smallest body compressed at all ([`MIN_COMPRESS_BYTES`], 1 KiB) already
/// costs ~87 µs, essentially all of Tokio's ~100 µs blocking-work budget: no
/// threshold exists that would keep an inline compression inside it — the fixed
/// cost alone (~85 µs) is already at the line, independent of size and
/// independent of which compression API produces it. Offloading costs ~40 µs of
/// added latency (`measure_spawn_blocking_round_trip`) and blocks no worker at
/// all, so it wins at every eligible size. It happens once per turn — the
/// prepared body is reused across retries and rotations — so that latency
/// is immaterial.
pub(crate) async fn compress_request_body(body: Bytes) -> std::io::Result<Option<Bytes>> {
    if body.len() < MIN_COMPRESS_BYTES {
        return Ok(None);
    }
    crate::offload::spawn_bounded(compress_slots(), move || compress(&body))
        .await?
        .map(Some)
}

/// Decode a zstd body, or `Ok(None)` when it decodes to more than `cap` bytes
/// or more than [`MAX_DECODE_RATIO`] times the compressed body's own size,
/// whichever is smaller (see the module doc for why the ratio bound exists
/// alongside the absolute one).
///
/// Small, cheaply-probed bodies decode inline on the async executor; anything
/// else is offloaded to the blocking pool (see [`INLINE_ZSTD_INPUT_BYTES`] and
/// [`INLINE_ZSTD_OUTPUT_BYTES`]). A thin wrapper over [`decode_zstd_and_parse`]
/// with an identity extractor — kept as its own function because the outbound
/// round-trip tests just want the decoded [`Bytes`] back, with nothing to
/// parse out of them.
///
/// `#[cfg(test)]`: production code parses inbound bodies (`codex_endpoint`'s
/// `model_label`), so it calls [`decode_zstd_and_parse`] directly rather than
/// decoding here and parsing separately — this wrapper's only remaining
/// callers are the round-trip tests in this module and in
/// `adapters::responses::body`.
#[cfg(test)]
pub(crate) async fn decode_zstd_within(body: Bytes, cap: usize) -> std::io::Result<Option<Bytes>> {
    decode_zstd_and_parse(body, cap, |decoded| decoded).await
}

/// Decode a zstd body and, without ever handing the decoded [`Bytes`] back to
/// the caller, apply `extract` to it in the same execution context the decode
/// itself ran in — inline if the decode was cheap enough to run inline,
/// inside the same offloaded blocking task otherwise. Same admission,
/// pre-filter, and budget rules as [`decode_zstd_within`] (which this
/// implements).
///
/// This exists because handing the decoded bytes back to an async caller for
/// it to parse separately defeats the point of bounding and offloading the
/// decode: [`MAX_DECODE_RATIO`] lets a ~1 MiB compressed upload buy up to a
/// 64 MiB decode budget, and a `serde_json::from_slice` over a document that
/// size is itself blocking-pool-worthy work (already milliseconds, far past
/// Tokio's ~100 µs budget) — running it on the async executor after the
/// offloaded decode returns would block a worker exactly the way offloading
/// the decode was meant to prevent (issue #291 follow-up). `extract` is
/// expected to reduce the decoded body to something small (e.g. a label
/// string); its result is what crosses back to the async side, not the bytes
/// it was computed from.
pub(crate) async fn decode_zstd_and_parse<T, F>(
    body: Bytes,
    cap: usize,
    extract: F,
) -> std::io::Result<Option<T>>
where
    F: FnOnce(Bytes) -> T + Send + 'static,
    T: Send + 'static,
{
    let budget = cap.min(body.len().saturating_mul(MAX_DECODE_RATIO));
    if body.len() <= INLINE_ZSTD_INPUT_BYTES {
        let inline_limit = INLINE_ZSTD_OUTPUT_BYTES.min(budget);
        if let Some(out) = decode_within(&body, inline_limit)? {
            return Ok(Some(extract(out)));
        }
        if inline_limit == budget {
            // The probe ran against the real `budget`, so its `None` is
            // already the authoritative "over budget" answer. Offloading
            // would re-run `decode_within` with an identical budget and
            // reach the same `None`, paying a second full decode plus a
            // `decode_slots` permit for it. Reachable whenever
            // `budget <= INLINE_ZSTD_OUTPUT_BYTES` — with a large `cap`,
            // a body of at most `INLINE_ZSTD_OUTPUT_BYTES / MAX_DECODE_RATIO`
            // bytes — which is exactly the small ratio-bomb shape, so the
            // hostile path is the one that stops paying twice.
            return Ok(None);
        }
        // Otherwise the inline probe only rules out a body that fits
        // `INLINE_ZSTD_OUTPUT_BYTES`; it does not tell us whether the body is
        // over `budget` (genuinely too big) or merely over the inline
        // allowance (fits `budget`, but too much decode work to do inline).
        // Fall through to the offloaded decode, which re-runs against the
        // real `budget` either way — `Ok(None)` from *that* call is the
        // authoritative "over budget" answer.
    }
    crate::offload::spawn_bounded(decode_slots(), move || {
        decode_within(&body, budget).map(|maybe| maybe.map(extract))
    })
    .await?
}

fn compress(body: &[u8]) -> std::io::Result<Bytes> {
    zstd::stream::encode_all(body, ZSTD_LEVEL).map(Bytes::from)
}

/// Capacity hint for [`decode_within`]'s output buffer: a typical *zstd* ratio
/// on this JSON, not the ~4x gzip ratio this was originally copied from
/// (`adapters::cursor::connect::decode_gzip_frame_within`). zstd-3 on
/// Responses-request JSON runs closer to [`TYPICAL_ZSTD_RATIO`] (~12x); the
/// smaller gzip-tuned multiplier under-allocated and forced `Vec` to
/// reallocate (and copy) partway through most real decodes. Deliberately a
/// little under `TYPICAL_ZSTD_RATIO` rather than equal to it: this is only a
/// sizing hint (an under-estimate just costs one extra reallocation, not
/// correctness), so there is no reason to tie it to the same constant the
/// inline pre-filter's safety-relevant assertion depends on.
const DECODE_CAPACITY_RATIO: usize = 10;

/// Decode `body`, giving up with `Ok(None)` rather than allocating past
/// `budget`. The decoder reads one byte beyond the budget so an over-budget
/// body is detected instead of silently truncated into invalid JSON. This
/// bounds the decoded output to `budget`; it is [`decode_zstd_within`] that
/// ties `budget` to the compressed input's own size via [`MAX_DECODE_RATIO`],
/// so a compressed body cannot buy an arbitrarily large decode for its size.
fn decode_within(body: &[u8], budget: usize) -> std::io::Result<Option<Bytes>> {
    use std::io::Read;

    // Size the buffer from a realistic *zstd* ratio, capped by the budget, so
    // a typical body decodes without repeated reallocation. The multiplier
    // mirrors the shape of `adapters::cursor::connect::decode_gzip_frame_within`
    // but not its value — see [`DECODE_CAPACITY_RATIO`].
    //
    // Deliberately not sized from the frame header's declared content size:
    // `zstd::stream::encode_all` (what `compress` uses, and the streaming shape
    // a client encoder uses too) does not pledge a source size, so
    // `get_frame_content_size` reports `None` for these frames and a
    // header-derived hint would be inert. It is also client-controlled, so it
    // could only ever be allowed to shrink this reservation, never grow it.
    let mut out = Vec::with_capacity(std::cmp::min(
        body.len().saturating_mul(DECODE_CAPACITY_RATIO),
        budget + 1,
    ));
    zstd::stream::read::Decoder::new(body)?
        .take(budget as u64 + 1)
        .read_to_end(&mut out)?;
    Ok((out.len() <= budget).then(|| Bytes::from(out)))
}

#[cfg(test)]
mod tests;
