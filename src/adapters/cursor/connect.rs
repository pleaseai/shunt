use bytes::{Bytes, BytesMut};

// Connect frame flags
pub const FLAG_GZIP: u8 = 0x01;
pub const FLAG_END: u8 = 0x02;

/// A single Connect frame with flags and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFrame {
    pub flags: u8,
    pub payload: Bytes,
}

/// Encode a payload into a Connect frame: 1 byte flags, 4 byte big-endian
/// payload length, then the payload bytes.
pub fn encode_connect_frame(payload: impl AsRef<[u8]>, flags: u8) -> Bytes {
    let payload = payload.as_ref();
    let mut out = BytesMut::with_capacity(5 + payload.len());
    out.extend_from_slice(&[flags]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.freeze()
}

/// Streaming decoder for Connect frames from a byte source.
///
/// Handles split chunks, multiple frames in a single chunk, and malformed
/// (oversized) lengths. Does NOT handle gzip decompression inline -- the
/// caller checks `FLAG_GZIP` and decompresses if desired.
///
/// End frames (FLAG_END set) with an empty or JSON payload are returned
/// as ConnectFrames. The caller inspects the payload to determine whether
/// it conveys a Connect error.
#[derive(Default)]
pub struct ConnectFrameDecoder {
    buffer: BytesMut,
}

impl ConnectFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes into the decoder. Returns all complete frames found.
    ///
    /// Returns an error if a frame header advertises a length that exceeds
    /// `max_frame_payload` (default 64 MiB).
    pub fn push(&mut self, chunk: impl AsRef<[u8]>) -> Result<Vec<ConnectFrame>, ConnectError> {
        self.buffer.extend_from_slice(chunk.as_ref());
        self.drain(64 * 1024 * 1024) // 64 MiB max payload
    }

    /// Same as `push` but with an explicit `max_payload` limit for testing.
    pub fn push_with_limit(
        &mut self,
        chunk: impl AsRef<[u8]>,
        max_payload: usize,
    ) -> Result<Vec<ConnectFrame>, ConnectError> {
        self.buffer.extend_from_slice(chunk.as_ref());
        self.drain(max_payload)
    }

    fn drain(&mut self, max_payload: usize) -> Result<Vec<ConnectFrame>, ConnectError> {
        let mut out = Vec::new();
        loop {
            if self.buffer.len() < 5 {
                break;
            }
            let len = u32::from_be_bytes([
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
                self.buffer[4],
            ]) as usize;

            if len > max_payload {
                return Err(ConnectError::PayloadTooLarge {
                    length: len,
                    max: max_payload,
                });
            }

            if self.buffer.len() < 5 + len {
                break;
            }

            let mut raw = self.buffer.split_to(5 + len);
            out.push(ConnectFrame {
                flags: raw[0],
                payload: raw.split_off(5).freeze(),
            });
        }
        Ok(out)
    }

    /// Return the number of buffered bytes (incomplete frame data).
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Assert the input ended on a frame boundary. Leftover buffered bytes are an
    /// incomplete trailing frame — i.e. the upstream body/stream was truncated —
    /// so this returns an error rather than letting the caller treat partial data
    /// as a complete response.
    pub fn finish(&self) -> Result<(), ConnectError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ConnectError::TruncatedFrame {
                buffered: self.buffer.len(),
            })
        }
    }
}

/// Maximum bytes we will decompress from a single gzipped Connect frame. Bounds
/// decompression so a malicious "zip bomb" payload cannot exhaust memory.
const MAX_DECOMPRESSED_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Compressed-size pre-filter for gzip frames that may be decoded inline.
///
/// The retained `gateway::decode_gzip_frame` benchmark measured representative
/// 1 KiB, 4 KiB, 16 KiB, and 64 KiB compressed frames at 8.804 µs, 25.4 µs,
/// 102.2 µs, and 435.9 µs median, respectively. The `spawn_blocking` round-trip
/// overhead can be reproduced separately with:
/// `cargo test -- --ignored --nocapture measure_spawn_blocking_round_trip`.
///
/// Compressed size alone cannot bound inflate work, so this is only a cheap
/// early-out for obviously large frames. [`INLINE_GZIP_OUTPUT_BYTES`] is the
/// actual inline-work safety bound.
pub(crate) const INLINE_GZIP_FRAME_BYTES: usize = 4 * 1024;

