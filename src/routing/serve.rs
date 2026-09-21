//! `serve`: how shunt performs one model call libsy asks for while routing
//! (ADR-0005 §3, issue #594).
//!
//! What this module guards is that an internal call is *shunt's* call and not
//! the caller's. Three things follow from that, and each is enforced here
//! rather than left to the caller:
//!
//! * It is made only behind an [`AdmittedContext`], which only the admission
//!   gate can mint. A judge call without one is a bug, not a request.
//! * It carries **no inbound credential**. Every reserved slot is stripped and
//!   both `SHARED_SLOTS` names are removed unconditionally — not by value, the
//!   way [`ShuntCredentials::strip_consumed_slots`] does for a passthrough
//!   relay, because that one deliberately keeps a caller's genuine upstream
//!   credential and would keep it here too. The call then runs on whatever
//!   credential its own target's route injects, which config validation
//!   guarantees exists.
//! * It is bounded end to end. The deadline wraps the whole
//!   dispatch-and-collect future rather than the transport's `.send()`: a
//!   `.send()` timeout stops at headers, so a `200` that then stalls forever
//!   would never fail open. Dropping that future is also what cancels the
//!   upstream request, which is why it is awaited in place and never spawned.
//!
//! Everything else is deliberately the same as client traffic: the same
//! `[[upstreams]]` failover chain, the same adapters, the same account pools,
//! and the same `shunt.requests` / `shunt.latency` series — separated only by
//! `caller = "router"`.

pub(crate) mod bounds;

use std::sync::Mutex;

use axum::http::{HeaderMap, Uri};
use switchyard_libsy::{CallModel, LibsyError};
use switchyard_protocol::{LlmClientError, LlmResponse, ModelId, Response};
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use crate::auth::slots::{ShuntCredentials, SHARED_SLOTS};
use crate::config::CallBounds;
use crate::proxy::failover::{
    chain::{run_chain, ChainRequest},
    InboundContext,
};
use crate::routing;
use crate::server::AppState;

/// Why a judge call did not produce a verdict, as the closed label set
/// `shunt.router.judge_calls{outcome}` reports (ADR-0005 §7).
///
/// Recorded by the call that failed and read by the drive that wraps it:
/// libsy's `RoutingOutcome` says only that no verdict was available, so the
/// *reason* has to be carried out of band or the metric would have one
/// `fail_open` bucket for five different operational faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JudgeFailure {
    /// `judge_timeout_ms` elapsed, headers or body.
    Timeout,
    /// The reply passed `judge_max_response_bytes`.
    Oversized,
    /// The chain answered, or failed, with a non-success status — or its body
    /// stream broke part-way through a reply it had already begun, which is a
    /// failed upstream rather than a malformed answer.
    UpstreamStatus,
    /// The reply was not JSON, or was not a Messages response.
    InvalidReply,
    /// The neutral request could not be encoded, or the reply decoded.
    Translation,
}

impl JudgeFailure {
    /// The metric label. A closed set, documented on
    /// [`crate::routing::judge::JudgeOutcome`] beside the one label that is not
    /// a failure of the call itself.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Oversized => "oversized",
            Self::UpstreamStatus => "upstream_error",
            Self::InvalidReply => "invalid_reply",
            Self::Translation => "translation_failed",
        }
    }
}

/// Proof that the request a judge call rides on has cleared inbound auth and
/// the managed-model policy (ADR-0005 §3).
///
/// Unforgeable by construction: the only constructor takes an
/// [`InboundContext`], and the only place one exists is the success path of
/// `check_inbound_auth`. That is the whole point of the type — a judge call
/// spends a gateway-held credential and the judge target's pool quota, so it
/// must be impossible to reach one from a request that was about to be
/// refused. A judge call without an `AdmittedContext` is a bug, not a request.
pub(crate) struct AdmittedContext<'a> {
    /// The advertised id the client asked for. Stamped onto the judge call's
    /// routes as `Route.model` so the adapter renders the id the caller knows.
    router_id: &'a str,
    /// The caller's headers, stripped of every credential slot before they are
    /// sent (see [`judge_headers`]).
    headers: &'a HeaderMap,
    /// Kept so the borrow cannot outlive the admission that produced it.
    _inbound: &'a InboundContext,
}

impl<'a> AdmittedContext<'a> {
    /// Minted only by the admission gate: `inbound` exists only on
    /// `check_inbound_auth`'s success path.
    pub(crate) fn mint(
        inbound: &'a InboundContext,
        headers: &'a HeaderMap,
        router_id: &'a str,
    ) -> Self {
        Self {
            router_id,
            headers,
            _inbound: inbound,
        }
    }

