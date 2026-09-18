use std::time::Instant;

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    accounts::StoreFamily,
    adapters::{AdapterFailure, AdapterResult},
    auth,
    config::{AccountConfig, AuthMode, Config},
    error::ShuntError,
    request::RequestBody,
    routing::{self, Route},
    server::AppState,
};

use super::{failover, ForwardError};

pub(super) struct Policy {
    original: Route,
    destination: Route,
    claude_fallback: Option<Route>,
}

pub(super) struct Request<'a> {
    pub state: AppState,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    pub inbound: &'a failover::InboundContext,
    pub body: RequestBody,
    pub requested_model: &'a str,
    pub started_at: Instant,
    /// The stage-router decision for this request, threaded through so the
    /// final attempt still reports `x-gateway-routed-model` and
    /// `x-gateway-route-source` exactly like the ordinary failover path.
    pub stage_stamp: Option<failover::StageStamp<'a>>,
}

impl Policy {
    pub(super) fn select(config: &Config, routes: &[Route]) -> Option<Self> {
        let [original] = routes else {
            return None;
        };
        let policy = config.server.weekly_fallback.as_ref()?;
        if !policy.enabled {
            return None;
        }
        let pair = policy.models.iter().find(|pair| {
            (original.provider == policy.claude_provider && original.upstream_model == pair.claude)
                || (original.provider == policy.codex_provider
                    && original.upstream_model == pair.codex)
        })?;
        let from_claude = original.provider == policy.claude_provider;
        let (provider, backend) = if from_claude {
            (&policy.codex_provider, &pair.codex)
        } else {
            (&policy.claude_provider, &pair.claude)
        };
        let destination =
            routing::route_for(config, provider, &original.model, backend, None, None);
        let claude = if from_claude { original } else { &destination };
        let claude_fallback = pair.claude_fallback.as_ref().map(|backend| Route {
            upstream_model: backend.clone(),
            ..claude.clone()
        });
        Some(Self {
            original: original.clone(),
            destination,
            claude_fallback,
        })
    }

    pub(super) fn auth_routes(&self) -> Vec<Route> {
        let mut routes = vec![self.original.clone(), self.destination.clone()];
        routes.extend(self.claude_fallback.iter().cloned());
        routes
    }

    pub(super) async fn forward(
        self,
        request: Request<'_>,
    ) -> Result<(StatusCode, axum::response::Response), ForwardError> {
        let original_accounts = resolve_accounts(&request.state, &self.original.provider).await;
        let mut destination_accounts = None;
        let mut route = self.original.clone();
        let mut switched = false;
        let mut fallback_used = false;

        if exhausted(&request.state, &self.original, &original_accounts) {
            destination_accounts =
                resolve_accounts(&request.state, &self.destination.provider).await;
            if exhausted(&request.state, &self.original, &original_accounts) {
                if exhausted(&request.state, &self.destination, &destination_accounts) {
                    return request.finish(&route, exhausted_response());
                }
                route = self.destination.clone();
                switched = true;
            }
        }

        loop {
            crate::metrics::record_failover(&route.provider, "attempted");
            let headers = failover::headers_for_route(
                &request.state,
                &route,
                request.headers,
                request.inbound,
                !switched,
                None,
            );
            let started_at = Instant::now();
            let result = failover::dispatch(
                request.state.clone(),
                route.clone(),
                request.uri,
                &headers,
                request.body.clone(),
            )
            .await;
            crate::metrics::record_proxied_request(
                &route.provider,
                &route.model,
                result_status(&result).as_u16(),
                started_at.elapsed().as_secs_f64() * 1000.0,
            );
            if !eligible_failure(&result) {
                return request.finish(&route, result);
            }

            let accounts = if switched {
                &destination_accounts
            } else {
                &original_accounts
            };
            let mut current_exhausted = exhausted(&request.state, &route, accounts);
            if current_exhausted && !switched {
                destination_accounts =
                    resolve_accounts(&request.state, &self.destination.provider).await;
                current_exhausted = exhausted(&request.state, &self.original, &original_accounts);
            }
            if current_exhausted {
                if exhausted(&request.state, &self.original, &original_accounts)
                    && exhausted(&request.state, &self.destination, &destination_accounts)
                {
                    return request.finish(&route, exhausted_response());
                }
                if !switched {
                    crate::metrics::record_failover(&route.provider, "advanced");
                    drop(result);
                    route = self.destination.clone();
                    switched = true;
                    continue;
                }
            } else if !fallback_used {
                if let Some(fallback) = &self.claude_fallback {
                    if fallback.provider == route.provider {
                        crate::metrics::record_failover(&route.provider, "advanced");
                        drop(result);
                        route = fallback.clone();
                        fallback_used = true;
                        continue;
                    }
                }
            }
            return request.finish(&route, result);
        }
    }
}

