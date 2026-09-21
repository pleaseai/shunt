use std::time::Instant;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    adapters::{
        anthropic::AnthropicAdapter, cursor::CursorAdapter, responses::ResponsesAdapter, Adapter,
        AdapterError,
    },
    auth::{inbound::ConsumedBy, slots::ShuntCredentials},
    config::CountTokens,
    count_tokens,
    error::ShuntError,
    routing::{self, AdapterKind},
    server::AppState,
};

use super::{
    count_tokens_unsupported, is_count_tokens, normalize_request_body, safeguards, ForwardError,
};

pub(crate) mod chain;

// Re-exported at the old path: the adapters and the committed streaming chain
// classify statuses with these, and the split that moved the loop into `chain`
// is not a change to where that rule is spelled.
pub(crate) use chain::{failure_priority, is_advance_status};

pub(super) async fn forward(
    state: AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let max_request_bytes = state.config.server.limits.max_request_bytes;
    if crate::http_tuning::content_length_exceeds(headers, max_request_bytes) {
        return Err(ForwardError {
            message: "request body exceeds the configured limit".to_string(),
            response: Box::new(crate::http_tuning::request_too_large(false).await),
        });
    }
    let body = crate::http_tuning::read_body(body, max_request_bytes, false)
        .await
        .map_err(|response| ForwardError {
            message: if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
                "request body exceeds the configured limit"
            } else {
                "failed to read request body"
            }
            .to_string(),
            response,
        })?;
    let mut body = crate::request::RequestBody::parse(body.to_vec())
        .map_err(routing::invalid_routing_request)
        .map_err(|error| ForwardError {
            message: "failed to route request".to_string(),
            response: Box::new(error.into_response()),
        })?;
    normalize_request_body(&mut body);
    // Claude Code's auto mode asks the API to classify the session's own tool
    // uses server-side (`safeguards` + the `dangerous-tool-use-…` beta). Only
    // api.anthropic.com answers it, and a completed response carrying no
    // `safeguard_results` at all makes the client retire the server classifier
    // for the rest of the session — so the requested types travel to the
    // response side, where the answer is synthesized if the upstream gave none.
    // Empty for every request that did not ask, which wraps nothing.
    //
    // Read before `apply_handoff_note` below may rewrite the system prompt: the
    // types are what the *client* asked to have classified, and a gateway-added
    // block is not part of that request.
    let requested_safeguards = safeguards::requested_types(body.json());
    // Context for a `[models.router]` entry, should the requested id turn
    // out to be one. Built unconditionally because construction is a handful of
    // field moves — the headers are borrowed, not read; the `x-claude-code-*`
    // hints are parsed inside `stage::select`, which only a `[[models]]` entry
    // that configures a router ever reaches.
    // Async because the driven lane runs inference; `None` for every id that
    // is not a `prefill_router` entry, which costs one `HashMap` miss — and a
    // constant `None` in a build without the feature.
    let prefill = routing::prefill::decide(&state.prefill_routers, body.json(), headers).await;
    let stage = routing::stage::StageContext {
        store: &state.stage_router,
        request: body.json(),
        headers,
        // A count_tokens probe must reach the same tier as the turn it is
        // measuring without recording it: Claude Code sends those with a history
        // one turn behind, so a committing probe would let the stale history
        // drive the session's pin.
        read_only: is_count_tokens(uri),
        now: started_at,
        pending: std::cell::Cell::new(None),
        decided: std::cell::Cell::new(None),
        consult: std::cell::Cell::new(None),
        prefill,
    };
    let (mut routes, requested_model) =
        routing::resolve_request_chain_value(&state.config, body.json(), Some(&stage)).map_err(
            |error| ForwardError {
                message: "failed to route request".to_string(),
                response: Box::new(error.into_response()),
            },
        )?;
    crate::observability::record_requested_model(&requested_model);
    let model_key = routing::strip_context_window_hint(&requested_model);
    // A `count_tokens` probe never enters the driven lane (ADR-0005 §3): it is
    // answered from the session's pin or the algorithm's no-model-call
    // decision, makes zero judge calls, and keeps today's first-route-only gate
    // and dispatch. `None` here is what excludes it, before an envelope is even
    // computed.
    let driven = (!is_count_tokens(uri))
        .then(|| state.config.driven_stage(model_key))
        .flatten();
    if is_count_tokens(uri) {
        // count_tokens answers from the first chain element only, so gate and
        // dispatch against just that element: a later credential-injecting
        // fallback must not force inbound auth on an otherwise-passthrough
        // count_tokens request.
        routes.truncate(1);
    }
    // Taken here rather than with the other stage outputs below, because this
    // is the fact that decides which chain the request is admitted against.
    let consult = stage.consult.take();
    // A turn that will consult a judge is gated against the entry's whole
    // dependency envelope rather than against the chain its router happened to
    // pick: the judge has not run yet, and it must not run for a caller who is
    // about to be refused. So a passthrough answer tier paired with a
    // credential-injecting judge demands `[server.auth]` — it is not a
    // passthrough entry.
    //
    // The consultation, not the entry's static shape, is what selects it. An
    // entry can be driven and still resolve this particular turn without a
    // judge: a `[models.subagents]` overlay answers a delegated turn before the
    // router runs at all (`routing::resolve_chain`), and a decisive turn is
    // settled by the signals. Gating those against the envelope would demand a
    // token for a credential the request cannot reach — which is a refusal the
    // caller cannot act on, since the chain they were actually routed to
    // injects nothing. Every other request gates against the chain it already
    // resolved and allocates nothing here.
    let envelope;
    let admission: &[routing::Route] = if driven.is_some() && consult.is_some() {
        envelope = routing::envelope::dependency_envelope(&state.config, model_key);
        &envelope
    } else {
        &routes
    };
    let (base_headers, inbound) =
        check_inbound_auth(&state, admission, headers).map_err(|error| *error)?;
    enforce_managed_model_policy(&state, inbound.gateway_claims.as_ref(), &requested_model)
        .map_err(|error| *error)?;
    // The request is admitted, so the tier it was routed at may be recorded —
    // and, for a driven entry, a judge may now be consulted. Both gates above
    // rejected before this line, and neither had run when the chain was
    // resolved: `check_inbound_auth` needs a chain to decide against, which for
    // a driven entry is the dependency envelope built above.
    // Taken, not committed: a driven entry may still move the tier below, and a
    // pin written before the verdict would record the tier the request was
    // *not* dispatched at. The commit happens once, after the drive.
    let mut pending = stage.pending.take();
    // The same boundary governs the counters: a rejected request is routed but
    // never served, so counting it would report traffic the gateway did not
    // carry. `None` for every request whose id carries no router.
    let mut router_outcome = stage.decided.take();
    // `stage` borrows the parsed body, and the handoff note below mutates it.
    // Every output — the pin, the outcome, the consultation — has already been
    // taken, so the borrow has nothing left to serve; it also holds `Cell`s and
    // must not live across the judge's `.await`. `read_only` is copied out
    // rather than re-derived from the URI so the note's exclusion stays tied to
    // the same flag the store decided against.
    let read_only = stage.read_only;
    drop(stage);
    // The drive. Between admission and the pin commit, because both boundaries
    // matter: a refused caller must spend no judge call, and the pin must
    // record the tier the turn was actually dispatched at.
    if let (Some((stage_cfg, classifier)), Some(consult), Some(outcome)) =
        (driven, consult, router_outcome.as_mut())
    {
        let bounds = stage_cfg.bounds();
        let verdict = if consult.judge_calls_used >= bounds.max_judge_calls {
            // Checked before the call is charged, so the budget is a ceiling on
            // calls made rather than on calls attempted.
            routing::judge::JudgeOutcome::FailOpen("budget_exhausted")
        } else {
            if let Some(pin) = pending.as_mut() {
                pin.record_judge_call();
            }
            // Cloned so the mint's borrow is of a local, leaving `outcome`
            // free to be rewritten by the verdict below.
            let router_id = outcome.model.clone();
            let admitted =
                crate::routing::serve::AdmittedContext::mint(&inbound, headers, &router_id);
            routing::judge::consult(&state, &admitted, stage_cfg, classifier, body.json()).await
        };
        // The outcome label only — never the verdict text, the `crux`, or any
        // message content: this line rides into every log sink the operator
        // configured, and the judge is reading the caller's transcript.
        tracing::info!(
            router = %outcome.model,
            judge = %classifier.target,
            outcome = verdict.label(),
            "consulted the stage-router judge"
        );
        // Recorded on every outcome, including the budget-exhausted one that
        // made no call at all — so the series totals to "turns that wanted a
        // judge", which is the number an operator tunes `max_judge_calls`
        // against.
        crate::metrics::record_judge_call(
            &outcome.model,
            routing::judge::ALGORITHM,
            verdict.label(),
        );
        if let routing::judge::JudgeOutcome::Decided(tier) = verdict {
            let target = tier.target(stage_cfg);
            routes = routing::resolve_target_chain(&state.config, target, &outcome.model);
            outcome.target = target.to_string();
            outcome.source = crate::routing::outcome::RouteSource::Stage(
                tier,
                crate::routing::stage::StageSource::Scorer(
                    switchyard_libsy::DecisionSource::LlmClassifier,
                ),
            );
            if let Some(pin) = pending.as_mut() {
                pin.set_tier(tier);
            }
        }
    }
    // The write reports the tier change it actually made, which is what the
    // flip counter must count: two concurrent turns of one session decide
    // against the same snapshot, so a flip derived from that snapshot would be
    // counted twice for a session that moved once.
    let stage_flip = pending.and_then(|pin| state.stage_router.commit(pin, started_at));
    if let Some(outcome) = &router_outcome {
        // `outcome.model`, not `requested_model`: the router was matched on the
        // id with any `[1m]` hint stripped, and the session was keyed on it too,
        // so labelling by the raw id would split one router across two series.
        // `read_only` rather than `is_count_tokens(uri)` again: it is the same
        // value, and reading the flag ties this exclusion to the one the store
        // already decided against instead of re-deriving it here.
        if !read_only {
            // Every algorithm, including stage: one series a reader can total
            // across router types (ADR-0005 §7).
            crate::metrics::record_router_decision(
                &outcome.model,
                outcome.algorithm,
                &outcome.target,
                outcome.source.as_label(),
            );
            // The shipped stage counter, unchanged: `None` for every algorithm
            // that has no tier, so `random` and `noop` traffic never appears in
            // a series a dashboard reads as stage-router decisions.
            if let Some((tier, source)) = outcome.source.stage_labels() {
                crate::metrics::record_stage_decision(&outcome.model, tier, source);
            }
        }
        if let Some((from, to)) = stage_flip {
            crate::metrics::record_stage_flip(&outcome.model, from.as_label(), to.as_label());
        }
    }
    // The handoff note rides the admitted request, so it is applied on the same
    // boundary the counters are: a rejected turn never reaches an upstream and
    // must not have its prompt rewritten on the way to being refused.
    apply_handoff_note(
        &state,
        &mut body,
        router_outcome.as_ref(),
        stage_flip,
        read_only,
    );
    let router_stamp = router_outcome.as_ref().map(|outcome| RouterStamp {
        routed_model: outcome.target.as_str(),
        source: outcome.source.as_label(),
    });
    // The same decision, owned, for the committed streaming chain below: it
    // consumes its request and awaits, so it cannot carry the borrow above.
    let owned_router_stamp = router_outcome.as_ref().map(|outcome| OwnedRouterStamp {
        routed_model: outcome.target.clone(),
        source: outcome.source.as_label(),
    });

    let first_route = routes
        .first()
        .expect("route chains are non-empty after resolution");
    if is_count_tokens(uri) {
        return count_tokens_response(
            state.clone(),
            first_route.clone(),
            uri,
            &base_headers,
            &inbound,
            body,
            &requested_model,
        )
        .await;
    }

    // Kept in `forward` rather than moved into `chain`: the committed streaming
    // chain races its attempts and returns its own response, so it is a
    // different dispatch shape, not a mode of the ordered loop. An internal
    // judge call never takes it — its body is non-streaming, which is what
    // `chain_stream_applies` gates on — so `run_chain` has no need of it.
    if super::chain_stream::chain_stream_applies(&state, &routes, &body) {
        return super::chain_stream::forward_chain_stream(
            super::chain_stream::ChainStreamRequest {
                state: state.clone(),
                primary_origin: chain::primary_origin(&state, &routes),
                routes,
                uri: uri.clone(),
                base_headers,
                inbound,
                body,
                requested_model,
                started_at,
                router_stamp: owned_router_stamp,
            },
        )
        .await;
    }
    let chain = chain::ChainRequest {
        state: state.clone(),
        routes,
        uri,
        base_headers: &base_headers,
        inbound: &inbound,
        body,
        requested_model: &requested_model,
        router_stamp,
        caller: "client",
        // A client turn is never capped: the reply is bounded by the request
        // the caller made, and a cap here would be a new way for ordinary
        // traffic to fail. Only `routing::serve`'s internal calls set one.
        response_byte_cap: None,
    };
    let success = chain::run_chain(chain).await?;
    Ok(observe_response(
        success.status,
        success.response,
        success.provider,
        success.model,
        started_at,
        &requested_safeguards,
        max_request_bytes,
    )
    .await)
}

