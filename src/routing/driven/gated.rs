//! One drive of a gated entry — `mode = "escalation"` or `type = "advisor"` —
//! for one admitted request (ADR-0005 §4, §8 PR 6, issue #596).
//!
//! The shape is [`super::drive`]'s: decode the caller's body to the neutral IR,
//! hand it to libsy, serve its calls, bound the whole run. What differs is the
//! first call. In both algorithms the first `CallModel` a drive makes (when it
//! makes one at all) is the turn the caller will be served, not a judge, so it
//! is served by [`gated_call`] — the caller's own body, credential, and mode,
//! retained whole — and every later call is a judge or review call served by
//! [`judge_call`] exactly as on an ungated entry. The ordinal is what tells
//! them apart, never the target id: a weak target may also be the judge.
//!
//! The answer is therefore not only *where* the turn goes but *whether a turn
//! already exists*: [`GatedDecision::Replay`] serves the retained bytes,
//! [`GatedDecision::Dispatch`] makes a live call (a latched session, a REDO),
//! and [`GatedDecision::Fail`] answers with an error before any header has
//! been committed.
//!
//! # Evidence → [`RouteSource`]
//!
//! Read together with whether libsy handed a response back, which is the
//! "serve the retained turn" signal (see [`read_evidence`]):
//!
//! | algorithm | response | evidence | [`RouteSource`] | judge outcome |
//! |---|---|---|---|---|
//! | escalation | served | `fail_open` | [`RouteSource::DrivenFailOpen`] | the recorded failure |
//! | escalation | served | anything else (`continue`, `pending`) | [`RouteSource::EscalationWeak`] | `decided` |
//! | escalation | none | `escalation` / `latched` | [`RouteSource::EscalationLatch`] | `retained` |
//! | escalation | none | `escalation` / `escalate` | [`RouteSource::EscalationLatch`] | `decided` |
//! | escalation | none | `fallback` | [`RouteSource::EscalationFallback`] | — |
//! | escalation | none | anything else | [`RouteSource::EscalationLatch`] | — |
//! | advisor | served | `advisor` / `approve` | [`RouteSource::AdvisorApprove`] | `decided` |
//! | advisor | served | `advisor` / `fail_open` | [`RouteSource::AdvisorFailOpen`] | the recorded failure |
//! | advisor | served | none | [`RouteSource::AdvisorPass`] | — |
//! | advisor | none | `advisor` / `redo` | [`RouteSource::AdvisorRedo`] | `decided` |
//! | advisor | none | none | [`RouteSource::AdvisorExhausted`] | — |
//!
//! "—" records no judge call: none was made.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::Value;
use switchyard_libsy::{drive as libsy_drive, LibsyError, RoutingOutcome};
use switchyard_protocol::{LlmRequest, Request};
use switchyard_translation::{TranslationEngine, TranslationPolicy, WireFormat};

use super::drive::{reserve, DriveNotes};
use super::{budget::JudgeBudget, DrivenEntry, GatedKind};
use crate::config::CallBounds;
use crate::error::ShuntError;
use crate::routing::context::libsy_metadata;
use crate::routing::outcome::RouteSource;
use crate::routing::serve::gated::{
    gated_call, GatedCapture, GatedRequest, RetainedTurn, UpstreamFailure,
};
use crate::routing::serve::{judge_call, AdmittedContext};
use crate::server::AppState;

/// What one gated drive decided.
pub(crate) enum GatedDecision {
    /// Serve the retained turn: its bytes, as captured.
    Replay {
        turn: RetainedTurn,
        target: String,
        source: RouteSource,
        judge_outcome: Option<&'static str>,
    },
    /// Nothing retained is served; `target` answers live. `append` is the
    /// messages a REDO adds to the caller's `messages` — the discarded turn's
    /// echo and the advisor's plan — and empty for every other dispatch.
    Dispatch {
        target: String,
        source: RouteSource,
        judge_outcome: Option<&'static str>,
        append: Vec<Value>,
    },
    /// Nothing is served but `response`. `relayed_status` is true when it is
    /// an upstream's own non-`2xx` answer the client path would have returned
    /// as a success, false for an error the caller receives as one.
    Fail {
        target: String,
        source: RouteSource,
        response: axum::response::Response,
        message: String,
        relayed_status: bool,
    },
}