async fn resolve_accounts(state: &AppState, provider: &str) -> Option<Vec<AccountConfig>> {
    let config = state.config.provider(provider)?;
    let result = match config.auth {
        AuthMode::ClaudeOauth => {
            auth::shared::resolve_pool_accounts(
                "Claude",
                &config.accounts,
                &config.account_scope,
                StoreFamily::Claude,
                auth::claude::store::default_accounts_dir(),
                auth::claude::store::scan_accounts,
            )
            .await
        }
        AuthMode::ChatgptOauth => {
            auth::shared::resolve_pool_accounts(
                "codex",
                &config.accounts,
                &config.account_scope,
                StoreFamily::Chatgpt,
                auth::codex::store::default_accounts_dir(),
                auth::codex::store::scan_accounts,
            )
            .await
        }
        _ => return None,
    };
    // The adapter reports resolver errors through its existing error path.
    result.ok()
}

fn exhausted(state: &AppState, route: &Route, accounts: &Option<Vec<AccountConfig>>) -> bool {
    accounts.as_ref().is_some_and(|accounts| {
        state
            .accounts
            .strict_weekly_exhausted(&route.provider, accounts)
    })
}

fn eligible_failure(result: &AdapterResult) -> bool {
    match result {
        Ok((status, _)) => status.is_client_error() || status.is_server_error(),
        Err(error) => match error.failure {
            Some(AdapterFailure::BeforeHeaders) => true,
            Some(AdapterFailure::UpstreamStatus(status)) => {
                status.is_client_error() || status.is_server_error()
            }
            Some(AdapterFailure::NoUpstreamAttempt) | None => false,
        },
    }
}

fn result_status(result: &AdapterResult) -> StatusCode {
    match result {
        Ok((status, _)) => *status,
        Err(error) => error.response.status(),
    }
}

fn exhausted_response() -> AdapterResult {
    let status = StatusCode::TOO_MANY_REQUESTS;
    Ok((
        status,
        ShuntError::new(
            status,
            "rate_limit_error",
            "both provider pools have exhausted their shared weekly quota",
        )
        .into_response(),
    ))
}

