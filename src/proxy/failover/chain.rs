//! The ordered failover attempt loop, shared by client turns and by the
//! internal calls a driven `[models.router]` makes (ADR-0005 §3, issue #594).
//!
//! Split out of `failover.rs` when the driven lane arrived, because a judge
//! call must take **exactly** this path and no parallel one: the same
//! `[[upstreams]]` ordering, the same advance/remember rules, the same
//! per-route header handling, the same account pools, and the same
//! `shunt.requests` / `shunt.latency` series. A second dispatcher for internal
//! calls would be a second set of those rules, and the failure mode of that
//! drift is a judge that keeps working after client traffic has failed over.
//!
//! What stays in `forward` rather than moving here is everything that is about
//! the *client's* response: the chain-stream branch (an internal call is never
//! streaming), safeguard synthesis, and the streaming metrics observer. Those
//! observe what the caller will see, and an internal call has no caller.
//! `caller` on [`ChainRequest`] is the one thing that tells the two apart in
//! the metrics, and it is the only difference.

use std::time::Instant;

use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;

use crate::{error::ShuntError, routing::Route, server::AppState};

use super::{
    dispatch, headers_for_route, is_passthrough_route, provider_origin, stamp_gateway_headers,
    InboundContext, RouterStamp,
};
use crate::proxy::ForwardError;
use crate::{
    adapters::{AdapterError, AdapterFailure},
    request::RequestBody,
};

/// Record an advance caused by a mapped upstream **error** response.
///
/// A named function rather than an inline `tracing::warn!` so the emission has
/// a call site a test can drive on one thread: the chain itself runs inside
/// spawned tasks, where a `with_default` subscriber never reaches.
///
/// `upstream_message` is deliberately not called `message`. `message` is the
/// field `tracing` records the event's own literal under, so passing the
/// upstream's text as `message` does not add a field — it emits the upstream
/// string bare and unlabelled next to the gateway's own wording, where neither
/// an operator reading the line nor a parser keying on `message` can tell the
/// two apart. The string is upstream-controlled, which is exactly the input
/// that should never land in the slot naming what happened.
///
/// Recorded with `?` rather than `%` for the same reason: `Debug` quotes the
/// value and escapes newlines, so an upstream cannot embed a line break and
/// have the remainder of its text read as a separate log record.
fn log_error_advance(provider: &str, model: &str, status: StatusCode, upstream_message: &str) {
    tracing::warn!(
        provider = %provider,
        model = %model,
        status = status.as_u16(),
        upstream_message = ?upstream_message,
        "upstream error triggered failover advance"
    );
}

/// Record an advance caused by a failure that arrived before response headers.
///
/// Same `upstream_message` naming rule as [`log_error_advance`].
fn log_before_headers_advance(provider: &str, model: &str, upstream_message: &str) {
    tracing::warn!(
        provider = %provider,
        model = %model,
        upstream_message = ?upstream_message,
        "upstream failed before response headers; advancing failover"
    );
}

/// One dispatch through an ordered failover chain.
pub(crate) struct ChainRequest<'a> {
    pub state: AppState,
    pub routes: Vec<Route>,
    pub uri: &'a Uri,
    /// Headers with the inbound credential slots already handled — the
    /// caller's, post-`check_inbound_auth`, or a judge call's stripped set.
    pub base_headers: &'a HeaderMap,
    pub inbound: &'a InboundContext,
    pub body: RequestBody,
    /// The advertised id, for the response stamp and the metric label.
    pub requested_model: &'a str,
    pub router_stamp: Option<RouterStamp<'a>>,
    /// `"client"` or `"router"` — the attribute that separates a caller's turn
    /// from an internal judge call in `shunt.requests` and `shunt.latency`.
    pub caller: &'static str,
    /// Bound on a whole-body read of the upstream reply, for the internal calls
    /// that have one. `None` on every client path — see
    /// [`crate::adapters::Adapter::forward`].
    pub response_byte_cap: Option<usize>,
}

/// A chain attempt that answered with a status the chain does not advance on.
pub(crate) struct ChainSuccess {
    pub status: StatusCode,
    pub response: axum::response::Response,
    /// The upstream that answered, for the observers `forward` applies.
    pub provider: String,
    pub model: String,
}

