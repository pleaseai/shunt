//! Reading the inbound Responses body's `model`: the label every metric, log
//! line, and span for the request carries, and — since issue #436 — the key
//! `[[server.codex_endpoint.routes]]` is matched against.
//!
//! The two uses have different tolerances, which is why [`resolve_model`]
//! returns an `Option` rather than a label string: a body whose `model` cannot
//! be read is labeled [`UNKNOWN_MODEL`] but must never *match* a route, since
//! the sentinel is a shunt-authored string an operator could otherwise capture
//! by declaring a route for the literal model `unknown`.

use axum::{body::Bytes, http::HeaderMap};
use serde::Deserialize;

use crate::compression::BodyEncoding;

/// Minimal view of the inbound Responses body: the `model` is read only for
/// metrics/logging labels — the body itself forwards upstream byte-for-byte, so
/// a missing or malformed model never blocks the request (the upstream rejects it).
/// `model` is deserialized as a [`ModelField`] rather than `Option<String>` so
/// [`parse_model`] can tell "field absent" apart from "field present but not a
/// string" instead of both silently becoming `None`.
#[derive(Debug, Deserialize)]
struct ModelView {
    model: Option<ModelField>,
}

/// What the inbound body's `model` field turned out to be, classified *without*
/// materializing it.
///
/// Deliberately not `serde_json::Value`: only a string is ever used, and every
/// other shape is used solely to name the type in a log line. Deserializing into
/// a `Value` would make serde allocate and retain the field's entire contents
/// first — so a client sending `"model": [ ...megabytes... ]` would turn this
/// best-effort labels-only parse into a large client-controlled heap allocation,
/// on top of the arrival buffer and (on the zstd path) the decoded copy that are
/// already resident (issue #291 follow-up). The non-string arms below drain their
/// contents through [`IgnoredAny`], which walks the input without building it.
#[derive(Debug)]
enum ModelField {
    Str(String),
    /// A JSON type name (`"array"`, `"object"`, ...) — never any client content.
    Other(&'static str),
}

impl<'de> Deserialize<'de> for ModelField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ModelFieldVisitor)
    }
}

struct ModelFieldVisitor;

