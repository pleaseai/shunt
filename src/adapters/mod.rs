use std::{future::Future, pin::Pin};

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};

use crate::{request::RequestBody, routing::Route, server::AppState};

pub mod anthropic;
pub mod antigravity;
pub mod cursor;
pub mod gemini;
pub mod noop;
pub mod responses;

/// Tie a storm-control [`AdmissionGuard`](crate::accounts::AdmissionGuard) to a
/// relayed response (issue #195). The response body is lazy — for a streaming
/// turn the adapter function returns long before axum drives the SSE bytes to
/// the client — so dropping the guard at return would free the admission slot
/// at roughly time-to-first-byte instead of for the turn's real duration. Moving
/// the guard into the body stream makes it drop when the stream is exhausted or
/// the client disconnects, so `in_flight` counts genuinely concurrent turns.
pub(crate) fn with_admission(
    response: Response,
    admission: Option<crate::accounts::AdmissionGuard>,
) -> Response {
    use futures_util::StreamExt;
    let Some(guard) = admission else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        // The `move` closure owns the guard; this reference only forces the
        // capture (a variable the body never touches is not captured at all).
        // The owned guard then drops with the closure when the stream does.
        let _held = &guard;
        chunk
    });
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

pub type AdapterResult = Result<(StatusCode, Response), AdapterError>;
pub type AdapterFuture<'a> = Pin<Box<dyn Future<Output = AdapterResult> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterFailure {
    /// The upstream returned this status before the adapter mapped it for the client.
    UpstreamStatus(StatusCode),
    /// The attempt failed before any upstream response headers were received.
    BeforeHeaders,
}

#[derive(Debug)]
pub struct AdapterError {
    pub message: String,
    pub response: Box<Response>,
    /// Additive failover metadata. `None` marks a local adapter/auth/validation
    /// error that must be returned immediately rather than retried elsewhere.
    pub failure: Option<AdapterFailure>,
}

/// The byte cap a bounded call's upstream reply crossed.
///
/// Carries no partial body — the point of the cap is that the bytes past it are
/// never held — and travels as an extension on the refusal's response, which is
/// how `routing::serve` tells an oversized reply apart from an upstream that
/// merely failed. A `'static` marker rather than a status code because the
/// status a client-facing refusal renders is `502` like several others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpstreamBodyTooLarge {
    pub(crate) max_bytes: usize,
}

impl std::fmt::Display for UpstreamBodyTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "upstream response body exceeded {} bytes",
            self.max_bytes
        )
    }
}

impl std::error::Error for UpstreamBodyTooLarge {}

/// How a bounded whole-body read ended.
pub(crate) enum UpstreamBodyError {
    /// The transport failed after the response headers were committed.
    Transport(reqwest::Error),
    /// The body passed the cap and was abandoned unread.
    TooLarge(UpstreamBodyTooLarge),
}

/// Read a whole upstream body, refusing it the moment it passes `cap`.
///
/// `None` is the client path and is `reqwest`'s own `bytes()`, byte for byte:
/// a client turn's reply is bounded by the client's own request, and adding a
/// cap there would be a new way for ordinary traffic to fail.
///
/// `Some` is an internal call, where the bound is the point. It stops *reading*
/// on the crossing rather than buffering and checking afterwards — checking
/// afterwards is checking after the memory has already been spent, which is
/// exactly what a bound on a judge's reply exists to prevent.
pub(crate) async fn collect_upstream_body(
    upstream: reqwest::Response,
    cap: Option<usize>,
) -> Result<bytes::Bytes, UpstreamBodyError> {
    let Some(max_bytes) = cap else {
        return upstream.bytes().await.map_err(UpstreamBodyError::Transport);
    };
    // An upstream that *declares* more than the cap is refused before a byte is
    // read. The running total below is still the enforcement point — this only
    // decides how early the same refusal happens — but it turns two cases from
    // slow into immediate: a body that would be drained to the cap and
    // discarded, and one whose sender declares a large length and then stalls,
    // which would otherwise sit until `judge_timeout_ms` and be reported as a
    // timeout rather than as the oversized reply it announced itself to be.
    //
    // Only the declaration is trusted downward, never upward: a length under
    // the cap proves nothing and the loop still counts every byte. A chunked
    // reply has no `content-length` at all, so this is a fast path rather than
    // a gate. Decoding only ever grows a body, so a declared length above the
    // cap cannot decode to something below it.
    //
    // Deliberately no `Vec::with_capacity(content_length)`: the length is the
    // sender's claim, so pre-allocating on it would let a peer that declares a
    // large body and sends nothing take that allocation for free. The vector
    // grows against bytes that actually arrived.
    if upstream
        .content_length()
        .is_some_and(|declared| declared > max_bytes as u64)
    {
        return Err(UpstreamBodyError::TooLarge(UpstreamBodyTooLarge {
            max_bytes,
        }));
    }
    use futures_util::StreamExt;
    let mut stream = upstream.bytes_stream();
    let mut collected: Vec<u8> = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(UpstreamBodyError::Transport)?;
        total = total.saturating_add(chunk.len());
        if total > max_bytes {
            return Err(UpstreamBodyError::TooLarge(UpstreamBodyTooLarge {
                max_bytes,
            }));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(collected))
}