/// Attempt each route in order until one answers with a status the chain does
/// not advance on.
///
/// Behaviour is exactly what `forward` did inline before the split, including
/// recording the request's ultimate outcome once — at whichever terminal point
/// is taken — rather than once per attempt (#281).
pub(crate) async fn run_chain(request: ChainRequest<'_>) -> Result<ChainSuccess, ForwardError> {
    let ChainRequest {
        state,
        routes,
        uri,
        base_headers,
        inbound,
        body,
        requested_model,
        router_stamp,
        caller,
        response_byte_cap,
    } = request;
    // Records the request's final outcome exactly once, at whichever terminal
    // return point below is taken — the intermediate per-attempt failover
    // advances already have their own `tracing::warn!` + metrics and are not
    // re-recorded here (#281 asks only for the request's ultimate outcome, to
    // keep the span/event signal one-per-request rather than one-per-attempt).
    let finish = |provider: &str, status: StatusCode| {
        crate::observability::record_span_outcome(provider, status);
        crate::observability::capture_upstream_outcome(provider, requested_model, status);
    };
    let attempted_total = routes.len();
    let last_route = routes
        .last()
        .expect("route chains are non-empty after resolution")
        .clone();
    let primary_origin = primary_origin(&state, &routes);
    let mut body = Some(body);
    let mut remembered: Option<RememberedFailure> = None;
    for (index, route) in routes.into_iter().enumerate() {
        crate::metrics::record_failover(&route.provider, "attempted");
        let attempt_headers = headers_for_route(
            &state,
            &route,
            base_headers,
            inbound,
            index == 0,
            primary_origin.as_deref(),
        );
        let provider = route.provider.clone();
        let model = route.model.clone();
        let upstream_model = route.upstream_model.clone();
        let attempt_started_at = Instant::now();
        // Move the buffered body into the final attempt instead of cloning it. Within
        // this failover loop, the common single-upstream chain transfers the body
        // without copying its (up to 64 MB) raw buffer, and a multi-upstream chain
        // only clones that buffer for attempts preceding the last. Those clones
        // still share the parsed tree; an adapter may clone the body again downstream.
        // The `Option` permits the final move while keeping the body available earlier.
        let attempt_body = if index + 1 < attempted_total {
            body.as_ref().expect("request body is present").clone()
        } else {
            body.take()
                .expect("request body is present for final attempt")
        };
        let result = dispatch(
            state.clone(),
            route,
            uri,
            &attempt_headers,
            attempt_body,
            response_byte_cap,
        )
        .await;

        if !super::is_count_tokens(uri)
            && !result.as_ref().is_ok_and(|(_, response)| {
                // The early-commit streaming responses sample their metrics
                // in-stream at classification: the dispatch-time return
                // precedes the upstream send, so recording here would count a
                // fake 200 with a near-zero latency.
                response
                    .extensions()
                    .get::<crate::adapters::responses::InStreamMetrics>()
                    .is_some()
            })
        {
            let status = match &result {
                Ok((status, _)) => status.as_u16(),
                Err(error) => error.response.status().as_u16(),
            };
            crate::metrics::record_proxied_request_as(
                caller,
                &provider,
                &model,
                status,
                attempt_started_at.elapsed().as_secs_f64() * 1000.0,
            );
        }

        match result {
            Ok((status, mut response)) => {
                stamp_gateway_headers(
                    &mut response,
                    &provider,
                    requested_model,
                    &upstream_model,
                    router_stamp,
                );
                if !is_advance_status(status) {
                    finish(&provider, status);
                    return Ok(ChainSuccess {
                        status,
                        response,
                        provider,
                        model,
                    });
                }
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    status = status.as_u16(),
                    "upstream response triggered failover advance"
                );
                remember_failure(
                    &mut remembered,
                    status,
                    FinalResponse::Relayed(response),
                    provider.clone(),
                    model,
                );
            }
            Err(error) => {
                let AdapterError {
                    message,
                    mut response,
                    failure,
                } = error;
                stamp_gateway_headers(
                    &mut response,
                    &provider,
                    requested_model,
                    &upstream_model,
                    router_stamp,
                );
                match failure {
                    Some(AdapterFailure::UpstreamStatus(raw_status))
                        if is_advance_status(raw_status) =>
                    {
                        log_error_advance(&provider, &model, raw_status, &message);
                        remember_failure(
                            &mut remembered,
                            raw_status,
                            FinalResponse::MappedError { message, response },
                            provider.clone(),
                            model,
                        );
                    }
                    Some(AdapterFailure::BeforeHeaders) => {
                        log_before_headers_advance(&provider, &model, &message);
                    }
                    _ => {
                        finish(&provider, response.status());
                        return Err(ForwardError::new(message, response));
                    }
                }
            }
        }

        if index + 1 < attempted_total {
            crate::metrics::record_failover(&provider, "advanced");
        }
    }

    crate::metrics::record_failover(&last_route.provider, "exhausted");
    if let Some(failure) = remembered {
        return match failure.response {
            FinalResponse::Relayed(response) => {
                let status = response.status();
                finish(&failure.provider, status);
                Ok(ChainSuccess {
                    status,
                    response,
                    provider: failure.provider,
                    model: failure.model,
                })
            }
            FinalResponse::MappedError { message, response } => {
                finish(&failure.provider, response.status());
                Err(ForwardError::new(message, response))
            }
        };
    }

    let message = format!("all upstreams failed ({attempted_total} attempted)");
    let mut response =
        ShuntError::new(StatusCode::BAD_GATEWAY, "api_error", message.clone()).into_response();
    stamp_gateway_headers(
        &mut response,
        &last_route.provider,
        requested_model,
        &last_route.upstream_model,
        router_stamp,
    );
    finish(&last_route.provider, StatusCode::BAD_GATEWAY);
    Err(ForwardError::new(message, Box::new(response)))
}