impl<'de> serde::de::Visitor<'de> for ModelFieldVisitor {
    type Value = ModelField;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(ModelField::Str(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(ModelField::Str(value))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(ModelField::Other("boolean"))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(ModelField::Other("number"))
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(ModelField::Other("number"))
    }

    fn visit_i128<E>(self, _: i128) -> Result<Self::Value, E> {
        Ok(ModelField::Other("number"))
    }

    fn visit_u128<E>(self, _: u128) -> Result<Self::Value, E> {
        Ok(ModelField::Other("number"))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(ModelField::Other("number"))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(ModelField::Other("null"))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        // Drain rather than collect: the elements are never read, and building
        // them is the allocation this type exists to avoid.
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(ModelField::Other("array"))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while map
            .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
            .is_some()
        {}
        Ok(ModelField::Other("object"))
    }
}

/// The label used when the request's model cannot be read (see
/// [`resolve_model`]). Never matched against a configured route.
pub(super) const UNKNOWN_MODEL: &str = "unknown";

/// Read the `model` for metrics/logging labels only — the body itself forwards
/// upstream byte-for-byte, so a body this cannot read never blocks the request
/// (the upstream rejects it).
///
/// Current Codex releases zstd-compress the Responses request body whenever both
/// of their gates pass, which includes the documented `chatgpt_base_url` client
/// shape pointed at this endpoint (issue #285). The compressed bytes relay
/// upstream fine — `content-encoding` is forwarded verbatim — but a plain
/// `from_slice` on them fails, which would silently label every metric, log line,
/// and span for the request `unknown`. So decode a zstd body for the label, and
/// log (rather than swallow) anything that still leaves the model unreadable.
///
/// [`MAX_REQUEST_BODY_BYTES`] is passed as [`decode_zstd_and_parse`]'s `cap`, the
/// same absolute limit this endpoint already applies to the arrival buffer — so
/// the arrival buffer and the decoded copy can be transiently resident together,
/// at worst two buffers each up to that cap (not one, as compressing surely
/// shrinks the wire size). What actually bounds the *decode work itself* for a
/// small, hostile body is `compression::MAX_DECODE_RATIO`, not this cap: it ties
/// worst-case decoded size to a multiple of what the peer actually uploaded
/// (issue #291). A small absolute cap here instead would be unsound for the
/// opposite reason — `serde_json::from_slice` needs a *complete* document, so any
/// truncation-style cap below a real turn's size would silently relabel every
/// large legitimate turn `unknown`, regressing issue #285's fix. The ratio bound
/// is what makes keeping the large absolute cap here safe.
///
/// The zstd branch fuses the decode with the `model` extraction inside one
/// bounded blocking task via [`decode_zstd_and_parse`], rather than decoding to
/// a [`Bytes`] here and parsing it afterward on the async executor: the decoded
/// body can be as large as [`MAX_REQUEST_BODY_BYTES`] (a ~1 MiB compressed
/// upload already buys a 64 MiB budget via the ratio bound), and a
/// `serde_json::from_slice` over a document that size is itself worker-blocking
/// work — a 400 KiB document alone is already milliseconds, far past Tokio's
/// ~100 µs budget. Doing both inside the same blocking task means the admission
/// permit covers the parse too, and only the extracted [`ParsedModel`] (never
/// the decoded bytes) crosses back to the async side (issue #291 follow-up).
/// The identity/`Other` branches below have the same worker-blocking parse
/// property but predate this fix — see the comment at their call site for why
/// they are deliberately left as-is.
pub(super) async fn resolve_model(
    headers: &HeaderMap,
    body: &Bytes,
    max_request_bytes: usize,
) -> Option<String> {
    match crate::compression::body_encoding(headers) {
        BodyEncoding::Zstd => {
            match crate::compression::decode_zstd_and_parse(
                body.clone(),
                max_request_bytes,
                |decoded| {
                    let decoded_bytes = decoded.len();
                    (parse_model(&decoded), decoded_bytes)
                },
            )
            .await
            {
                Ok(Some((parsed, decoded_bytes))) => {
                    model_from_parsed(parsed, decoded_bytes, body.len())
                }
                Ok(None) => {
                    tracing::warn!(
                        wire_bytes = body.len(),
                        limit = max_request_bytes,
                        "inbound codex body decodes past the request size limit or the \
                         compressed-to-decoded ratio bound; model label unavailable"
                    );
                    None
                }
                Err(error) => {
                    // `error` here is a libzstd-authored message (allocation/format
                    // failure), not client-controlled content — unlike the parse
                    // error handled in `model_from_parsed`, so logging it verbatim
                    // does not risk echoing the request body.
                    tracing::warn!(
                        wire_bytes = body.len(),
                        error = %error,
                        "failed to decode zstd inbound codex body; model label unavailable"
                    );
                    None
                }
            }
        }
        // A coding shunt does not decode (anything other than zstd/identity) is
        // not fatal to the label: fall through and attempt a best-effort plain
        // parse below, same as `Identity`. Returning `unknown` unconditionally
        // here would let a client suppress its own model label by sending a
        // bogus `content-encoding` header on an otherwise-plain body.
        BodyEncoding::Other => {
            tracing::warn!(
                content_encoding = ?headers.get(axum::http::header::CONTENT_ENCODING),
                "inbound codex body uses an unsupported content-encoding; \
                 attempting a best-effort plain-JSON parse for the model label"
            );
            // Pre-existing (predates issue #291's fix, which only fuses the new
            // zstd decode with its parse — see the doc comment above): this parse
            // still runs synchronously on the async executor. Left as-is
            // deliberately so that asymmetry with the zstd branch above is legible
            // rather than accidental.
            model_from_parsed(parse_model(body), body.len(), body.len())
        }
        BodyEncoding::Identity => {
            // Pre-existing (predates issue #291's fix, which only fuses the new
            // zstd decode with its parse — see the doc comment above): this parse
            // runs synchronously on the async executor rather than the blocking
            // pool. Left as-is deliberately, out of scope for the zstd-only fix.
            model_from_parsed(parse_model(body), body.len(), body.len())
        }
    }
}

/// The metrics/logging label for a request: [`resolve_model`]'s model, or
/// [`UNKNOWN_MODEL`] when it could not be read. Kept as its own function so the
/// label rule lives next to the parse it degrades from; production reads the
/// `Option` directly (an unreadable model must not match a route) and derives
/// the label from it.
#[cfg(test)]
pub(super) async fn model_label(
    headers: &HeaderMap,
    body: &Bytes,
    max_request_bytes: usize,
) -> String {
    resolve_model(headers, body, max_request_bytes)
        .await
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string())
}

