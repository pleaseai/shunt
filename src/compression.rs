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
//! Compression and decompression are CPU-bound, so anything past
//! [`INLINE_ZSTD_BYTES`] runs on Tokio's blocking pool under bounded admission
//! rather than on the async executor (same discipline as Cursor's framing/gzip
//! work, `adapters::cursor::offload`).

use axum::http::{header::CONTENT_ENCODING, HeaderMap};
use bytes::Bytes;

/// The zstd level codex compresses Responses request bodies at
/// (`zstd::stream::encode_all(.., 3)`), which is also zstd's own default: near
/// gzip-level ratios at several hundred MB/s.
const ZSTD_LEVEL: i32 = 3;

/// Bodies up to this size are compressed (or decoded) inline on the async
/// executor; anything larger is offloaded. Level-3 zstd runs at hundreds of MB/s,
/// so 64 KiB is well under a millisecond of work — cheaper to do inline than to
/// pay a `spawn_blocking` hop for. Matches the Cursor request path's inline
/// budget (`adapters::cursor::INLINE_IMAGE_DECODE_BYTES`).
pub(crate) const INLINE_ZSTD_BYTES: usize = 64 * 1024;

/// Bodies smaller than this are sent uncompressed: at a few hundred bytes the
/// frame header plus a poor ratio on a short, low-redundancy body can leave the
/// request no smaller, and the backend accepts both encodings. Every real turn
/// (instructions + tool schemas + history) is far above this, so the gate only
/// spares degenerate bodies.
pub(crate) const MIN_COMPRESS_BYTES: usize = 1024;

/// Admission slots for zstd request compression and inbound label decoding.
///
/// One task per upstream request attempt, at most one attempt per turn (the
/// prepared bytes are reused across retries and account rotations). Kept separate
/// from the Cursor pools so a burst on one provider's path cannot delay another's.
///
/// A permit bounds one in-progress task and that task's working set, not total
/// resident memory: queued inputs and completed outputs remain resident outside
/// the permit.
fn zstd_slots() -> &'static tokio::sync::Semaphore {
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
/// [`MIN_COMPRESS_BYTES`] and is better sent as-is. Compression past
/// [`INLINE_ZSTD_BYTES`] is offloaded to the blocking pool.
pub(crate) async fn compress_request_body(body: Bytes) -> std::io::Result<Option<Bytes>> {
    if body.len() < MIN_COMPRESS_BYTES {
        return Ok(None);
    }
    if body.len() <= INLINE_ZSTD_BYTES {
        return compress(&body).map(Some);
    }
    crate::offload::spawn_bounded(zstd_slots(), move || compress(&body))
        .await?
        .map(Some)
}

/// Decode a zstd body whose decoded form is at most `budget` bytes, or
/// `Ok(None)` when it decodes to more than that. Decoding past
/// [`INLINE_ZSTD_BYTES`] of *input* is offloaded to the blocking pool.
pub(crate) async fn decode_zstd_within(
    body: Bytes,
    budget: usize,
) -> std::io::Result<Option<Bytes>> {
    if body.len() <= INLINE_ZSTD_BYTES {
        return decode_within(&body, budget);
    }
    crate::offload::spawn_bounded(zstd_slots(), move || decode_within(&body, budget)).await?
}

fn compress(body: &[u8]) -> std::io::Result<Bytes> {
    zstd::stream::encode_all(body, ZSTD_LEVEL).map(Bytes::from)
}

/// Decode `body`, giving up with `Ok(None)` rather than allocating past `budget`.
/// The decoder reads one byte beyond the budget so an over-budget body is
/// detected instead of silently truncated into invalid JSON — a compressed body
/// must never be able to make shunt buffer more than the caller's own cap allows.
fn decode_within(body: &[u8], budget: usize) -> std::io::Result<Option<Bytes>> {
    use std::io::Read;

    let mut out = Vec::new();
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

    /// A JSON-shaped body of `len` bytes: compressible like a real request body
    /// (repeated keys and structure), so a ratio assertion is meaningful.
    fn json_body(len: usize) -> Bytes {
        let mut body = String::from("{\"model\":\"gpt-5.2-codex\",\"input\":[");
        while body.len() < len {
            body.push_str("{\"role\":\"user\",\"content\":\"hello there\"},");
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
    async fn compresses_and_round_trips_an_inline_body() {
        let body = json_body(8 * 1024);
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

    /// A body past the inline threshold takes the blocking-pool path and still
    /// round-trips identically.
    #[tokio::test]
    async fn compresses_an_offloaded_body() {
        let body = json_body(INLINE_ZSTD_BYTES * 2);
        assert!(body.len() > INLINE_ZSTD_BYTES);
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
    /// needs an input that stays large after compression — an incompressible one.
    #[tokio::test]
    async fn decodes_an_offloaded_body() {
        // xorshift64 output: enough entropy that zstd cannot shrink it, unlike a
        // low-period arithmetic sequence (mirrors the Cursor gzip fixtures).
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        let body: Bytes = (0..INLINE_ZSTD_BYTES * 2)
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
            compressed.len() > INLINE_ZSTD_BYTES,
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
}
