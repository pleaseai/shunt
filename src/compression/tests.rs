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
/// Responses-request JSON. The one-shot `encode_all` figures are what
/// establish that no inline compression threshold is worth having (see
/// [`compress_request_body`]); the `zstd::bulk::Compressor` figures isolate
/// how much of that fixed cost is the one-shot API's context setup rather
/// than zstd itself (also documented on [`compress_request_body`] — measured
/// and deliberately not adopted); the decode figures size
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

    println!("-- compress via a reused zstd::bulk::Compressor (context reuse) --");
    for size_kib in [1, 2, 4, 8, 16, 32, 64, 128] {
        let body = json_body(size_kib * 1024);
        let mut compressor =
            zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("compressor should build");
        for _ in 0..WARMUP_ITERATIONS {
            let _ = compressor
                .compress(&body)
                .expect("compression should succeed");
        }
        let mut samples = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let start = Instant::now();
            let _ = compressor
                .compress(&body)
                .expect("compression should succeed");
            samples.push(start.elapsed());
        }
        println!(
            "bulk::Compressor {size_kib:>4} KiB input: median {:?}",
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