/// Append the entry's `handoff_notes` text to the forwarded system prompt, on
/// the turns a signal moved the tier.
///
/// `flip` is [`StageContext::commit`]'s return — the transition the write
/// *actually made* — and not a flag the decision computed against the pin it
/// read. Those differ under concurrency, which is the same reason the flip
/// counter above is stamped from it: two admitted turns of one session can both
/// read an efficient pin, both choose capable, and both believe they moved the
/// tier, but only the first commit does. Gating on the commit keeps the second
/// from telling its model it is taking over a conversation that had already
/// changed hands. It also subsumes the sequential cases — a first turn, and a
/// signal that merely re-confirms the pinned tier, both commit no transition.
///
/// Runs after admission, so a rejected turn's prompt is never rewritten, and
/// never for a `count_tokens` probe: a probe measures the turn the client is
/// about to send, and adding a block the real turn may not carry would make the
/// count describe a different request.
///
/// The config is re-read here rather than carried on the outcome because the
/// outcome is deliberately observability-only (ADR-0004 §5) — it holds labels,
/// not policy — and `outcome.model` is already the id validation guarantees
/// uniquely names one `[[models]]` entry.
fn apply_handoff_note(
    state: &AppState,
    body: &mut crate::request::RequestBody,
    outcome: Option<&crate::routing::outcome::RouterOutcome>,
    flip: Option<(
        crate::routing::stage::StageTier,
        crate::routing::stage::StageTier,
    )>,
    read_only: bool,
) {
    if read_only {
        return;
    }
    let Some(outcome) = outcome else {
        return;
    };
    let crate::routing::outcome::RouteSource::Stage(tier, source) = outcome.source else {
        return;
    };
    let Some(notes) = state
        .config
        .models
        .iter()
        .find(|model| model.id == outcome.model)
        .and_then(|model| model.router.as_ref())
        .and_then(crate::config::RouterConfig::stage)
        .and_then(|stage| stage.handoff_notes.as_ref())
    else {
        return;
    };
    let Some(note) = crate::routing::handoff::note_for(notes, tier, source, flip.is_some()) else {
        return;
    };
    let note = note.to_string();
    body.mutate(|request| crate::routing::handoff::append_system_block(request, &note));
}

