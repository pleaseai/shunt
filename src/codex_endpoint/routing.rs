//! Preparing the request body for a matched `[[server.codex_endpoint.routes]]`
//! entry (issue #436).
//!
//! Two things separate a routed request from the fixed-provider passthrough:
//! the body `model` may have to be replaced with the route's `upstream_model`,
//! and a third-party upstream has to receive the body **identity-encoded** (the
//! zstd request encoding is a ChatGPT/Codex-backend convention that a stock
//! Responses API does not accept). Both are the same operation — materialize
//! the decoded JSON once and hand back plain bytes — so they share one entry
//! point, [`identity_body`].

use std::sync::OnceLock;

use axum::{body::Bytes, http::HeaderMap};
use serde_json::Value;

use crate::compression::{BodyEncoding, INLINE_ZSTD_OUTPUT_BYTES};

/// Admission slots for the routed-body rewrite (parse + `model` replacement +
/// re-serialize). Its own class, per `offload`'s module doc: a burst of large
/// routed rewrites must not consume the decode pool's permits and stall every
/// zstd request on the gateway, and vice versa.
///
/// A permit bounds *concurrency*, not the size of any one rewrite — that is
/// bounded upstream by `max_request_bytes` and, on the zstd path, by the
/// compressed-to-decoded ratio.
fn rewrite_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(crate::offload::cpu_sized_semaphore)
}

/// Why a routed request's body could not be prepared. Both arms are
/// gateway-owned failures the caller turns into a response; on the inbound
/// Codex endpoint they are re-shaped into the OpenAI error envelope at `post`.
pub(super) enum BodyError {
    /// The zstd body decodes past the request size limit or the
    /// compressed-to-decoded ratio bound — the same 413 the label path reports.
    TooLarge,
    /// The body could not be decoded, is not JSON, or is not a JSON object, so
    /// its `model` cannot be rewritten. Unlike the metrics label (which
    /// degrades to `unknown`), this blocks the request: shunt would otherwise
    /// send a third-party upstream a body naming a model it does not serve.
    Invalid,
}

/// Materialize the routed request body as **identity-encoded** bytes, replacing
/// the top-level `model` with `rewrite_model` when one is given.
///
/// `rewrite_model` is `None` when the route's `upstream_model` equals the model
/// the client asked for; the body is then never re-serialized, so a route that
/// merely redirects a model reaches the upstream byte-for-byte as the client
/// wrote it.
///
/// `decoded` short-circuits the decode entirely. `super::model::resolve_model`
/// already decoded a zstd body inside a bounded blocking task to read `model`,
/// and that same buffer is exactly what this function would otherwise produce —
/// so when the caller hands it over, this decodes nothing at all, whatever
/// `content-encoding` claims (PR #478 review, P2). The branches below are the
/// fallback for when there is nothing to reuse.
///
/// The rewrite itself never runs unbounded on the async executor. Inside the
/// zstd fallback it is fused with the decode in that one bounded blocking task
/// (`decode_zstd_and_parse`). Everywhere else — the reuse path and the identity
/// fallback — it runs inline only for a body within
/// [`INLINE_ZSTD_OUTPUT_BYTES`], the same calibrated gate the decode uses for
/// its own inline fast path, and is otherwise offloaded to the blocking pool
/// under [`rewrite_slots`]: a `serde_json` round trip over a multi-megabyte
/// client-controlled document is milliseconds of worker-blocking work, far past
/// Tokio's ~100 µs budget (PR #478 review, P1). With no rewrite there is no
/// parse, so there is nothing to offload and the bytes return directly.
///
/// A content coding shunt cannot decode fails the request rather than
/// forwarding the opaque bytes: the routed path never forwards
/// `content-encoding`, so relaying a body shunt cannot turn into identity bytes
/// would hand the upstream a payload it has no way to read.
pub(super) async fn identity_body(
    headers: &HeaderMap,
    body: &Bytes,
    decoded: Option<Bytes>,
    rewrite_model: Option<&str>,
    max_request_bytes: usize,
) -> Result<Bytes, BodyError> {
    if let Some(decoded) = decoded {
        return apply_model_bounded(decoded, rewrite_model).await;
    }
    match crate::compression::body_encoding(headers) {
        BodyEncoding::Zstd => {
            let rewrite_model = rewrite_model.map(ToOwned::to_owned);
            match crate::compression::decode_zstd_and_parse(
                body.clone(),
                max_request_bytes,
                move |decoded| apply_model(decoded, rewrite_model.as_deref()),
            )
            .await
            {
                Ok(Some(result)) => result,
                Ok(None) => {
                    tracing::warn!(
                        wire_bytes = body.len(),
                        limit = max_request_bytes,
                        "routed inbound codex body decodes past the request size limit or the \
                         compressed-to-decoded ratio bound"
                    );
                    Err(BodyError::TooLarge)
                }
                Err(error) => {
                    // A libzstd-authored message (allocation/format failure),
                    // not client-controlled content — safe to log verbatim.
                    tracing::warn!(
                        wire_bytes = body.len(),
                        error = %error,
                        "failed to decode zstd inbound codex body for a routed request"
                    );
                    Err(BodyError::Invalid)
                }
            }
        }
        BodyEncoding::Identity => apply_model_bounded(body.clone(), rewrite_model).await,
        BodyEncoding::Other => {
            tracing::warn!(
                content_encoding = ?headers.get(axum::http::header::CONTENT_ENCODING),
                "routed inbound codex body uses an unsupported content-encoding; \
                 it cannot be re-encoded as identity for a routed upstream"
            );
            Err(BodyError::Invalid)
        }
    }
}