    /// The advertised id, for the routes this call is dispatched on.
    pub(crate) fn router_id(&self) -> &str {
        self.router_id
    }
}

/// Serve one `Step::CallModel` from libsy's driven lane.
///
/// Always fulfils the promise — with the verdict on success and with the typed
/// failure otherwise — because an unfulfilled promise leaves the algorithm
/// blocked until the whole run is dropped. Returns `Err` only when `respond`
/// itself fails, which is `drive`'s documented signal for an infrastructure
/// fault rather than a failed model call.
pub(crate) async fn judge_call(
    state: AppState,
    admitted: &AdmittedContext<'_>,
    call: CallModel,
    bounds: CallBounds,
    failure: &Mutex<Option<JudgeFailure>>,
) -> switchyard_libsy::Result<()> {
    let Some(target) = call.models.first().cloned() else {
        return call.respond(Err(LibsyError::NoTargets));
    };
    let result = dispatch(&state, admitted, &call, &target, bounds, failure).await;
    call.respond(result)
}

/// Everything between the promise and its answer, so `judge_call` above is
/// just "fulfil whatever this returns".
async fn dispatch(
    state: &AppState,
    admitted: &AdmittedContext<'_>,
    call: &CallModel,
    target: &ModelId,
    bounds: CallBounds,
    failure: &Mutex<Option<JudgeFailure>>,
) -> switchyard_libsy::Result<Response> {
    let engine = TranslationEngine::default();
    let policy = TranslationPolicy::default();
    let body = match judge_body(&engine, &policy, call) {
        Ok(body) => body,
        Err(message) => {
            return Err(client_call(
                target,
                failure,
                JudgeFailure::Translation,
                LlmClientError::RequestTranslation(message),
            ))
        }
    };
    let routes = routing::resolve_target_chain(&state.config, target.as_str(), admitted.router_id);
    let headers = judge_headers(state, admitted.headers);
    // `/v1/messages` because the judge request *is* a Messages request: the
    // codec emits that shape and the adapters route on this path exactly as
    // they do for a client turn.
    let uri = Uri::from_static("/v1/messages");
    let inbound = InboundContext::internal();
    // The deadline wraps dispatch *and* collection. Awaited in place, never
    // spawned: dropping this future on elapse is what cancels the upstream
    // request, and a spawned task would keep running with nothing to answer.
    let collected = tokio::time::timeout(bounds.judge_timeout, async {
        let outcome = run_chain(ChainRequest {
            state: state.clone(),
            routes,
            uri: &uri,
            base_headers: &headers,
            inbound: &inbound,
            body,
            requested_model: admitted.router_id,
            router_stamp: None,
            caller: "router",
            // The cap, at the point the adapter would otherwise buffer the
            // whole reply to rewrite its `model`. `collect_bounded` below
            // stays as defence in depth: it bounds the relayed stream, which
            // is the path an adapter that buffers nothing takes.
            response_byte_cap: Some(bounds.judge_max_response_bytes),
        })
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            // An upstream body the adapter refused *before* buffering it is
            // the byte cap biting, not a failed upstream: it has to report the
            // bound the operator wrote, exactly as the collector below does for
            // a reply that reached this far.
            Err(error) => {
                return Err(match error.body_too_large() {
                    Some(too_large) => (
                        JudgeFailure::Oversized,
                        LlmClientError::InvalidResponse {
                            source: Box::new(too_large),
                        },
                    ),
                    None => (
                        JudgeFailure::UpstreamStatus,
                        LlmClientError::UpstreamHttp {
                            status: error.status(),
                            body: error.message().to_string(),
                        },
                    ),
                })
            }
        };
        let status = outcome.status;
        let bytes = bounds::collect_bounded(
            outcome.response.into_body(),
            bounds.judge_max_response_bytes,
        )
        .await
        .map_err(|error| match error {
            bounds::CollectError::Oversized(oversized) => (
                JudgeFailure::Oversized,
                LlmClientError::InvalidResponse {
                    source: Box::new(oversized),
                },
            ),
            // The connection broke part-way through the reply. Recording it
            // as `InvalidReply` — which is what the truncated bytes would
            // have produced once their JSON parse failed — would name the
            // judge's answer as malformed when the transport is what failed,
            // and leave an operator reading `invalid_reply` in the metric for
            // a network fault.
            bounds::CollectError::Transport(source) => (
                JudgeFailure::UpstreamStatus,
                LlmClientError::Transport {
                    source: Box::new(source),
                },
            ),
        })?;
        if !status.is_success() {
            // Collected first, and under the same cap: the error body is what
            // makes an upstream refusal diagnosable, and it is no more trusted
            // than a success body.
            return Err((
                JudgeFailure::UpstreamStatus,
                LlmClientError::UpstreamHttp {
                    status,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                },
            ));
        }
        Ok(bytes)
    })
    .await;
    let bytes = match collected {
        Ok(Ok(bytes)) => bytes,
        Ok(Err((kind, error))) => return Err(client_call(target, failure, kind, error)),
        Err(elapsed) => {
            return Err(client_call(
                target,
                failure,
                JudgeFailure::Timeout,
                LlmClientError::Timeout {
                    source: Box::new(elapsed),
                },
            ))
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return Err(client_call(
                target,
                failure,
                JudgeFailure::InvalidReply,
                LlmClientError::InvalidResponse {
                    source: Box::new(error),
                },
            ))
        }
    };
    let decoded = engine.decode_response(WireFormat::AnthropicMessages, &value, &policy);
    let aggregate = match decoded {
        Ok(decoded) => decoded.response,
        Err(error) => {
            return Err(client_call(
                target,
                failure,
                JudgeFailure::Translation,
                LlmClientError::ResponseTranslation(error.to_string()),
            ))
        }
    };
    Ok(Response {
        llm_response: LlmResponse::Agg(aggregate),
        metadata: None,
        // Deliberately empty: the judge's upstream headers are shunt's own
        // exchange with the judge provider and belong to no client response.
        upstream_headers: HeaderMap::new(),
    })
}