async fn count_tokens_response(
    state: AppState,
    route: routing::Route,
    uri: &Uri,
    base_headers: &HeaderMap,
    inbound: &InboundContext,
    body: crate::request::RequestBody,
    requested_model: &str,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let provider = route.provider.clone();
    let upstream_model = route.upstream_model.clone();
    let result = if route.adapter == AdapterKind::Noop {
        // A noop entry never calls an upstream, so there is no provider whose
        // `count_tokens` mode could apply and nothing to count: the turn it is
        // probing will carry no content either.
        Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({ "input_tokens": 0 })).into_response(),
        ))
    } else if matches!(
        route.adapter,
        AdapterKind::Responses
            | AdapterKind::Cursor
            | AdapterKind::Gemini
            | AdapterKind::AntigravityCli
    ) {
        let mode = state
            .config
            .provider(&provider)
            .map(|provider| provider.count_tokens)
            .unwrap_or(CountTokens::Estimate);
        Ok(match mode {
            CountTokens::Tiktoken => {
                // Count over the tree the inbound parse already produced instead of
                // re-deserializing the same buffer on the blocking pool. Moving only
                // the `Arc` also keeps the raw bytes out of the task: `spawn_blocking`
                // cannot be cancelled, so a client that disconnects mid-request leaves
                // this encode running to completion, and anything it captured stays
                // resident (up to a 64 MB body) for that whole window.
                let request = body.json_arc();
                let input_tokens = tokio::task::spawn_blocking(move || {
                    count_tokens::count_input_tokens_value(&request)
                })
                .await
                .unwrap_or(0);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response(),
                )
            }
            CountTokens::Estimate => count_tokens_unsupported(),
        })
    } else {
        // Single element (count_tokens uses only the first chain entry), so it is
        // the primary route — the caller's credential is kept without any origin
        // parsing.
        let headers = headers_for_route(&state, &route, base_headers, inbound, true, None);
        dispatch(
            state, route, uri, &headers, body,
            // A `count_tokens` probe never enters the driven lane, so it is
            // always a client path.
            None,
        )
        .await
    };
    match result {
        Ok((status, mut response)) => {
            // Deliberately unstamped with the stage pair. A `count_tokens`
            // probe is routed but not recorded, and on a session that is not
            // yet pinned it scores its own one-turn-behind history — so the
            // tier it resolves to is not necessarily the tier the turn it is
            // measuring will get. Reporting it as this session's routed tier
            // would state something the gateway has not decided.
            stamp_gateway_headers(
                &mut response,
                &provider,
                requested_model,
                &upstream_model,
                None,
            );
            crate::observability::record_span_outcome(&provider, status);
            crate::observability::capture_upstream_outcome(&provider, requested_model, status);
            Ok((status, response))
        }
        Err(error) => {
            let mut response = error.response;
            // Deliberately unstamped with the stage pair. A `count_tokens`
            // probe is routed but not recorded, and on a session that is not
            // yet pinned it scores its own one-turn-behind history — so the
            // tier it resolves to is not necessarily the tier the turn it is
            // measuring will get. Reporting it as this session's routed tier
            // would state something the gateway has not decided.
            stamp_gateway_headers(
                &mut response,
                &provider,
                requested_model,
                &upstream_model,
                None,
            );
            let status = response.status();
            crate::observability::record_span_outcome(&provider, status);
            crate::observability::capture_upstream_outcome(&provider, requested_model, status);
            Err(ForwardError {
                message: error.message,
                response,
            })
        }
    }
}