/// Maximum complete decompressed output accepted inline on a Tokio worker.
/// The bounded probe materializes one additional byte to detect larger output.
///
/// Deflate can expand by roughly 1,032:1, and inflate throughput varies by about
/// an order of magnitude with the data's redundancy, so compressed length cannot
/// bound worker time. Direct measurement puts 32 KiB of realistic data at roughly
/// 54–65 µs, within Tokio's 100 µs blocking budget; random/incompressible data is
/// faster (roughly 8 µs), not a slower worst case.
pub(crate) const INLINE_GZIP_OUTPUT_BYTES: usize = 32 * 1024;

#[cfg(test)]
pub(crate) static OFFLOADED_DECODES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static IN_FLIGHT_DECODES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static MAX_IN_FLIGHT_DECODES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct InFlightDecode;

#[cfg(test)]
impl InFlightDecode {
    fn enter() -> Self {
        let in_flight = IN_FLIGHT_DECODES.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        MAX_IN_FLIGHT_DECODES.fetch_max(in_flight, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for InFlightDecode {
    fn drop(&mut self) {
        IN_FLIGHT_DECODES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Decode at most `budget + 1` output bytes to determine whether a gzip frame is
/// small enough to finish inline. `None` means the caller must redo the complete
/// decode off-thread; malformed input remains an error.
fn decode_gzip_frame_within(
    payload: &[u8],
    budget: usize,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    use std::io::Read;
    let decoder = flate2::read::GzDecoder::new(payload);
    // Size the buffer from the compressed length at a realistic ratio, capped by
    // the budget, so a typical frame decodes without repeated growth.
    let mut out = Vec::with_capacity(std::cmp::min(payload.len() * 4, budget + 1));
    decoder.take(budget as u64 + 1).read_to_end(&mut out)?;
    Ok((out.len() <= budget).then_some(out))
}

/// Decode a small gzip frame inline, or use Tokio's bounded blocking pool path.
pub(crate) async fn decode_gzip_frame_async(payload: Bytes) -> Result<Vec<u8>, std::io::Error> {
    if payload.len() <= INLINE_GZIP_FRAME_BYTES {
        if let Some(out) = decode_gzip_frame_within(&payload, INLINE_GZIP_OUTPUT_BYTES)? {
            return Ok(out);
        }
        // Re-inflating the first 32 KiB off-thread is intentional and bounded;
        // it avoids letting compressed size masquerade as an output-work bound.
    }

    crate::adapters::cursor::offload::spawn_bounded_gzip(move || {
        #[cfg(test)]
        let _in_flight = InFlightDecode::enter();
        #[cfg(test)]
        OFFLOADED_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        decode_gzip_frame(&payload)
    })
    .await?
}

/// Decode gzipped payload bytes. The caller decides when to call this based
/// on frame flags & FLAG_GZIP.
///
/// Decompression is capped at [`MAX_DECOMPRESSED_FRAME_BYTES`] to guard against
/// zip-bomb payloads. Exceeding the cap returns an error rather than silently
/// truncating, so a partial (corrupt) protobuf payload is never returned as
/// `Ok`.
pub fn decode_gzip_frame(payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    // Read one byte past the cap so an over-limit payload is detectable rather
    // than truncated to exactly the cap.
    decoder
        .take(MAX_DECOMPRESSED_FRAME_BYTES + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > MAX_DECOMPRESSED_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "decompressed payload exceeds maximum allowed size",
        ));
    }
    Ok(out)
}

#[derive(serde::Deserialize)]
struct ConnectErrorPayload {
    error: ConnectErrorDetails,
}

#[derive(serde::Deserialize)]
struct ConnectErrorDetails {
    code: String,
    message: Option<String>,
}

/// Parse a Connect end-frame JSON error payload into a structured error.
///
/// Returns `None` if the payload is empty or not valid Connect error JSON.
///
/// Uses a lightweight struct deserializer (ignoring unknown fields) rather than
/// parsing into `serde_json::Value`, and treats `message` as optional so a
/// coded error without a message is still surfaced.
pub fn parse_connect_error(payload: &[u8]) -> Option<ConnectEndError> {
    if payload.is_empty() {
        return None;
    }
    let parsed: ConnectErrorPayload = serde_json::from_slice(payload).ok()?;
    let code = parsed.error.code;
    let message = parsed
        .error
        .message
        .unwrap_or_else(|| "Connect error".to_string());
    let status = match code.as_str() {
        // Preserve auth semantics so map_decode_error can surface 401/403 to the
        // client instead of masking them as a generic 502 Bad Gateway.
        "unauthenticated" => 401,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        // Cursor reports prompt-size failures as a Connect application code.
        // Treat them as client input errors so Claude Code can auto-compact and
        // retry instead of seeing a misleading gateway failure.
        "context_length_exceeded" => 400,
        _ => 502,
    };
    Some(ConnectEndError {
        code,
        message,
        detail: String::from_utf8_lossy(payload).into_owned(),
        status,
    })
}

#[derive(Debug, Clone)]
pub struct ConnectEndError {
    pub code: String,
    pub message: String,
    pub detail: String,
    pub status: u16,
}

impl std::fmt::Display for ConnectEndError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Connect error {}: {} ({})",
            self.status, self.message, self.code
        )
    }
}