/// Drive a gated `entry` for one admitted request.
///
/// Unlike [`super::drive`] this can refuse the turn — a gated algorithm's own
/// failure leaves no servable answer, and a nonterminal advisor turn must not
/// be served — but every refusal happens before a header is committed.
pub(crate) async fn drive_gated(
    state: &AppState,
    admitted: &AdmittedContext<'_>,
    entry: &DrivenEntry,
    gated: &GatedRequest<'_>,
    headers: &HeaderMap,
) -> GatedDecision {
    let kind = entry
        .gated
        .expect("drive_gated runs only for a gated entry");
    let metadata = libsy_metadata(headers);
    let key = JudgeBudget::key(metadata.session_id.as_deref(), metadata.agent_id.as_deref());
    // No pre-drive `budget.used() >= max` fast path here, unlike
    // `super::drive`, and deliberately scoped to gated entries. Short-circuiting
    // a spent session to the fail-open target would override the algorithm's
    // own state: a latched escalation session would be sent back to the weak
    // tier it had already been judged unfit for, and an advisor whose review
    // budget is spent answers live anyway. The reservation in the call closure
    // below still refuses every judge call past `max_judge_calls`, which libsy
    // folds into its own fail-open. The ungated fast path's cost to an
    // affinity replay is tracked as #648.
    let engine = TranslationEngine::default();
    let policy = TranslationPolicy::default();
    let decoded =
        match engine.decode_request(WireFormat::AnthropicMessages, gated.body.json(), &policy) {
            Ok(decoded) => decoded,
            Err(error) => {
                tracing::warn!(
                    router = %admitted.router_id(),
                    error = %error,
                    "gated routing skipped: the request did not decode to the neutral IR"
                );
                return untranslatable(entry, kind);
            }
        };
    let neutral = Request {
        llm_request: decoded.request,
        raw_request: None,
        metadata: Some(metadata),
    };

    let bounds = entry.bounds;
    let notes = DriveNotes::default();
    let notes = &notes;
    let slot = Mutex::new(None);
    let slot = &slot;
    let calls = AtomicU32::new(0);
    let calls = &calls;
    let request_used = AtomicU32::new(0);
    let request_used = &request_used;
    let outcome = tokio::time::timeout(
        gated_deadline(bounds),
        libsy_drive(
            entry.algorithm.clone(),
            neutral,
            entry.models.clone(),
            |call| {
                // The gated turn is not a judge call and is not charged to
                // `max_judge_calls`: it is the answer the caller asked for, and
                // charging it would let the budget refuse the turn itself.
                let first = calls.fetch_add(1, Ordering::Relaxed) == 0;
                let reserved = !first
                    && reserve(
                        &entry.budget,
                        key.as_ref(),
                        request_used,
                        bounds.max_judge_calls,
                    );
                async move {
                    if first {
                        return gated_call(gated, call, bounds, slot).await;
                    }
                    if !reserved {
                        notes.refuse_budget();
                        return call.respond(Err(LibsyError::AlgorithmError {
                            message: "max_judge_calls is spent for this session".to_string(),
                        }));
                    }
                    judge_call(state.clone(), admitted, call, bounds, notes.failure()).await
                }
            },
        ),
    )
    .await;
    let captured = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match outcome {
        Ok(Ok(outcome)) => decide(entry, kind, outcome, captured, notes),
        Ok(Err(error)) => {
            // Labels only: the error can carry an upstream body, and this line
            // rides into every log sink the operator configured.
            tracing::warn!(
                router = %admitted.router_id(),
                algorithm = entry.algorithm_label,
                error_kind = libsy_error_kind(&error),
                "gated routing produced no outcome"
            );
            refuse(entry, captured)
        }
        Err(_) => refuse(entry, captured),
    }
}

/// The outer guard on one gated drive: the gated call at its full duration,
/// every judge call the budget can admit at its full deadline, and a second
/// of slack — saturating, like [`super::drive::drive_deadline`].
pub(super) fn gated_deadline(bounds: CallBounds) -> Duration {
    bounds
        .gated_max_duration
        .saturating_add(bounds.judge_timeout.saturating_mul(bounds.max_judge_calls))
        .saturating_add(Duration::from_secs(1))
}

