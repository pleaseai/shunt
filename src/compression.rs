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
//! * inbound — [`decode_zstd_within`] decodes a compressed body the Codex CLI
//!   sent to `[server.codex_endpoint]`, so the `model` metrics/log label can be
//!   read from it. The passthrough still forwards the original bytes verbatim.
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

/// Compressed-size pre-filter for the body a **decode** may attempt inline
/// (mirrors `adapters::cursor::connect::INLINE_GZIP_FRAME_BYTES`). Unlike
/// compression, a compressed body's size does not bound its decoded size — see
/// the module doc — so this alone cannot be the inline/offload gate; it is only
/// a cheap early-out so an obviously large frame skips straight to the bounded
/// probe's allocation. [`INLINE_ZSTD_OUTPUT_BYTES`] is the bound that actually
/// keeps inline work small.
pub(crate) const INLINE_ZSTD_INPUT_BYTES: usize = 4 * 1024;

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
/// around 10x in practice (a live probe on this path measured 2988 -> 251
/// bytes, ~12x); 64x is generous headroom for a legitimately redundant agentic
/// history while still bounding worst-case decode work to a multiple of what
/// the peer actually uploaded, rather than an absolute size unrelated to it.
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
/// inline fast path. `zstd::stream::encode_all` builds a fresh level-3 encoder
/// per call (a ~2 MiB window plus hash tables), and that setup dominates any
/// body small enough to be an inline candidate. Measured medians from
/// `measure_inline_zstd_budgets` (`cargo test --release -- --ignored --nocapture
/// measure_inline_zstd_budgets`) on representative Responses-request JSON: 1 KiB
/// ~94 µs, 2 KiB ~94 µs, 4 KiB ~99 µs, 8 KiB ~109 µs, 16 KiB ~138 µs, 32 KiB
/// ~158 µs, 64 KiB ~233 µs, 128 KiB ~397 µs. 128x the bytes costs only ~4x the
/// time — roughly 90 µs fixed plus ~2.4 µs/KiB (~420 MB/s marginal).
///
/// So the smallest body compressed at all ([`MIN_COMPRESS_BYTES`], 1 KiB) already
/// costs ~94 µs, essentially all of Tokio's ~100 µs blocking-work budget: no
/// threshold exists that would keep an inline compression inside it. Offloading
/// costs ~40 µs of added latency (`measure_spawn_blocking_round_trip`) and blocks
/// no worker at all, so it wins at every eligible size. It happens once per turn
/// — the prepared body is reused across retries and rotations — so that latency
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
/// [`INLINE_ZSTD_OUTPUT_BYTES`]).
pub(crate) async fn decode_zstd_within(body: Bytes, cap: usize) -> std::io::Result<Option<Bytes>> {
    let budget = cap.min(body.len().saturating_mul(MAX_DECODE_RATIO));
    if body.len() <= INLINE_ZSTD_INPUT_BYTES {
        if let Some(out) = decode_within(&body, INLINE_ZSTD_OUTPUT_BYTES.min(budget))? {
            return Ok(Some(out));
        }
        // The inline probe only rules out a body that fits
        // `INLINE_ZSTD_OUTPUT_BYTES`; it does not tell us whether the body is
        // over `budget` (genuinely too big) or merely over the inline
        // allowance (fits `budget`, but too much decode work to do inline).
        // Fall through to the offloaded decode, which re-runs against the
        // real `budget` either way — `Ok(None)` from *that* call is the
        // authoritative "over budget" answer.
    }
    crate::offload::spawn_bounded(decode_slots(), move || decode_within(&body, budget)).await?
}

fn compress(body: &[u8]) -> std::io::Result<Bytes> {
    zstd::stream::encode_all(body, ZSTD_LEVEL).map(Bytes::from)
}

