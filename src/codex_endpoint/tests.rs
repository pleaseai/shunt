use super::{model_label, pool_sticky_key, UNKNOWN_MODEL};
use axum::{
    body::Bytes,
    http::{header::CONTENT_ENCODING, HeaderMap},
};

/// A body big enough that `compress_request_body` does not skip it, shaped
/// like the real inbound Responses request (`model` first, then the turn).
fn request_body(model: &str) -> Bytes {
    let filler = "conversation history ".repeat(200);
    Bytes::from(
        serde_json::json!({
            "model": model,
            "input": [{"role": "user", "content": filler}],
        })
        .to_string(),
    )
}

fn zstd_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, "zstd".parse().unwrap());
    headers
}

/// Real gzip-compressed bytes, so a fixture claiming `content-encoding: gzip`
/// is genuinely unparseable as plain JSON rather than happening to be valid
/// JSON that a naive fallback would accidentally read anyway.
fn gzip_compress(body: &[u8]) -> Bytes {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body).expect("gzip encode should succeed");
    Bytes::from(encoder.finish().expect("gzip encode should succeed"))
}

#[tokio::test]
async fn reads_the_model_from_an_uncompressed_body() {
    assert_eq!(
        model_label(&HeaderMap::new(), &request_body("gpt-5.2-codex")).await,
        "gpt-5.2-codex"
    );
}

/// Current Codex releases zstd-compress the request body on the
/// `chatgpt_base_url` client shape (issue #285). Before decoding, the label
/// parse failed silently and every metric/log/span for the request was
/// labeled `unknown`.
#[tokio::test]
async fn reads_the_model_from_a_zstd_body() {
    let body = crate::compression::compress_request_body(request_body("gpt-5.2-codex"))
        .await
        .expect("compression should succeed")
        .expect("the fixture should be large enough to compress");

    assert_eq!(model_label(&zstd_headers(), &body).await, "gpt-5.2-codex");
}

/// A body that claims `zstd` but cannot be decoded degrades to the `unknown`
/// label — the request itself still relays verbatim.
#[tokio::test]
async fn falls_back_to_unknown_for_an_undecodable_zstd_body() {
    let body = request_body("gpt-5.2-codex");
    assert_eq!(model_label(&zstd_headers(), &body).await, UNKNOWN_MODEL);
}

/// A content coding shunt does not decode falls through to a best-effort
/// plain parse (B5): since this fixture's bytes are genuinely
/// gzip-compressed (not valid JSON), the parse still fails and the label
/// still degrades to `unknown` — but for the real reason, not because
/// `Other` was rejected unconditionally. An unconditional rejection would
/// let a client suppress its own model label by sending a bogus
/// `content-encoding` header on an otherwise-plain, parseable body.
#[tokio::test]
async fn falls_back_to_unknown_for_an_unsupported_content_encoding() {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
    let body = gzip_compress(request_body("gpt-5.2-codex").as_ref());
    assert_eq!(model_label(&headers, &body).await, UNKNOWN_MODEL);
}

/// A body claiming an unsupported content-encoding, but that is in fact
/// plain, parseable JSON, still yields its `model` — proving the fallback
/// added for B5 actually reads through rather than only changing the log
/// line.
#[tokio::test]
async fn reads_the_model_despite_an_unsupported_content_encoding_label() {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
    assert_eq!(
        model_label(&headers, &request_body("gpt-5.2-codex")).await,
        "gpt-5.2-codex"
    );
}

#[tokio::test]
async fn falls_back_to_unknown_without_a_model_field() {
    let body = Bytes::from_static(b"{\"input\":[]}");
    assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
}

/// A `model` field present but not a string (B1) degrades to `unknown` just
/// like a missing field, rather than panicking or silently stringifying it.
#[tokio::test]
async fn falls_back_to_unknown_when_the_model_field_is_not_a_string() {
    let body = Bytes::from_static(b"{\"model\":42,\"input\":[]}");
    assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
}

/// Malformed JSON (not merely an unreadable `model`) degrades to `unknown`
/// (B1) rather than propagating a parse error to the caller — the body still
/// forwards verbatim regardless.
#[tokio::test]
async fn falls_back_to_unknown_for_malformed_json() {
    let body = Bytes::from_static(b"not json at all");
    assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
}

#[test]
fn prefixes_the_authenticated_client() {
    assert_eq!(
        pool_sticky_key(Some("alice"), Some("sess-1".to_string())),
        Some("alice:sess-1".to_string())
    );
}

#[test]
fn distinguishes_clients_sharing_a_session_id() {
    // Two tenants replaying the same `session-id` must not collide on the pool,
    // so one cannot pin another's session onto a chosen account.
    let alice = pool_sticky_key(Some("alice"), Some("shared".to_string()));
    let bob = pool_sticky_key(Some("bob"), Some("shared".to_string()));
    assert_ne!(alice, bob);
}

#[test]
fn falls_back_to_the_bare_session_without_auth() {
    assert_eq!(
        pool_sticky_key(None, Some("sess-1".to_string())),
        Some("sess-1".to_string())
    );
}

#[test]
fn is_none_without_a_session_id() {
    assert_eq!(pool_sticky_key(Some("alice"), None), None);
    assert_eq!(pool_sticky_key(None, None), None);
}