impl Request<'_> {
    fn finish(
        &self,
        route: &Route,
        result: AdapterResult,
    ) -> Result<(StatusCode, axum::response::Response), ForwardError> {
        let status = result_status(&result);
        crate::observability::record_span_outcome(&route.provider, status);
        crate::observability::capture_upstream_outcome(
            &route.provider,
            self.requested_model,
            status,
        );
        match result {
            Ok((status, mut response)) => {
                failover::stamp_gateway_headers(
                    &mut response,
                    &route.provider,
                    self.requested_model,
                    &route.upstream_model,
                    self.stage_stamp,
                );
                Ok(failover::observe_response(
                    status,
                    response,
                    route.provider.clone(),
                    route.model.clone(),
                    self.started_at,
                ))
            }
            Err(mut error) => {
                failover::stamp_gateway_headers(
                    &mut error.response,
                    &route.provider,
                    self.requested_model,
                    &route.upstream_model,
                    self.stage_stamp,
                );
                Err(ForwardError {
                    message: error.message,
                    response: error.response,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use serde_json::json;

    use super::*;
    use crate::adapters::AdapterError;
    use crate::config::{ModelConfig, RouteConfig, RoutePrefixConfig};

    fn config() -> Config {
        let mut config = Config::default();
        config.server.weekly_fallback = Some(serde_json::from_value(json!({
            "enabled": true, "claude_provider": "anthropic", "codex_provider": "codex",
            "models": [{"claude": "claude-fable-5", "codex": "gpt-6-astra", "claude_fallback": "claude-opus-5"}]
        })).unwrap());
        config
    }

    #[test]
    fn outcome_classification_requires_upstream_failure_provenance() {
        for status in [200, 302, 400, 401, 403, 404, 429, 500, 529] {
            let status = StatusCode::from_u16(status).unwrap();
            let eligible = status.is_client_error() || status.is_server_error();
            let response = || {
                axum::response::Response::builder()
                    .status(status)
                    .body(Body::empty())
                    .unwrap()
            };
            assert_eq!(eligible_failure(&Ok((status, response()))), eligible);
            for (failure, expected) in [
                (None, false),
                (Some(AdapterFailure::NoUpstreamAttempt), false),
                (Some(AdapterFailure::BeforeHeaders), true),
                (Some(AdapterFailure::UpstreamStatus(status)), eligible),
            ] {
                let result = Err(AdapterError {
                    message: "test".into(),
                    response: Box::new(response()),
                    failure,
                });
                assert_eq!(eligible_failure(&result), expected);
            }
        }
    }

    #[test]
    fn policy_matches_normalized_backend_and_provider_across_single_route_sources() {
        let mut config = config();
        config.models.push(ModelConfig {
            id: "model-alias".into(),
            display_name: None,
            upstream_model: Some([("anthropic".into(), "claude-fable-5".into())].into()),
            stage_router: None,
        });
        config.routes.push(RouteConfig {
            model: "route-alias".into(),
            provider: "codex".into(),
            upstream_model: Some("gpt-6-astra".into()),
            effort: None,
            service_tier: None,
        });
        config.route_prefixes.push(RoutePrefixConfig {
            prefix: "gpt-".into(),
            provider: "codex".into(),
        });
        for model in [
            "model-alias[1M]",
            "route-alias[1m]",
            "gpt-6-astra[1M]",
            "claude-fable-5[1m]",
        ] {
            let routes = routing::resolve_model_chain(&config, model);
            assert!(Policy::select(&config, &routes).is_some(), "model: {model}");
        }
        let route = routing::resolve_model(&config, "claude-fable-5");
        assert!(Policy::select(&config, &[route.clone(), route.clone()]).is_none());
        assert!(Policy::select(
            &config,
            &[Route {
                provider: "unrelated".into(),
                ..route.clone()
            }]
        )
        .is_none());
        assert!(Policy::select(
            &config,
            &[Route {
                upstream_model: "claude-fable-5-versioned".into(),
                ..route.clone()
            }]
        )
        .is_none());
        config.server.weekly_fallback.as_mut().unwrap().enabled = false;
        assert!(Policy::select(&config, std::slice::from_ref(&route)).is_none());
        config.server.weekly_fallback = None;
        assert!(Policy::select(&config, &[route]).is_none());
    }

    #[test]
    fn same_provider_fallback_keeps_route_options_and_switch_uses_destination_defaults() {
        let mut config = config();
        let codex = config.providers.get_mut("codex").unwrap();
        codex.effort = Some("high".into());
        codex.service_tier = Some("flex".into());
        let mut original = routing::resolve_model(&config, "claude-fable-5");
        original.effort = Some("low".into());
        original.service_tier = Some("priority".into());
        let policy = Policy::select(&config, std::slice::from_ref(&original)).unwrap();
        let fallback = policy.claude_fallback.as_ref().unwrap();
        assert_eq!(fallback.effort, original.effort);
        assert_eq!(fallback.service_tier, original.service_tier);
        assert_eq!(fallback.upstream_model, "claude-opus-5");
        assert_eq!(policy.destination.effort.as_deref(), Some("high"));
        assert_eq!(policy.destination.service_tier.as_deref(), Some("flex"));
        assert_eq!(policy.auth_routes().len(), 3);
    }

    /// The weekly policy is selected instead of the ordinary failover loop, so
    /// it must reproduce that loop's stage-router headers. `finish` threads the
    /// request's already-computed stamp into `stamp_gateway_headers`; dropping
    /// it (passing `None`) silently loses `x-gateway-routed-model` and
    /// `x-gateway-route-source` for every weekly-policy response.
    #[tokio::test]
    async fn finish_stamps_the_stage_router_headers_when_a_stamp_is_present() {
        let config = config();
        // `finish` never touches `state`; a default state avoids re-validating
        // the test's enabled policy against the default passthrough providers.
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let route = routing::resolve_model(&config, "claude-fable-5");
        let uri: Uri = "/v1/messages".parse().unwrap();
        let headers = HeaderMap::new();
        let inbound = failover::InboundContext::for_test();
        let request = Request {
            state,
            uri: &uri,
            headers: &headers,
            inbound: &inbound,
            body: RequestBody::parse(b"{}".to_vec()).unwrap(),
            requested_model: "claude-fable-5",
            started_at: Instant::now(),
            stage_stamp: Some(failover::StageStamp::for_test("claude-auto", "test-source")),
        };
        let result = Ok((
            StatusCode::OK,
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .body(axum::body::Body::empty())
                .unwrap(),
        ));
        let (_, response) = match request.finish(&route, result) {
            Ok(ok) => ok,
            Err(_) => panic!("finish on a successful attempt must return the response"),
        };
        assert_eq!(response.headers()["x-gateway-routed-model"], "claude-auto");
        assert_eq!(response.headers()["x-gateway-route-source"], "test-source");
    }
}