/// Decode `body`, giving up with `Ok(None)` rather than allocating past
/// `budget`. The decoder reads one byte beyond the budget so an over-budget
/// body is detected instead of silently truncated into invalid JSON. This
/// bounds the decoded output to `budget`; it is [`decode_zstd_within`] that
/// ties `budget` to the compressed input's own size via [`MAX_DECODE_RATIO`],
/// so a compressed body cannot buy an arbitrarily large decode for its size.
fn decode_within(body: &[u8], budget: usize) -> std::io::Result<Option<Bytes>> {
    use std::io::Read;

    // Size the buffer from a realistic ratio, capped by the budget, so a
    // typical body decodes without repeated reallocation (mirrors
    // `adapters::cursor::connect::decode_gzip_frame_within`).
    let mut out = Vec::with_capacity(std::cmp::min(body.len().saturating_mul(4), budget + 1));
    zstd::stream::read::Decoder::new(body)?
        .take(budget as u64 + 1)
        .read_to_end(&mut out)?;
    Ok((out.len() <= budget).then(|| Bytes::from(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(encoding: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, encoding.parse().unwrap());
        headers
    }

    /// A JSON-shaped body of at least `len` bytes: repeated keys and structure
    /// like a real Responses request, but varied message content (an
    /// xorshift64-driven word picker, not the same string over and over) so
    /// the compression ratio stays in a realistic range rather than the
    /// extreme, near-best-case ratio identical repeated content gets — a body
    /// that compresses unrealistically well would itself trip
    /// [`MAX_DECODE_RATIO`] and make these round-trip tests indistinguishable
    /// from the decompression-bomb fixture in `rejects_a_decompression_bomb`.
    fn json_body(len: usize) -> Bytes {
        const WORDS: [&str; 24] = [
            "the",
            "quick",
            "brown",
            "fox",
            "jumps",
            "over",
            "lazy",
            "dog",
            "function",
            "apply_patch",
            "review",
            "error",
            "handling",
            "path",
            "repository",
            "diff",
            "unified",
            "schema",
            "parameter",
            "object",
            "required",
            "description",
            "session",
            "rotation",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next_word = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            WORDS[(state as usize) % WORDS.len()]
        };

        let mut body = String::from("{\"model\":\"gpt-5.2-codex\",\"input\":[");
        while body.len() < len {
            body.push_str("{\"role\":\"user\",\"content\":\"");
            for _ in 0..12 {
                body.push_str(next_word());
                body.push(' ');
            }
            body.push_str("\"},");
        }
        body.push_str("{\"role\":\"user\",\"content\":\"end\"}]}");
        Bytes::from(body)
    }

    #[test]
    fn classifies_content_encoding() {
        assert_eq!(body_encoding(&HeaderMap::new()), BodyEncoding::Identity);
        assert_eq!(
            body_encoding(&header_map("identity")),
            BodyEncoding::Identity
        );
        assert_eq!(body_encoding(&header_map("zstd")), BodyEncoding::Zstd);
        // Case-insensitive and whitespace-tolerant, per RFC 9110 content codings.
        assert_eq!(body_encoding(&header_map(" ZSTD ")), BodyEncoding::Zstd);
        assert_eq!(body_encoding(&header_map("gzip")), BodyEncoding::Other);
        // A stacked list is deliberately not decoded.
        assert_eq!(
            body_encoding(&header_map("gzip, zstd")),
            BodyEncoding::Other
        );
    }

    /// A realistic body compresses and round-trips, and the compressed form is
    /// materially smaller — the whole point of the feature.
    #[tokio::test]
    async fn compresses_and_round_trips_a_small_body() {
        let body = json_body(3 * 1024);
        let compressed = compress_request_body(body.clone())
            .await
            .expect("compression should succeed")
            .expect("a body this size should be compressed");
        assert!(
            compressed.len() * 4 < body.len(),
            "expected a large win on JSON, got {} -> {}",
            body.len(),
            compressed.len()
        );

        let decoded = decode_zstd_within(compressed, body.len())
            .await
            .expect("decode should succeed")
            .expect("decoded body should fit the budget");
        assert_eq!(decoded, body);
    }

    /// A body far larger than any inbound decode would attempt inline still
    /// round-trips identically through the blocking-pool compression path.
    #[tokio::test]
    async fn compresses_a_large_body() {
        let body = json_body(64 * 1024);
        let compressed = compress_request_body(body.clone())
            .await
            .expect("compression should succeed")
            .expect("a body this size should be compressed");
        let decoded = decode_zstd_within(compressed, body.len())
            .await
            .expect("decode should succeed")
            .expect("decoded body should fit the budget");
        assert_eq!(decoded, body);
    }

    /// The offloaded *decode* path is keyed on the compressed input size, so it
    /// needs a compressed frame that stays past [`INLINE_ZSTD_INPUT_BYTES`] —
    /// an incompressible body reliably does, since zstd cannot shrink it.
    #[tokio::test]
    async fn decodes_an_offloaded_body() {
        // xorshift64 output: enough entropy that zstd cannot shrink it, unlike a
        // low-period arithmetic sequence (mirrors the Cursor gzip fixtures).
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        let body: Bytes = (0..INLINE_ZSTD_INPUT_BYTES * 2)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<u8>>()
            .into();
        let compressed = compress(&body).expect("compression should succeed");
        assert!(
            compressed.len() > INLINE_ZSTD_INPUT_BYTES,
            "the fixture must stay large after compression to reach the offloaded path"
        );

        let decoded = decode_zstd_within(compressed, body.len())
            .await
            .expect("decode should succeed")
            .expect("decoded body should fit the budget");
        assert_eq!(decoded, body);
    }

    /// Below `MIN_COMPRESS_BYTES` the body is left alone rather than framed for
    /// no gain.
    #[tokio::test]
    async fn leaves_a_tiny_body_uncompressed() {
        let body = Bytes::from_static(b"{\"model\":\"gpt-5.2-codex\"}");
        assert!(body.len() < MIN_COMPRESS_BYTES);
        assert!(compress_request_body(body)
            .await
            .expect("the gate should not fail")
            .is_none());
    }

    /// A body that decodes past the budget is reported as over-budget rather than
    /// truncated to `budget` bytes (which would parse as invalid JSON) or
    /// allowed to allocate without bound.
    #[tokio::test]
    async fn refuses_a_body_that_decodes_past_the_budget() {
        let body = json_body(64 * 1024);
        let compressed = compress_request_body(body.clone())
            .await
            .unwrap()
            .expect("body should compress");
        assert!(compressed.len() < body.len());

        assert!(decode_zstd_within(compressed.clone(), body.len() - 1)
            .await
            .expect("an over-budget body is not an error")
            .is_none());
        // Exactly at the budget it is still returned whole.
        assert_eq!(
            decode_zstd_within(compressed, body.len())
                .await
                .unwrap()
                .expect("a body exactly at the budget fits"),
            body
        );
    }

    /// Garbage that claims to be zstd surfaces as an error rather than as an
    /// empty or partial body.
    #[tokio::test]
    async fn reports_an_error_for_a_body_that_is_not_zstd() {
        let error = decode_zstd_within(Bytes::from_static(b"{\"model\":\"x\"}"), 1024)
            .await
            .expect_err("undecodable input should be an error");
        assert!(!error.to_string().is_empty());
    }

    /// Regression test for issue #291: 8 MiB of zeros compresses to a few KiB
    /// (an extreme ratio no real Responses body reaches), so even with a huge
    /// absolute `cap` the decode must be rejected by [`MAX_DECODE_RATIO`] alone
    /// — otherwise a tiny compressed body could force shunt to allocate and
    /// decode tens of megabytes with no admission control.
    #[tokio::test]
    async fn rejects_a_decompression_bomb_via_the_ratio_bound() {
        let bomb = vec![0u8; 8 * 1024 * 1024];
        let compressed = compress(&bomb).expect("zero-filled input should compress");
        assert!(
            compressed.len() * MAX_DECODE_RATIO < bomb.len(),
            "fixture must actually exceed the ratio bound to exercise it, got {} -> {}",
            bomb.len(),
            compressed.len()
        );

        let decoded = decode_zstd_within(compressed, MAX_REQUEST_BODY_BYTES_FOR_TEST)
            .await
            .expect("a ratio-bomb is rejected, not an I/O error");
        assert!(
            decoded.is_none(),
            "a body decoding to {}x its compressed size must be rejected even though \
             the absolute cap alone would allow it",
            MAX_DECODE_RATIO
        );
    }

    /// A legitimate body at a realistic compression ratio still decodes under
    /// the same large absolute cap a real endpoint passes — the ratio bound
    /// added for issue #291 must not regress ordinary large turns.
    #[tokio::test]
    async fn decodes_a_legitimate_body_at_a_realistic_ratio_under_the_same_cap() {
        let body = json_body(256 * 1024);
        let compressed = compress_request_body(body.clone())
            .await
            .expect("compression should succeed")
            .expect("a body this size should be compressed");
        assert!(
            compressed.len() * MAX_DECODE_RATIO > body.len(),
            "fixture must stay within a realistic ratio for this to be a meaningful check"
        );

        let decoded = decode_zstd_within(compressed, MAX_REQUEST_BODY_BYTES_FOR_TEST)
            .await
            .expect("decode should succeed")
            .expect("a realistic-ratio body must not be rejected by the ratio bound");
        assert_eq!(decoded, body);
    }

    /// Same cap `codex_endpoint::MAX_REQUEST_BODY_BYTES` passes in production,
    /// duplicated here so these tests do not depend on that module.
    const MAX_REQUEST_BODY_BYTES_FOR_TEST: usize = 64 * 1024 * 1024;

    /// Retained measurement, not run in CI: prints median timings for
    /// compression and decode at several sizes of representative
    /// Responses-request JSON. The compression figures are what establish that no
    /// inline compression threshold is worth having (see
    /// [`compress_request_body`]); the decode figures size
    /// [`INLINE_ZSTD_OUTPUT_BYTES`] against Tokio's ~100 µs blocking-work budget
    /// (mirrors `adapters::cursor::connect::measure_spawn_blocking_round_trip`).
    /// Run with:
    /// `cargo test --release -- --ignored --nocapture measure_inline_zstd_budgets`
    #[ignore]
    #[test]
    fn measure_inline_zstd_budgets() {
        use std::time::{Duration, Instant};

        const ITERATIONS: usize = 1000;
        const WARMUP_ITERATIONS: usize = 50;

        fn median(mut samples: Vec<Duration>) -> Duration {
            samples.sort();
            samples[samples.len() / 2]
        }

        println!("-- compress (representative JSON) --");
        for size_kib in [1, 2, 4, 8, 16, 32, 64, 128] {
            let body = json_body(size_kib * 1024);
            for _ in 0..WARMUP_ITERATIONS {
                let _ = compress(&body).expect("compression should succeed");
            }
            let mut samples = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let start = Instant::now();
                let _ = compress(&body).expect("compression should succeed");
                samples.push(start.elapsed());
            }
            println!(
                "compress {size_kib:>4} KiB input: median {:?}",
                median(samples)
            );
        }

        println!("-- decode_within (decoded output size) --");
        for size_kib in [8, 16, 32, 64] {
            let body = json_body(size_kib * 1024);
            let compressed = compress(&body).expect("compression should succeed");
            for _ in 0..WARMUP_ITERATIONS {
                let _ = decode_within(&compressed, body.len())
                    .expect("decode should succeed")
                    .expect("decoded body should fit the budget");
            }
            let mut samples = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let start = Instant::now();
                let out = decode_within(&compressed, body.len())
                    .expect("decode should succeed")
                    .expect("decoded body should fit the budget");
                assert_eq!(out.len(), body.len());
                samples.push(start.elapsed());
            }
            println!(
                "decode {size_kib:>4} KiB output: median {:?}",
                median(samples)
            );
        }
    }
}