/// [`apply_model`] under bounded admission: inline for a body within
/// [`INLINE_ZSTD_OUTPUT_BYTES`], on the blocking pool under [`rewrite_slots`]
/// above it.
///
/// Only the rewriting case is ever offloaded — without a `rewrite_model` there
/// is no parse and no serialize, just the bytes themselves, so the gate is
/// skipped rather than paying a permit and a task hop to return them.
///
/// A `spawn_bounded` failure (a closed semaphore, or a panic inside the task)
/// is reported as [`BodyError::Invalid`]: the body could not be prepared, which
/// is precisely what that arm means to the caller.
async fn apply_model_bounded(
    decoded: Bytes,
    rewrite_model: Option<&str>,
) -> Result<Bytes, BodyError> {
    let Some(rewrite_model) = rewrite_model else {
        return Ok(decoded);
    };
    if decoded.len() <= INLINE_ZSTD_OUTPUT_BYTES {
        return apply_model(decoded, Some(rewrite_model));
    }
    let rewrite_model = rewrite_model.to_string();
    crate::offload::spawn_bounded(rewrite_slots(), move || {
        apply_model(decoded, Some(&rewrite_model))
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "failed to offload the routed inbound codex body rewrite"
        );
        Err(BodyError::Invalid)
    })
}