/// The origin a passthrough failover attempt is allowed to reuse the caller's
/// credential for.
///
/// The caller's credential is retained across a passthrough failover only
/// when the *primary* route is itself passthrough: then the credential is the
/// caller's own upstream credential, presented for the primary's origin, so a
/// same-origin passthrough fallback may reuse it. When the primary instead
/// injects its own credential, the caller credential is a gateway/client
/// secret that must never be replayed upstream — no origin is retained and
/// every passthrough fallback strips it. Only failover attempts consult this,
/// so a single-upstream chain parses no URL here at all.
///
/// Shared with `forward`'s chain-stream branch, which takes the same decision
/// before this loop is reached.
pub(super) fn primary_origin(state: &AppState, routes: &[Route]) -> Option<String> {
    let first = routes.first()?;
    (routes.len() > 1 && is_passthrough_route(state, first))
        .then(|| provider_origin(state, &first.provider))
        .flatten()
}

pub(crate) fn is_advance_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
    ) || status.is_server_error()
}

pub(crate) fn failure_priority(status: StatusCode) -> u8 {
    match status {
        StatusCode::TOO_MANY_REQUESTS => 4,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => 3,
        StatusCode::NOT_FOUND => 2,
        _ if status.is_server_error() => 1,
        _ => 0,
    }
}

enum FinalResponse {
    Relayed(axum::response::Response),
    MappedError {
        message: String,
        response: Box<axum::response::Response>,
    },
}

struct RememberedFailure {
    raw_status: StatusCode,
    response: FinalResponse,
    provider: String,
    model: String,
}

fn remember_failure(
    remembered: &mut Option<RememberedFailure>,
    raw_status: StatusCode,
    response: FinalResponse,
    provider: String,
    model: String,
) {
    if remembered
        .as_ref()
        .is_some_and(|current| failure_priority(current.raw_status) >= failure_priority(raw_status))
    {
        return;
    }
    *remembered = Some(RememberedFailure {
        raw_status,
        response,
        provider,
        model,
    });
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    use axum::http::StatusCode;

    use super::{log_before_headers_advance, log_error_advance};

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture(operation: impl FnOnce()) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter(Arc::clone(&writer_output)))
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, operation);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        logs
    }

    /// The upstream's text must travel under its own key.
    ///
    /// Naming the field `message` does not add a field: `tracing` already
    /// records the event's literal under `message`, so the upstream string is
    /// emitted bare — no `key=` in front of it — immediately after the
    /// gateway's own wording. That is the defect this asserts against, and it
    /// is why the assertion is on the labelled form rather than on the text
    /// merely appearing somewhere in the line.
    #[test]
    fn an_error_advance_labels_the_upstream_text_instead_of_shadowing_the_event_message() {
        let logs = capture(|| {
            log_error_advance(
                "anthropic",
                "claude-sonnet-4-6",
                StatusCode::INTERNAL_SERVER_ERROR,
                "upstream said boom",
            );
        });

        assert!(
            logs.contains("upstream_message=\"upstream said boom\""),
            "upstream text must be a labelled field; got: {logs}"
        );
        assert!(
            logs.contains("upstream error triggered failover advance"),
            "the event keeps its own wording; got: {logs}"
        );
        assert!(
            logs.contains("status=500"),
            "the advance status is recorded; got: {logs}"
        );
    }

    /// Same rule on the before-headers arm.
    ///
    /// A second test rather than a loop because the two call sites take
    /// different arguments, and a shared helper that papered over that is how
    /// one of them would quietly stop being covered.
    #[test]
    fn a_before_headers_advance_labels_the_upstream_text() {
        let logs = capture(|| {
            log_before_headers_advance("anthropic", "claude-sonnet-4-6", "connection reset");
        });

        assert!(
            logs.contains("upstream_message=\"connection reset\""),
            "upstream text must be a labelled field; got: {logs}"
        );
        assert!(
            logs.contains("upstream failed before response headers"),
            "the event keeps its own wording; got: {logs}"
        );
    }

    /// Guard the assertion above against passing for the wrong reason.
    ///
    /// `upstream_message="..."` could in principle appear because the text was
    /// quoted inside the event literal. Emitting a value that is distinctive
    /// and *not* present in either literal keeps the check tied to the field.
    #[test]
    fn the_labelled_field_carries_the_value_it_was_given() {
        let logs = capture(|| {
            log_error_advance("p", "m", StatusCode::BAD_GATEWAY, "zzdistinctzz");
        });

        assert!(
            logs.contains("upstream_message=\"zzdistinctzz\""),
            "got: {logs}"
        );
    }
}