/// Which judge-outcome label a reading records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JudgeReading {
    /// `decided`: a judge answered inside its bounds.
    Decided,
    /// `retained`: an earlier verdict held and no call was made.
    Retained,
    /// Whatever [`DriveNotes`] recorded — the failed call's own label, or
    /// `budget_exhausted` — defaulting to `invalid_reply`.
    FromNotes,
    /// No judge was asked, so nothing is recorded.
    Unrecorded,
}

/// The evidence map in [the module docs](self), as a pure function of the
/// three facts it reads.
pub(super) fn read_evidence(
    kind: GatedKind,
    served_retained: bool,
    evidence: Option<&Value>,
) -> (RouteSource, JudgeReading) {
    let field = |name| {
        evidence
            .and_then(|evidence| evidence.get(name))
            .and_then(Value::as_str)
    };
    let (source, verdict) = (field("source"), field("verdict"));
    match (kind, served_retained) {
        (GatedKind::Escalation, true) => match source {
            Some("fail_open") => (RouteSource::DrivenFailOpen, JudgeReading::FromNotes),
            _ => (RouteSource::EscalationWeak, JudgeReading::Decided),
        },
        (GatedKind::Escalation, false) => match (source, verdict) {
            (Some("escalation"), Some("latched")) => {
                (RouteSource::EscalationLatch, JudgeReading::Retained)
            }
            (Some("escalation"), Some("escalate")) => {
                (RouteSource::EscalationLatch, JudgeReading::Decided)
            }
            (Some("fallback"), _) => (RouteSource::EscalationFallback, JudgeReading::Unrecorded),
            _ => (RouteSource::EscalationLatch, JudgeReading::Unrecorded),
        },
        (GatedKind::Advisor, true) => match (source, verdict) {
            (Some("advisor"), Some("approve")) => {
                (RouteSource::AdvisorApprove, JudgeReading::Decided)
            }
            (Some("advisor"), Some("fail_open")) => {
                (RouteSource::AdvisorFailOpen, JudgeReading::FromNotes)
            }
            _ => (RouteSource::AdvisorPass, JudgeReading::Unrecorded),
        },
        (GatedKind::Advisor, false) => match (source, verdict) {
            (Some("advisor"), Some("redo")) => (RouteSource::AdvisorRedo, JudgeReading::Decided),
            _ => (RouteSource::AdvisorExhausted, JudgeReading::Unrecorded),
        },
    }
}

/// Read one successful outcome into a decision.
fn decide(
    entry: &DrivenEntry,
    kind: GatedKind,
    outcome: RoutingOutcome,
    captured: Option<GatedCapture>,
    notes: &DriveNotes,
) -> GatedDecision {
    let Some(selected) = outcome.selected_model_ids.first().map(ToString::to_string) else {
        return gateway_failure(entry, "gated routing selected no target");
    };
    if !entry.targets.contains(&selected) {
        // Defensive, as in `super::drive::decide`: an id outside the entry's
        // own set would resolve through `server.default_provider` as a
        // literal upstream model name.
        return gateway_failure(entry, "gated routing selected a target outside its set");
    }
    let evidence = outcome
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.evidence.as_ref());
    let served_retained = outcome.response.is_some();
    let (source, reading) = read_evidence(kind, served_retained, evidence);
    let judge_outcome = match reading {
        JudgeReading::Decided => Some("decided"),
        JudgeReading::Retained => Some("retained"),
        JudgeReading::FromNotes => Some(notes.label("invalid_reply")),
        JudgeReading::Unrecorded => None,
    };
    if served_retained {
        // libsy's response is only the signal. The bytes served are the ones
        // shunt captured, so a signal without a capture has nothing to serve.
        return match captured {
            Some(GatedCapture::Retained(turn)) => GatedDecision::Replay {
                turn,
                target: selected,
                source,
                judge_outcome,
            },
            _ => gateway_failure(entry, "gated routing kept a turn that was not retained"),
        };
    }
    let append = if source == RouteSource::AdvisorRedo {
        match redo_tail(&outcome.request) {
            Ok(append) => append,
            // Dispatching without the feedback would re-run the turn the
            // advisor just rejected, so an unencodable REDO is a failure.
            Err(()) => return gateway_failure(entry, "the advisor's REDO could not be encoded"),
        }
    } else {
        Vec::new()
    };
    GatedDecision::Dispatch {
        target: selected,
        source,
        judge_outcome,
        append,
    }
}

