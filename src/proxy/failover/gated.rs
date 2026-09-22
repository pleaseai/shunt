//! The request side of buffer-and-replay: running a gated drive inside
//! `forward` and turning its decision into a response (ADR-0005 §4, issue
//! #596).
//!
//! Split out of `failover.rs` so the client path reads the same as before: a
//! gated entry's turn enters [`consult`] where an ungated one enters
//! `routing::driven::drive`, and leaves with one of three things. A
//! [`routing::driven::GatedDecision::Dispatch`] rewrites the chain (and, for a
//! REDO, the body) and the ordinary dispatch below `forward` carries on,
//! router headers and all. A replay or a failure is answered from here, by
//! [`finish`], *after* `forward` has recorded the router decision — the
//! counters describe what the turn was served as, and a replayed turn is a
//! served turn.
//!
//! Every response built here carries both router headers. On a replay they are
//! the only way a client can tell a retained turn from a live one
//! (`escalation_weak`, `advisor_approve`, …), and a `gated_error` names why the
//! turn it asked for never arrived.

use std::time::Instant;

use axum::http::{header::CONTENT_LENGTH, HeaderMap, StatusCode, Uri};
use serde_json::Value;

use super::{observe_response, stamp_router_headers, InboundContext, OwnedRouterStamp};
use crate::proxy::ForwardError;
use crate::request::RequestBody;
use crate::routing::{
    self,
    driven::{DrivenEntry, GatedDecision},
    outcome::RouterOutcome,
    serve::{
        gated::{GatedRequest, RetainedTurn},
        AdmittedContext,
    },
    Route,
};
use crate::server::AppState;

/// The admitted request a gated drive rides on, borrowed from `forward`.
pub(super) struct GatedTurn<'a> {
    pub(super) state: &'a AppState,
    pub(super) entry: &'a DrivenEntry,
    pub(super) uri: &'a Uri,
    /// The caller's original headers: the session metadata libsy keys on, and
    /// what [`AdmittedContext`] strips for a judge call.
    pub(super) headers: &'a HeaderMap,
    /// Post-admission headers and context: what the gated call dispatches on.
    pub(super) base_headers: &'a HeaderMap,
    pub(super) inbound: &'a InboundContext,
    pub(super) requested_model: &'a str,
}

/// A gated turn that is answered without a live dispatch.
pub(super) enum GatedExit {
    Replay {
        turn: RetainedTurn,
        stamp: OwnedRouterStamp,
    },
    Fail {
        response: axum::response::Response,
        message: String,
        relayed_status: bool,
        stamp: OwnedRouterStamp,
    },
}

/// Drive a gated entry and apply its decision to the request.
///
/// `outcome` is rewritten to the verdict so the router-decision counter and
/// both headers report it; `routes` and `body` are rewritten only for a live
/// dispatch. `None` means `forward` dispatches as usual.
pub(super) async fn consult(
    turn: GatedTurn<'_>,
    body: &mut RequestBody,
    routes: &mut Vec<Route>,
    outcome: &mut RouterOutcome,
) -> Option<GatedExit> {
    let router_id = outcome.model.clone();
    let admitted = AdmittedContext::mint(turn.inbound, turn.headers, &router_id);
    let request = GatedRequest {
        state: turn.state,
        uri: turn.uri,
        base_headers: turn.base_headers,
        inbound: turn.inbound,
        body: &*body,
        requested_model: turn.requested_model,
        router_id: &router_id,
    };
    let decision =
        routing::driven::drive_gated(turn.state, &admitted, turn.entry, &request, turn.headers)
            .await;
    let (target, source, judge_outcome) = match &decision {
        GatedDecision::Replay {
            target,
            source,
            judge_outcome,
            ..
        }
        | GatedDecision::Dispatch {
            target,
            source,
            judge_outcome,
            ..
        } => (target.clone(), *source, *judge_outcome),
        GatedDecision::Fail { target, source, .. } => (target.clone(), *source, None),
    };
    outcome.target.clone_from(&target);
    outcome.source = source;
    // Recorded only when a judge was actually asked (or an earlier verdict
    // held): a turn the algorithm served without a review is not a judge call,
    // and counting it would inflate the series `max_judge_calls` is tuned by.
    if let Some(judge_outcome) = judge_outcome {
        crate::metrics::record_judge_call(&router_id, turn.entry.algorithm_label(), judge_outcome);
    }
    // Labels only — never the verdict text or any message content.
    tracing::info!(
        router = %router_id,
        algorithm = turn.entry.algorithm_label(),
        outcome = judge_outcome.unwrap_or("none"),
        source = source.as_label(),
        "drove the router's gated turn"
    );
    let stamp = OwnedRouterStamp {
        routed_model: target,
        source: source.as_label(),
    };
    match decision {
        GatedDecision::Replay { turn, .. } => Some(GatedExit::Replay { turn, stamp }),
        GatedDecision::Fail {
            response,
            message,
            relayed_status,
            ..
        } => Some(GatedExit::Fail {
            response,
            message,
            relayed_status,
            stamp,
        }),
        GatedDecision::Dispatch { target, append, .. } => {
            *routes = routing::resolve_target_chain(&turn.state.config, &target, &router_id);
            if !append.is_empty() {
                // The REDO turn: the caller's own messages with the discarded
                // turn's echo and the advisor's plan appended. It is a live
                // dispatch whose headers are the first the client sees — the
                // discarded turn was never sent.
                body.mutate(|request| append_messages(request, append));
            }
            None
        }
    }
}

/// Answer a replayed or failed gated turn.
pub(super) async fn finish(
    exit: GatedExit,
    started_at: Instant,
    requested_safeguards: &[String],
    max_body_bytes: usize,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    match exit {
        GatedExit::Replay { turn, stamp } => {
            let mut response = axum::response::Response::new(axum::body::Body::from(turn.body));
            *response.status_mut() = turn.status;
            *response.headers_mut() = turn.headers;
            // The retained bytes are served whole, so the length is the
            // body's own; an upstream length describing a different encoding
            // of the same turn would contradict them.
            response.headers_mut().remove(CONTENT_LENGTH);
            stamp_router_headers(&mut response, &stamp);
            // The same observers a live turn passes through: safeguard
            // synthesis and the stream metrics see the replay exactly as they
            // would have seen the frames live.
            Ok(observe_response(
                turn.status,
                response,
                turn.provider,
                turn.model,
                started_at,
                requested_safeguards,
                max_body_bytes,
            )
            .await)
        }
        GatedExit::Fail {
            mut response,
            message,
            relayed_status,
            stamp,
        } => {
            stamp_router_headers(&mut response, &stamp);
            if relayed_status {
                Ok((response.status(), response))
            } else {
                Err(ForwardError::new(message, Box::new(response)))
            }
        }
    }
}

/// Append `messages` to the request's `messages` array. `false` — nothing
/// changed — when the request has no such array. The drive's decoder does not
/// require one (a body without `messages` decodes to an empty conversation),
/// but a REDO follows only an executor turn the upstream answered `2xx`, and a
/// Messages upstream refuses a body without `messages`, so the case is left to
/// that refusal rather than handled here.
fn append_messages(request: &mut Value, messages: Vec<Value>) -> bool {
    let Some(existing) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    existing.extend(messages);
    true
}