/// Replace the decoded body's top-level `model`, or hand the decoded bytes back
/// untouched when there is nothing to rewrite.
///
/// Runs inside a blocking task — the zstd branch's decode task, or the one
/// [`apply_model_bounded`] spawns — so it takes and returns owned [`Bytes`]
/// rather than borrowing the decoded buffer.
///
/// The parse error is deliberately not logged: `serde_json::Error`'s `Display`
/// embeds the offending value, so recording it would echo the
/// client-controlled request body into `warn!` (and from there into Sentry
/// breadcrumbs and the OTel logs bridge) — the same rule `model_from_parsed`
/// documents at length.
fn apply_model(decoded: Bytes, rewrite_model: Option<&str>) -> Result<Bytes, BodyError> {
    let Some(rewrite_model) = rewrite_model else {
        return Ok(decoded);
    };
    let mut value: Value = serde_json::from_slice(&decoded).map_err(|error| {
        tracing::warn!(
            decoded_bytes = decoded.len(),
            error_line = error.line(),
            error_column = error.column(),
            error_kind = ?error.classify(),
            "routed inbound codex body is not valid JSON; cannot rewrite its `model`"
        );
        BodyError::Invalid
    })?;
    let Some(object) = value.as_object_mut() else {
        tracing::warn!(
            decoded_bytes = decoded.len(),
            "routed inbound codex body is not a JSON object; cannot rewrite its `model`"
        );
        return Err(BodyError::Invalid);
    };
    object.insert(
        "model".to_string(),
        Value::String(rewrite_model.to_string()),
    );
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                "failed to re-serialize the routed inbound codex body"
            );
            BodyError::Invalid
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 32 * 1024 * 1024;

    fn zstd_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_ENCODING,
            "zstd".parse().unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn rewrites_the_model_and_keeps_every_other_field() {
        let body =
            Bytes::from_static(br#"{"model":"glm-5.3","instructions":"be brief","stream":true}"#);
        let out = identity_body(&HeaderMap::new(), &body, None, Some("gpt-5.6-sol"), LIMIT)
            .await
            .unwrap_or_else(|_| panic!("a plain JSON object should rewrite"));
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "gpt-5.6-sol");
        assert_eq!(value["instructions"], "be brief");
        assert_eq!(value["stream"], true);
    }

    #[tokio::test]
    async fn returns_a_plain_body_untouched_without_a_rewrite() {
        let body = Bytes::from_static(br#"{"model":"glm-5.3"}"#);
        let out = identity_body(&HeaderMap::new(), &body, None, None, LIMIT)
            .await
            .unwrap_or_else(|_| panic!("a plain body needs no work"));
        assert_eq!(out, body);
    }

    #[tokio::test]
    async fn decodes_a_zstd_body_to_identity_bytes() {
        let plain = Bytes::from(
            serde_json::json!({"model": "glm-5.3", "input": "conversation history ".repeat(200)})
                .to_string(),
        );
        let compressed = crate::compression::compress_request_body(plain.clone())
            .await
            .unwrap()
            .expect("the fixture should be large enough to compress");

        let out = identity_body(&zstd_headers(), &compressed, None, None, LIMIT)
            .await
            .unwrap_or_else(|_| panic!("a zstd body should decode"));
        assert_eq!(out, plain);

        let rewritten = identity_body(
            &zstd_headers(),
            &compressed,
            None,
            Some("gpt-5.6-sol"),
            LIMIT,
        )
        .await
        .unwrap_or_else(|_| panic!("a zstd body should decode and rewrite"));
        let value: Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["model"], "gpt-5.6-sol");
    }

    #[tokio::test]
    async fn rejects_a_body_that_is_not_a_json_object() {
        for body in [&b"not json"[..], &b"[1,2,3]"[..]] {
            assert!(matches!(
                identity_body(
                    &HeaderMap::new(),
                    &Bytes::from_static(body),
                    None,
                    Some("m"),
                    LIMIT
                )
                .await,
                Err(BodyError::Invalid)
            ));
        }
    }

    #[tokio::test]
    async fn rejects_an_undecodable_content_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_ENCODING,
            "gzip".parse().unwrap(),
        );
        assert!(matches!(
            identity_body(&headers, &Bytes::from_static(b"{}"), None, None, LIMIT).await,
            Err(BodyError::Invalid)
        ));
    }
    /// A rewrite over a body past the inline gate takes the offloaded path
    /// (`apply_model_bounded` -> `spawn_bounded`), which must produce exactly
    /// the same result as the inline one — the gate is about *where* the parse
    /// runs, never about what it produces.
    #[tokio::test]
    async fn rewrites_a_body_larger_than_the_inline_gate() {
        let filler = "x".repeat(200 * 1024);
        let body = Bytes::from(
            serde_json::json!({"model": "glm-5.3", "input": filler, "stream": true}).to_string(),
        );
        assert!(
            body.len() > INLINE_ZSTD_OUTPUT_BYTES,
            "the fixture must exceed the inline gate to exercise the offload"
        );

        let out = identity_body(&HeaderMap::new(), &body, None, Some("gpt-5.6-sol"), LIMIT)
            .await
            .unwrap_or_else(|_| panic!("a large JSON object should rewrite"));

        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "gpt-5.6-sol");
        assert_eq!(value["stream"], true);
        assert_eq!(value["input"].as_str().unwrap().len(), 200 * 1024);
    }

    /// A supplied `decoded` skips the decode entirely, whatever
    /// `content-encoding` claims. Proven the only way it can be: the `body` is
    /// garbage that no zstd decode could survive, so a successful rewrite is
    /// possible only if that decode never ran.
    #[tokio::test]
    async fn reuses_a_supplied_decoded_body_instead_of_decoding() {
        let garbage = Bytes::from_static(b"this is not zstd and never was");
        let decoded = Bytes::from_static(br#"{"model":"glm-5.3","instructions":"be brief"}"#);

        let out = identity_body(
            &zstd_headers(),
            &garbage,
            Some(decoded),
            Some("gpt-5.6-sol"),
            LIMIT,
        )
        .await
        .unwrap_or_else(|_| panic!("the supplied decoded body should be used as-is"));

        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "gpt-5.6-sol");
        assert_eq!(value["instructions"], "be brief");

        // Control: without the reuse, the same call fails on the decode.
        assert!(matches!(
            identity_body(&zstd_headers(), &garbage, None, Some("gpt-5.6-sol"), LIMIT).await,
            Err(BodyError::Invalid)
        ));
    }
}