/// The two messages a REDO appends — the discarded turn's echo and the
/// advisor's plan, which upstream pushes onto the request it routes — encoded
/// back to Anthropic JSON so they can be appended to the caller's own body.
///
/// Only the tail is encoded, not the whole rewritten request: the dispatch
/// sends the client's bytes with these two messages added, never a
/// re-encoding of the conversation.
fn redo_tail(request: &Request) -> Result<Vec<Value>, ()> {
    let messages = &request.llm_request.messages;
    let tail = messages
        .get(messages.len().checked_sub(2).ok_or(())?..)
        .ok_or(())?;
    let tail = LlmRequest {
        messages: tail.to_vec(),
        ..LlmRequest::default()
    };
    let encoded = TranslationEngine::default()
        .encode_request(
            WireFormat::AnthropicMessages,
            &tail,
            &TranslationPolicy::default(),
        )
        .map_err(drop)?;
    encoded
        .body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| messages.len() == 2)
        .cloned()
        .ok_or(())
}

/// The drive ended without an outcome. An upstream refusal the gated call
/// recorded is relayed unchanged; anything else is the gateway's own `502`.
fn refuse(entry: &DrivenEntry, captured: Option<GatedCapture>) -> GatedDecision {
    match captured {
        Some(GatedCapture::UpstreamError(UpstreamFailure::Answered {
            response, status, ..
        })) => GatedDecision::Fail {
            target: entry.fail_open.clone(),
            source: RouteSource::GatedError,
            message: format!("the gated upstream answered {status}"),
            response,
            relayed_status: true,
        },
        Some(GatedCapture::UpstreamError(UpstreamFailure::Failed { message, response })) => {
            GatedDecision::Fail {
                target: entry.fail_open.clone(),
                source: RouteSource::GatedError,
                message,
                response,
                relayed_status: false,
            }
        }
        // A cut advisor turn lands here: libsy propagates the transport error
        // from buffering it, and neither a REDO nor a second executor call is
        // made. The upstream did answer `2xx`, but the caller never saw it —
        // no header is committed until this response — so ADR-0002's
        // post-2xx rule holds and the failure is the gateway's to report.
        Some(GatedCapture::Cut(reason)) => {
            gateway_failure(entry, &format!("the gated turn was discarded: {reason}"))
        }
        // The algorithm failed after a turn was retained (an advisor review
        // under `fail_open = false`), or before one was made. Either way there
        // is no servable turn.
        _ => gateway_failure(entry, "gated routing produced no servable turn"),
    }
}

/// A gateway-owned `502` in the Anthropic error shape. The message is the
/// gateway's own wording: libsy's error text can carry an upstream body.
fn gateway_failure(entry: &DrivenEntry, message: &str) -> GatedDecision {
    GatedDecision::Fail {
        target: entry.fail_open.clone(),
        source: RouteSource::GatedError,
        response: ShuntError::new(StatusCode::BAD_GATEWAY, "api_error", message).into_response(),
        message: message.to_string(),
        relayed_status: false,
    }
}

/// The request could not be decoded to the neutral IR, so nothing was driven
/// and nothing retained: the entry's fail-open target answers live, as an
/// ungated drive's translation failure does.
fn untranslatable(entry: &DrivenEntry, kind: GatedKind) -> GatedDecision {
    GatedDecision::Dispatch {
        target: entry.fail_open.clone(),
        source: match kind {
            GatedKind::Escalation => RouteSource::DrivenFailOpen,
            GatedKind::Advisor => RouteSource::AdvisorFailOpen,
        },
        judge_outcome: Some("translation_failed"),
        append: Vec::new(),
    }
}

/// A closed label for a drive error, for the log line.
fn libsy_error_kind(error: &LibsyError) -> &'static str {
    match error {
        LibsyError::ClientCall { .. } => "client_call",
        LibsyError::AlgorithmError { .. } => "algorithm_error",
        LibsyError::NoTargets => "no_targets",
        _ => "other",
    }
}

#[cfg(test)]
mod tests;