/// Render [`UpstreamBodyError::TooLarge`] as an adapter failure that carries the
/// marker `routing::serve` reads back.
///
/// `failure: None` deliberately: an upstream that answered correctly and merely
/// answered *too much* is not a reason to advance the failover chain onto
/// another provider, and retrying it would spend the cap again.
pub(crate) fn too_large_error(too_large: UpstreamBodyTooLarge) -> AdapterError {
    let mut response =
        crate::error::ShuntError::new(StatusCode::BAD_GATEWAY, "api_error", too_large.to_string())
            .into_response();
    response.extensions_mut().insert(too_large);
    AdapterError {
        message: too_large.to_string(),
        response: Box::new(response),
        failure: None,
    }
}

/// Whether `accumulated` bytes have passed `cap`, as the marker
/// `routing::serve` reads back to resolve a judge as `oversized`.
///
/// For the adapters that build a reply up piece by piece rather than reading
/// one upstream body: [`collect_upstream_body`] cannot bound a reply that
/// arrives as a translated event stream, but the accumulation still has to be
/// bounded, and on the same marker so the two report identically. `None` is
/// the client path and never refuses.
pub(crate) fn over_cap(accumulated: usize, cap: Option<usize>) -> Option<UpstreamBodyTooLarge> {
    let max_bytes = cap?;
    (accumulated > max_bytes).then_some(UpstreamBodyTooLarge { max_bytes })
}

pub(crate) trait Adapter {
    /// Dispatch one request upstream.
    ///
    /// `response_byte_cap` bounds a **whole-body read** the adapter performs on
    /// the upstream reply. It is `None` for every client turn — client
    /// behaviour is byte-for-byte what it was — and `Some` only for the
    /// internal calls `routing::serve` makes, where `judge_max_response_bytes`
    /// has to bite before the body is materialised rather than after.
    ///
    /// An adapter materialises a whole reply on two shapes, and both are
    /// bounded here: a single whole-body read (see [`collect_upstream_body`])
    /// and an accumulation built up from a translated event stream (see
    /// [`over_cap`]). Only an adapter that does neither — one that relays every
    /// byte onward without holding it — has nothing to cap and ignores it; the
    /// bound then falls to `routing::serve`'s own collector, which reads the
    /// relayed stream under the same cap.
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
        response_byte_cap: Option<usize>,
    ) -> AdapterFuture<'a>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;

    use super::with_admission;
    use crate::{accounts::AccountPool, config::AccountConfig};

    fn account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn with_admission_holds_slot_until_body_is_consumed() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool
            .clone()
            .try_admit("codex", &acc, 1, false)
            .expect("first admission");

        let response = with_admission(
            axum::response::Response::new(Body::from("data: chunk\n\n")),
            Some(guard),
        );

        // The slot stays occupied while the wrapped body is still pending —
        // the guard must not drop at `with_admission`'s return.
        assert!(
            pool.clone().try_admit("codex", &acc, 1, false).is_none(),
            "slot should be held while the response body is unread"
        );

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body drives to completion");
        assert_eq!(bytes.as_ref(), b"data: chunk\n\n");

        let reguard = pool.clone().try_admit("codex", &acc, 1, false);
        assert!(
            reguard.is_some(),
            "slot should free once the body stream is exhausted"
        );
    }

    #[tokio::test]
    async fn with_admission_frees_slot_when_body_is_dropped_unread() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool
            .clone()
            .try_admit("codex", &acc, 1, false)
            .expect("first admission");

        let response = with_admission(
            axum::response::Response::new(Body::from("data: chunk\n\n")),
            Some(guard),
        );
        drop(response); // client disconnect before reading the stream

        assert!(
            pool.try_admit("codex", &acc, 1, false).is_some(),
            "slot should free when the response is dropped unread"
        );
    }
}