/// Encode the neutral request libsy handed over as an Anthropic Messages body.
///
/// Forces `stream` off. The codec does not set it, but the field is the one
/// difference between a reply this path can parse and a stream it cannot: a
/// judge answer is collected whole, and ADR-0005 §3 says judge calls are
/// non-streaming.
fn judge_body(
    engine: &TranslationEngine,
    policy: &TranslationPolicy,
    call: &CallModel,
) -> Result<crate::request::RequestBody, String> {
    let encoded = engine
        .encode_request(
            WireFormat::AnthropicMessages,
            &call.request.llm_request,
            policy,
        )
        .map_err(|error| error.to_string())?;
    let mut value = encoded.body;
    if let Some(object) = value.as_object_mut() {
        object.remove("stream");
    }
    let raw = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let mut body = crate::request::RequestBody::parse(raw).map_err(|error| error.to_string())?;
    // Normalized exactly as a client body is, so the judge request cannot be
    // rejected by a rule client traffic is already held to.
    crate::proxy::normalize_request_body(&mut body);
    Ok(body)
}

/// The caller's headers with every inbound credential slot removed.
///
/// Three removals, and the middle one is the one that is easy to get wrong:
///
/// * [`ShuntCredentials::strip_reserved_slots`] — the fixed `x-shunt-*` names,
///   `cookie`, and the configured `[server.auth]`/`[server.admin]` headers.
/// * Both [`SHARED_SLOTS`] names (`authorization`, `x-api-key`),
///   **unconditionally**. Not `strip_consumed_slots`: that one judges by value
///   so a passthrough relay keeps the caller's genuine upstream credential —
///   exactly the credential that must not travel here, because it was
///   presented for the answer provider's origin and this call goes to the
///   judge's.
/// * `anthropic-beta` and `content-length`. The beta set is the caller's
///   negotiated feature list for their own turn and has nothing to do with the
///   judge's packaged request; the length describes a body that no longer
///   exists.
pub(crate) fn judge_headers(state: &AppState, caller: &HeaderMap) -> HeaderMap {
    let mut headers = caller.clone();
    ShuntCredentials::from_state(state).strip_reserved_slots(&mut headers);
    for slot in SHARED_SLOTS {
        headers.remove(slot);
    }
    headers.remove("anthropic-beta");
    headers.remove("content-length");
    headers
}

/// Record the failure kind and wrap the client error as libsy's own.
///
/// One place, so a new error path cannot reach `respond` without also landing
/// in the metric — the label set is closed precisely because every producer
/// goes through here.
fn client_call(
    target: &ModelId,
    failure: &Mutex<Option<JudgeFailure>>,
    kind: JudgeFailure,
    source: LlmClientError,
) -> LibsyError {
    *failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kind);
    LibsyError::ClientCall {
        target: target.clone(),
        source,
    }
}

#[cfg(test)]
mod tests;