async fn dispatch(
    state: AppState,
    route: routing::Route,
    uri: &Uri,
    headers: &HeaderMap,
    body: crate::request::RequestBody,
    response_byte_cap: Option<usize>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    match route.adapter {
        AdapterKind::Anthropic => {
            AnthropicAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
        AdapterKind::Responses => {
            ResponsesAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
        AdapterKind::Cursor => {
            CursorAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
        AdapterKind::Gemini => {
            crate::adapters::gemini::GeminiAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
        AdapterKind::AntigravityCli => {
            crate::adapters::antigravity::AntigravityAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
        AdapterKind::Noop => {
            crate::adapters::noop::NoopAdapter
                .forward(state, route, uri, headers, body, response_byte_cap)
                .await
        }
    }
}

async fn observe_response(
    status: StatusCode,
    response: axum::response::Response,
    provider: String,
    model: String,
    started_at: Instant,
    requested_safeguards: &[String],
    max_body_bytes: usize,
) -> (StatusCode, axum::response::Response) {
    // Ahead of the metrics observer so the observer sees what the client will:
    // the synthesis only adds a field to `message_delta`, leaving the frames the
    // observer samples (`stop_reason`, usage, error events) exactly as relayed.
    let response = safeguards::synthesize(response, requested_safeguards, max_body_bytes).await;
    let response = crate::stream_metrics::observe_response(
        response,
        crate::stream_metrics::Protocol::Anthropic,
        provider,
        model,
        started_at,
    );
    (status, response)
}

fn enforce_managed_model_policy(
    state: &AppState,
    claims: Option<&crate::gateway::jwt::Claims>,
    requested_model: &str,
) -> Result<(), Box<ForwardError>> {
    let Some((auth, claims)) = state.gateway_auth.as_ref().zip(claims) else {
        return Ok(());
    };
    let Some(available_models) = auth
        .managed_settings(&claims.email)
        .and_then(crate::gateway::managed::available_models)
    else {
        return Ok(());
    };
    let policy_model = routing::strip_context_window_hint(requested_model);
    if available_models
        .iter()
        .any(|model| model.as_str() == Some(policy_model))
    {
        return Ok(());
    }
    let message =
        format!("model \"{requested_model}\" is not permitted by this gateway's managed policy");
    Err(Box::new(ForwardError {
        message: message.clone(),
        response: Box::new(
            ShuntError::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
                .into_response(),
        ),
    }))
}

#[derive(Clone)]
pub(crate) struct InboundContext {
    gateway_claims: Option<crate::gateway::jwt::Claims>,
    client: Option<String>,
    static_client: bool,
}

impl InboundContext {
    /// The context a gateway-internal call rides on: no gateway claims, no
    /// client, not a static client.
    ///
    /// The judge's own admission is the caller's — it was decided by
    /// [`check_inbound_auth`] against the dependency envelope before the drive
    /// was entered — and the headers this rides carry no caller credential at
    /// all (`routing::serve::judge_headers`). So there is nothing here to
    /// inherit: an inherited `static_client` would stamp
    /// `x-shunt-inbound-client` with the caller's identity on a call the caller
    /// did not make, and inherited claims would re-run a policy that has
    /// already been enforced on the advertised id.
    pub(crate) fn internal() -> Self {
        Self {
            gateway_claims: None,
            client: None,
            static_client: false,
        }
    }
}

/// Authenticate once against the whole route chain. Client credential stripping
/// is deferred to [`headers_for_route`] so a passthrough attempt retains the
/// caller's upstream credential while credential-injecting attempts cannot leak
/// it. On failover, a passthrough attempt keeps the credential only while its
/// origin matches the primary upstream's, so a host-specific token is never
/// replayed to a different origin (a same-origin fallback still carries it).
pub(crate) fn check_inbound_auth(
    state: &AppState,
    routes: &[routing::Route],
    headers: &HeaderMap,
) -> Result<(HeaderMap, InboundContext), Box<ForwardError>> {
    let mut forwarded = headers.clone();
    // Every slot shunt reserves by *name* — the fixed `x-shunt-*` names plus
    // whatever `[server.auth]`/`[server.admin]` are configured to — goes here,
    // once, through the shared enumeration in `auth::slots`. That module owns
    // why the two configured headers are treated asymmetrically (the static
    // header is removed even when it names a shared slot; the admin header is
    // not, because the shared slots carry the caller's own upstream credential
    // and are cleared by value in `headers_for_route` instead).
    ShuntCredentials::from_state(state).strip_reserved_slots(&mut forwarded);

    let gateway_claims = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(headers));
    // One definition of "passthrough" for both the auth gate and header
    // handling. The inline form this replaced disagreed with the helper on an
    // unknown provider — it treated one as *not* credential-injecting, which
    // skipped `[server.auth]` rather than demanding it. Fail closed instead.
    let injects_credential = routes
        .iter()
        .any(|route| !is_passthrough_route(state, route));
    if !injects_credential || (state.inbound_auth.is_none() && state.gateway_auth.is_none()) {
        return Ok((
            forwarded,
            InboundContext {
                gateway_claims,
                client: None,
                static_client: false,
            },
        ));
    }

    let static_client = state
        .inbound_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_client(headers));
    if static_client.is_some() || gateway_claims.is_some() {
        let client = static_client
            .map(str::to_string)
            .or_else(|| gateway_claims.as_ref().map(|claims| claims.email.clone()))
            .expect("one composed authentication branch matched");
        tracing::info!(client = %client, "inbound client authenticated for route chain");
        return Ok((
            forwarded,
            InboundContext {
                gateway_claims,
                client: Some(client),
                static_client: static_client.is_some(),
            },
        ));
    }

    tracing::warn!("inbound auth failed: missing or invalid client credential");
    let message = if let Some(auth) = &state.inbound_auth {
        format!(
            "missing or invalid credential: this gateway requires a client token (via {}, Authorization: Bearer, or x-api-key) or gateway login",
            auth.header()
        )
    } else {
        "missing or invalid credential: sign in to this gateway and send the issued bearer token"
            .to_string()
    };
    Err(Box::new(ForwardError {
        message: "inbound authentication failed".to_string(),
        response: Box::new(
            ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
                .into_response(),
        ),
    }))
}

pub(crate) fn headers_for_route(
    state: &AppState,
    route: &routing::Route,
    base: &HeaderMap,
    inbound: &InboundContext,
    is_primary: bool,
    primary_origin: Option<&str>,
) -> HeaderMap {
    let injects_credential = !is_passthrough_route(state, route);
    if !injects_credential {
        // Passthrough forwards the caller's own upstream credential. That
        // credential is origin-specific to the primary upstream, so a slot is
        // kept only while the destination origin matches the primary's: a
        // failover attempt to a *different* origin strips both `authorization`
        // and `x-api-key` and fails closed rather than replay a host-specific
        // token off-origin. A same-origin failover (e.g. two passthrough
        // entries on one host) keeps a slot that holds the caller's own
        // upstream credential; the primary route is the origin the credential
        // was presented for, so it is kept without parsing any URL (the
        // single-upstream hot path).
        //
        // Within a same-origin attempt, each retained slot is also checked *by
        // value*, independently, against all three of shunt's own inbound
        // credentials — the gateway JWT, a configured static `[server.auth]`
        // token, and a `[server.admin]` credential — and stripped only if it
        // holds one, never
        // both slots just because one of them does. `authorization` and
        // `x-api-key` are the two slots an `apiKeyHelper` can fill (it fills
        // both), so any of shunt's own credentials can land in either or
        // both, beside a genuine upstream credential in the other slot that
        // must keep flowing. `check_inbound_auth`'s auth gate authenticates
        // once for the whole route chain, so a chain mixing mapped and
        // passthrough routes can still reach this branch same-origin with a
        // shunt-owned credential in one of these slots. An admin credential
        // reaches these slots because the admin surface accepts one in
        // `x-api-key` beside its own header, and it is the highest-value of
        // the three: it can provision upstream accounts. A dedicated
        // `x-shunt-token` header remains a good operational habit — it keeps
        // `authorization`/`x-api-key` free for the caller's real credential
        // without needing this per-value check at all (`docs/m4-inbound-auth.md`
        // §2) — but it is no longer the only thing standing between a static
        // token and the upstream: `ShuntCredentials::strip_consumed_slots`
        // (shared with `discovery/upstream.rs`, and enumerated once in
        // `auth::slots`) is applied to both slots for every credential kind, so
        // a static token delivered through a rotating-credential mechanism like
        // `apiKeyHelper` is stripped here too.
        let mut headers = base.clone();
        let same_origin = is_primary
            || matches!(
                (primary_origin, provider_origin(state, &route.provider).as_deref()),
                (Some(primary), Some(this)) if primary == this
            );
        if !same_origin {
            headers.remove("authorization");
            headers.remove("x-api-key");
            tracing::debug!(
                provider = %route.provider,
                "stripped passthrough credential slots: off_origin"
            );
            return headers;
        }
        for (slot, reason) in ShuntCredentials::from_state(state).strip_consumed_slots(&mut headers)
        {
            tracing::debug!(
                provider = %route.provider,
                slot,
                reason = reason_label(reason),
                "stripped passthrough credential slot"
            );
        }
        return headers;
    }

    let mut headers = base.clone();
    headers.remove("authorization");
    headers.remove("x-api-key");
    if inbound.static_client {
        if let Some(client) = inbound
            .client
            .as_deref()
            .and_then(|value| value.parse().ok())
        {
            headers.insert("x-shunt-inbound-client", client);
        }
    }
    headers
}

/// Whether a route's provider forwards the caller's own upstream credential
/// rather than injecting a gateway-held one.
///
/// One line, because the rule itself lives on
/// [`crate::config::Config::route_is_passthrough`]: config validation needs the
/// same predicate to reject a passthrough judge target (ADR-0005 §3) and has no
/// `AppState` to ask. Two copies would be two definitions of "passthrough", and
/// the failure mode of that drift is a config that validates and then cannot be
/// served.
fn is_passthrough_route(state: &AppState, route: &routing::Route) -> bool {
    state.config.route_is_passthrough(route)
}

/// The origin (scheme + host + port) of a provider's `base_url`, used to decide
/// whether a passthrough failover attempt targets the same origin the caller's
/// credential was presented for. `None` when the provider is unknown, its
/// `base_url` cannot be parsed, or the URL has an opaque origin — callers treat
/// that as "origin changed" and strip the credential (fail closed).
fn provider_origin(state: &AppState, provider: &str) -> Option<String> {
    let base_url = state.config.provider(provider)?.base_url.as_str();
    let origin = reqwest::Url::parse(base_url)
        .ok()?
        .origin()
        .ascii_serialization();
    (origin != "null").then_some(origin)
}

/// Label for a [`ConsumedBy`] match used only in a `tracing::debug!` reason
/// field — never the token value itself.
fn reason_label(reason: ConsumedBy) -> &'static str {
    match reason {
        ConsumedBy::GatewayJwt => "gateway_jwt",
        ConsumedBy::StaticToken => "static_token",
        ConsumedBy::AdminCredential => "admin_credential",
    }
}

pub(crate) fn stamp_gateway_model_header(response: &mut axum::response::Response, model: &str) {
    if let Ok(value) = HeaderValue::from_str(model) {
        response.headers_mut().insert("x-gateway-model", value);
    }
}

/// [`RouterStamp`] with the model id owned.
///
/// The committed streaming chain (`chain_stream`) takes its inputs by value and
/// awaits, so it cannot hold the borrow the synchronous paths use.
pub(super) struct OwnedRouterStamp {
    pub(super) routed_model: String,
    pub(super) source: &'static str,
}

/// Stamp only the router pair, for the committed streaming chain.
///
/// Both values come from the router decision, which is made before any upstream
/// is contacted — so unlike `x-gateway-upstream` and `x-gateway-upstream-model`,
/// they do not depend on which attempt wins the race and are correct on this
/// path too.
pub(super) fn stamp_router_headers(
    response: &mut axum::response::Response,
    stamp: &OwnedRouterStamp,
) {
    for (name, value) in [
        ("x-gateway-routed-model", stamp.routed_model.as_str()),
        ("x-gateway-route-source", stamp.source),
    ] {
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    }
}

/// What a router decided, for the two headers that report it.
///
/// Absent for every request whose model id carries no `[models.router]` table,
/// which is why both headers are omitted rather than sent empty: a client
/// cannot otherwise tell "routed to the efficient tier" from "not routed by a
/// router at all".
#[derive(Clone, Copy)]
pub(crate) struct RouterStamp<'a> {
    /// The configured model id the chosen tier routes to. Distinct from
    /// `x-gateway-upstream-model`, which is the name sent upstream — for a
    /// router these differ whenever the target maps its own `upstream_model`.
    pub(crate) routed_model: &'a str,
    pub(crate) source: &'static str,
}

fn stamp_gateway_headers(
    response: &mut axum::response::Response,
    upstream: &str,
    model: &str,
    upstream_model: &str,
    stage: Option<RouterStamp<'_>>,
) {
    for (name, value) in [
        ("x-gateway-upstream", upstream),
        ("x-gateway-model", model),
        ("x-gateway-upstream-model", upstream_model),
    ]
    .into_iter()
    .chain(stage.into_iter().flat_map(|stage| {
        [
            ("x-gateway-routed-model", stage.routed_model),
            ("x-gateway-route-source", stage.source),
        ]
    })) {
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests;