impl std::error::Error for ConnectEndError {}

#[derive(Debug, Clone)]
pub enum ConnectError {
    PayloadTooLarge { length: usize, max: usize },
    TruncatedFrame { buffered: usize },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::PayloadTooLarge { length, max } => {
                write!(f, "Connect frame payload {length} exceeds max {max}")
            }
            ConnectError::TruncatedFrame { buffered } => {
                write!(
                    f,
                    "Cursor response truncated: {buffered} byte(s) of an incomplete frame remain"
                )
            }
        }
    }
}

impl std::error::Error for ConnectError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)] // Intentional cross-runtime test serialization.

    use super::*;
    use std::io::Write;

    fn offload_observer() -> std::sync::MutexGuard<'static, ()> {
        crate::adapters::cursor::offload::OFFLOAD_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn gzip(payload: &[u8]) -> Bytes {
        gzip_with(payload, flate2::Compression::fast())
    }

    fn gzip_with(payload: &[u8], compression: flate2::Compression) -> Bytes {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), compression);
        encoder.write_all(payload).unwrap();
        Bytes::from(encoder.finish().unwrap())
    }

    fn incompressible_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn oversized_gzip_payload() -> Bytes {
        static PAYLOAD: std::sync::OnceLock<Bytes> = std::sync::OnceLock::new();
        PAYLOAD
            .get_or_init(|| gzip(&vec![b'a'; (MAX_DECOMPRESSED_FRAME_BYTES as usize) + 1024]))
            .clone()
    }

    #[test]
    fn parses_context_overflow_as_bad_request() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "error": {
                "code": "context_length_exceeded",
                "message": "maximum context length exceeded"
            }
        }))
        .unwrap();
        let error = parse_connect_error(&payload).unwrap();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "context_length_exceeded");
    }

    #[test]
    fn encode_roundtrip() {
        let frame = encode_connect_frame(b"hello", 0);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, 0);
        assert_eq!(&frames[0].payload[..], b"hello");
    }

    #[test]
    fn encode_with_gzip_flag() {
        let frame = encode_connect_frame(b"gzip-data", FLAG_GZIP);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP);
    }

    #[test]
    fn encode_with_end_flag() {
        let frame = encode_connect_frame(b"", FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_END);
        assert!(frames[0].payload.is_empty());
    }

    #[test]
    fn encode_with_gzip_and_end_flags() {
        let payload = b"end-data";
        let frame = encode_connect_frame(payload, FLAG_GZIP | FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP | FLAG_END);
        assert_eq!(&frames[0].payload[..], payload);
    }

    #[test]
    fn multiple_frames_in_single_chunk() {
        let f1 = encode_connect_frame(b"first", 0);
        let f2 = encode_connect_frame(b"second", 0);
        let mut combined = BytesMut::new();
        combined.extend_from_slice(&f1);
        combined.extend_from_slice(&f2);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(combined).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0].payload[..], b"first");
        assert_eq!(&frames[1].payload[..], b"second");
    }

    #[test]
    fn split_chunks_are_assembled() {
        let frame = encode_connect_frame(b"split-test", 0);
        let (a, b) = frame.split_at(3);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(a).unwrap();
        assert!(frames.is_empty());

        let frames = decoder.push(b).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].payload[..], b"split-test");
    }

    #[test]
    fn split_at_header_boundary() {
        let frame = encode_connect_frame(b"split-at-5", 0);
        // Split after the flags byte but before the length bytes are complete
        let (a, b) = frame.split_at(1);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(a).unwrap();
        assert!(frames.is_empty());

        let frames = decoder.push(b).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].payload[..], b"split-at-5");
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut decoder = ConnectFrameDecoder::new();
        // Encode a frame with 1M payload (will exceed our 10-byte max)
        let oversized = encode_connect_frame(vec![0u8; 100], 0);
        let result = decoder.push_with_limit(&oversized, 10);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConnectError::PayloadTooLarge { length, max } => {
                assert_eq!(length, 100);
                assert_eq!(max, 10);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn finish_rejects_truncated_trailing_frame() {
        let mut decoder = ConnectFrameDecoder::new();
        let frame = encode_connect_frame(b"complete", 0);
        // Feed one full frame plus a partial header of a second frame.
        let mut input = frame.to_vec();
        input.extend_from_slice(&[0u8, 0, 0, 1]); // 4 bytes — short of the 5-byte header
        let frames = decoder.push(&input).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            decoder.finish(),
            Err(ConnectError::TruncatedFrame { buffered: 4 })
        ));
    }

    #[test]
    fn finish_accepts_frame_boundary() {
        let mut decoder = ConnectFrameDecoder::new();
        let frame = encode_connect_frame(b"complete", 0);
        decoder.push(&frame).unwrap();
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn empty_chunk_produces_no_frames() {
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(b"").unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn buf_returns_buffered_bytes() {
        let mut decoder = ConnectFrameDecoder::new();
        // Push part of a frame header
        decoder.push(b"\x00\x00").unwrap();
        assert_eq!(decoder.buffered(), 2);
    }

    #[test]
    fn clean_end_frame_empty_payload() {
        let frame = encode_connect_frame(b"", FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_END);
        assert!(frames[0].payload.is_empty());
        // Parse error from empty payload
        assert!(parse_connect_error(&frames[0].payload).is_none());
    }

    #[test]
    fn connect_json_error_parsing() {
        let json_err = serde_json::json!({
            "error": {
                "code": "resource_exhausted",
                "message": "quota exceeded",
                "details": []
            }
        });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let frame = encode_connect_frame(&payload, FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);

        let err = parse_connect_error(&frames[0].payload).unwrap();
        assert_eq!(err.code, "resource_exhausted");
        assert_eq!(err.status, 429);
        assert_eq!(err.message, "quota exceeded");
    }

    #[test]
    fn connect_json_unavailable_error() {
        let json_err = serde_json::json!({
            "error": {
                "code": "unavailable",
                "message": "service unavailable"
            }
        });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let err = parse_connect_error(&payload).unwrap();
        assert_eq!(err.code, "unavailable");
        assert_eq!(err.status, 502);
    }

    #[test]
    fn connect_auth_error_codes_map_to_http_auth_statuses() {
        // Auth codes must keep their 401/403 semantics so the decode-error
        // mapping surfaces them as authentication errors, not a generic 502.
        for (code, expected) in [("unauthenticated", 401), ("permission_denied", 403)] {
            let payload = serde_json::to_vec(&serde_json::json!({
                "error": {"code": code, "message": "denied"}
            }))
            .unwrap();
            let err = parse_connect_error(&payload).unwrap();
            assert_eq!(err.code, code);
            assert_eq!(err.status, expected);
        }
    }

    #[test]
    fn frame_fixture_matches_reference_layout() {
        // Connect frame: flags=0x00, length=3 (0x00000003), payload="abc"
        // Wire format: [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x62, 0x63]
        let frame = encode_connect_frame(b"abc", 0);
        assert_eq!(hex::encode(frame), "0000000003616263");
    }

    #[test]
    fn frame_fixture_with_flags() {
        // flags=0x01, length=3
        let frame = encode_connect_frame(b"xyz", 0x01);
        assert_eq!(hex::encode(frame), "010000000378797a");
    }

    #[test]
    fn gzip_frame_decompress() {
        let compressed = gzip(b"hello gzip");

        let frame = encode_connect_frame(&compressed, FLAG_GZIP);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP);

        let decompressed = decode_gzip_frame(&frames[0].payload).unwrap();
        assert_eq!(decompressed, b"hello gzip");
    }

    #[test]
    fn connect_error_without_message_still_parses() {
        // A coded error with no `message` field must still surface (not be
        // dropped by an early return).
        let json_err = serde_json::json!({ "error": { "code": "resource_exhausted" } });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let err = parse_connect_error(&payload).unwrap();
        assert_eq!(err.code, "resource_exhausted");
        assert_eq!(err.status, 429);
        assert_eq!(err.message, "Connect error");
    }

    #[test]
    fn gzip_frame_rejects_oversized_payload() {
        // A payload that decompresses beyond the cap must error rather than
        // silently truncate.
        let compressed = oversized_gzip_payload();
        let err = decode_gzip_frame(&compressed).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn gzip_decode_within_distinguishes_budget_and_corruption() {
        let within = gzip(b"within budget");
        assert_eq!(
            decode_gzip_frame_within(&within, b"within budget".len()).unwrap(),
            Some(b"within budget".to_vec())
        );

        let over = gzip(b"one byte over");
        assert_eq!(
            decode_gzip_frame_within(&over, b"one byte over".len() - 1).unwrap(),
            None
        );
        assert!(decode_gzip_frame_within(b"not-gzip", 1024).is_err());
    }

    #[tokio::test]
    async fn async_gzip_small_compressed_large_output_decodes_off_thread() {
        let _observer = offload_observer();
        let expected = vec![b'a'; 1024 * 1024];
        let compressed = gzip_with(&expected, flate2::Compression::best());
        assert!(compressed.len() <= INLINE_GZIP_FRAME_BYTES);
        assert_eq!(
            decode_gzip_frame_within(&compressed, INLINE_GZIP_OUTPUT_BYTES).unwrap(),
            None
        );

        let before = OFFLOADED_DECODES.load(std::sync::atomic::Ordering::Relaxed);
        let decoded = decode_gzip_frame_async(compressed).await.unwrap();
        assert_eq!(decoded, expected);
        // The observer lock attributes this exact increment to this decode rather
        // than a concurrently running async gzip test.
        assert_eq!(
            OFFLOADED_DECODES.load(std::sync::atomic::Ordering::Relaxed),
            before + 1,
            "large-output frame must execute through spawn_blocking"
        );
    }

    #[tokio::test]
    async fn async_gzip_below_threshold_matches_sync_decode() {
        let _observer = offload_observer();
        let expected = b"small gzip response frame";
        let compressed = gzip(expected);
        assert!(compressed.len() <= INLINE_GZIP_FRAME_BYTES);

        let sync = decode_gzip_frame(&compressed).unwrap();
        let before = OFFLOADED_DECODES.load(std::sync::atomic::Ordering::Relaxed);
        let asynchronous = decode_gzip_frame_async(compressed).await.unwrap();
        assert_eq!(asynchronous, sync);
        assert_eq!(asynchronous, expected);
        assert_eq!(
            OFFLOADED_DECODES.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "small-output frame must stay on the inline decode path"
        );
    }

    #[tokio::test]
    async fn async_gzip_above_threshold_matches_sync_decode() {
        let _observer = offload_observer();
        let expected = incompressible_bytes(INLINE_GZIP_FRAME_BYTES * 2);
        let compressed = gzip(&expected);
        assert!(compressed.len() > INLINE_GZIP_FRAME_BYTES);

        let sync = decode_gzip_frame(&compressed).unwrap();
        let asynchronous = decode_gzip_frame_async(compressed).await.unwrap();
        assert_eq!(asynchronous, sync);
        assert_eq!(asynchronous, expected);
    }

    #[tokio::test]
    async fn gzip_decode_proceeds_while_request_prep_is_saturated() {
        let _observer = offload_observer();
        let gzip_slots = crate::adapters::cursor::offload::gzip_slots();
        let request_prep_slots = crate::adapters::cursor::offload::request_prep_slots();
        let gzip_capacity = gzip_slots.available_permits();
        let request_prep_capacity = request_prep_slots.available_permits();
        assert!(gzip_capacity > 0);
        assert!(request_prep_capacity > 0);

        let held_request_prep = request_prep_slots
            .acquire_many(request_prep_capacity as u32)
            .await
            .expect("request-preparation semaphore should remain open");
        assert_eq!(request_prep_slots.available_permits(), 0);

        let expected = incompressible_bytes(INLINE_GZIP_FRAME_BYTES * 2);
        let compressed = gzip(&expected);
        assert!(compressed.len() > INLINE_GZIP_FRAME_BYTES);
        let decoded = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            decode_gzip_frame_async(compressed),
        )
        .await
        .expect("gzip decode should proceed while request preparation is saturated")
        .expect("gzip payload should decode");
        assert_eq!(decoded, expected);

        drop(held_request_prep);
        assert_eq!(gzip_slots.available_permits(), gzip_capacity);
        assert_eq!(
            request_prep_slots.available_permits(),
            request_prep_capacity
        );
    }

    #[tokio::test]
    async fn async_gzip_rejects_oversized_payload() {
        let _observer = offload_observer();
        let compressed = oversized_gzip_payload();
        assert!(compressed.len() > INLINE_GZIP_FRAME_BYTES);

        let err = decode_gzip_frame_async(compressed).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn async_gzip_rejects_corruption_on_both_paths() {
        let _observer = offload_observer();
        let inline = Bytes::from_static(b"not-gzip");
        assert!(inline.len() <= INLINE_GZIP_FRAME_BYTES);
        assert!(decode_gzip_frame_async(inline).await.is_err());

        let mut blocking = gzip(&incompressible_bytes(INLINE_GZIP_FRAME_BYTES * 2));
        blocking.truncate(blocking.len() - 1);
        assert!(blocking.len() > INLINE_GZIP_FRAME_BYTES);
        assert!(decode_gzip_frame_async(blocking).await.is_err());
    }

    #[tokio::test]
    async fn async_gzip_offloads_never_exceed_slot_limit() {
        let _observer = offload_observer();
        let slots = crate::adapters::cursor::offload::gzip_slots().available_permits();
        MAX_IN_FLIGHT_DECODES.store(0, std::sync::atomic::Ordering::SeqCst);
        let compressed = gzip(&incompressible_bytes(INLINE_GZIP_FRAME_BYTES * 16));
        assert!(compressed.len() > INLINE_GZIP_FRAME_BYTES);

        let decodes = (0..slots * 4).map(|_| decode_gzip_frame_async(compressed.clone()));
        let results = futures_util::future::join_all(decodes).await;
        assert!(results.into_iter().all(|result| result.is_ok()));

        let observed = MAX_IN_FLIGHT_DECODES.load(std::sync::atomic::Ordering::SeqCst);
        // This is a one-sided safety bound, not proof that scheduling exercised
        // every slot; a non-overlapping run may legitimately observe only one.
        assert!(
            observed <= slots,
            "observed {observed} decodes for {slots} slots"
        );
    }

    /// Reproduce the native blocking-pool scheduling and join calibration used by
    /// [`INLINE_GZIP_FRAME_BYTES`]. Ignored because wall-clock thread wakeups are
    /// intentionally unsuitable for CodSpeed's instruction-counting simulation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn measure_spawn_blocking_round_trip() {
        const SAMPLES: usize = 1_000;

        tokio::task::spawn_blocking(|| {}).await.unwrap();
        let started = std::time::Instant::now();
        for _ in 0..SAMPLES {
            tokio::task::spawn_blocking(|| {}).await.unwrap();
        }
        let elapsed = started.elapsed();
        println!(
            "spawn_blocking round-trip: {:?} mean over {SAMPLES} samples",
            elapsed / SAMPLES as u32
        );
    }
}