/// Turn a [`ParsedModel`] into the model id, logging *why* it is unavailable
/// when it is. Shared by every [`resolve_model`] branch so the log shape is
/// identical regardless of which path produced the [`ParsedModel`].
///
/// The `Malformed` arm deliberately logs only the error's classification
/// (`line`/`column`/`classify()`), never `error.to_string()` /
/// `error = %error`: `serde_json::Error`'s `Display` embeds the offending
/// value it choked on (e.g. `invalid type: string "<entire body>", expected
/// struct ModelView`), so logging it verbatim would echo the client-controlled
/// request body — up to `MAX_REQUEST_BODY_BYTES` of it — into `warn!`, which
/// becomes a Sentry breadcrumb (`observability`) and is exported by the OTel
/// logs bridge (`telemetry`). Do not "helpfully" restore `error = %error` here.
fn model_from_parsed(
    parsed: ParsedModel,
    decoded_bytes: usize,
    wire_bytes: usize,
) -> Option<String> {
    match parsed {
        ParsedModel::Model(model) => Some(model),
        ParsedModel::Malformed(error) => {
            tracing::warn!(
                decoded_bytes,
                wire_bytes,
                error_line = error.line(),
                error_column = error.column(),
                error_kind = ?error.classify(),
                "inbound codex body is not valid JSON; labeling metrics and logs `unknown`"
            );
            None
        }
        ParsedModel::Missing => {
            tracing::warn!(
                decoded_bytes,
                wire_bytes,
                "inbound codex body has no `model` field; labeling metrics and logs `unknown`"
            );
            None
        }
        ParsedModel::NotAString(model_type) => {
            tracing::warn!(
                decoded_bytes,
                wire_bytes,
                model_type,
                "inbound codex body's `model` field is not a string; labeling metrics and logs `unknown`"
            );
            None
        }
    }
}

/// The distinguishable outcomes of reading `model` out of a decoded body, so
/// [`resolve_model`] can log *why* the model is unavailable instead of folding
/// malformed JSON, a missing field, and a wrong-typed field into one silent
/// `None` (as a bare `.ok().and_then(..)` chain over `Option<String>` would).
enum ParsedModel {
    Model(String),
    /// The body is not valid JSON at all.
    Malformed(serde_json::Error),
    /// Valid JSON with no `model` field (or an explicit `null`).
    Missing,
    /// Valid JSON with a `model` field that is not a string. Carries only the
    /// JSON type name, never the client-controlled value — see [`ModelField`].
    NotAString(&'static str),
}

fn parse_model(body: &[u8]) -> ParsedModel {
    match serde_json::from_slice::<ModelView>(body) {
        Ok(ModelView {
            model: Some(ModelField::Str(model)),
        }) => ParsedModel::Model(model),
        // `Option`'s deserializer maps an explicit `null` to `None` before
        // `ModelFieldVisitor` runs, so absent and `null` arrive here alike.
        Ok(ModelView { model: None }) => ParsedModel::Missing,
        Ok(ModelView {
            model: Some(ModelField::Other(model_type)),
        }) => ParsedModel::NotAString(model_type),
        Err(error) => ParsedModel::Malformed(error),
    }
}
