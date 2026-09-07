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

use axum::{body::Bytes, http::HeaderMap};
use serde_json::Value;

use crate::compression::BodyEncoding;

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
/// the client asked for; the body is then only decoded, never re-serialized, so
/// a route that merely redirects a model reaches the upstream byte-for-byte as
/// the client wrote it.
///
/// The zstd branch fuses the decode with the rewrite inside one bounded
/// blocking task via [`crate::compression::decode_zstd_and_parse`], for the same
/// reason the model label does: the decoded body can be large enough that
/// parsing it is itself worker-blocking work, so it must run under the decode's
/// admission permit rather than on the async executor afterwards.
///
/// A content coding shunt cannot decode fails the request rather than
/// forwarding the opaque bytes: the routed path never forwards
/// `content-encoding`, so relaying a body shunt cannot turn into identity bytes
/// would hand the upstream a payload it has no way to read.
pub(super) async fn identity_body(
    headers: &HeaderMap,
    body: &Bytes,
    rewrite_model: Option<&str>,
    max_request_bytes: usize,
) -> Result<Bytes, BodyError> {
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
        BodyEncoding::Identity => apply_model(body.clone(), rewrite_model),
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

/// Replace the decoded body's top-level `model`, or hand the decoded bytes back
/// untouched when there is nothing to rewrite.
///
/// Runs inside the zstd branch's blocking task, so it takes and returns owned
/// [`Bytes`] rather than borrowing the decoded buffer.
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
        let out = identity_body(&HeaderMap::new(), &body, Some("gpt-5.6-sol"), LIMIT)
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
        let out = identity_body(&HeaderMap::new(), &body, None, LIMIT)
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

        let out = identity_body(&zstd_headers(), &compressed, None, LIMIT)
            .await
            .unwrap_or_else(|_| panic!("a zstd body should decode"));
        assert_eq!(out, plain);

        let rewritten = identity_body(&zstd_headers(), &compressed, Some("gpt-5.6-sol"), LIMIT)
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
            identity_body(&headers, &Bytes::from_static(b"{}"), None, LIMIT).await,
            Err(BodyError::Invalid)
        ));
    }
}
