//! Routed inbound Codex request to a **non-ChatGPT** Responses upstream
//! (`[[server.codex_endpoint.routes]]`, issue #436).
//!
//! The sibling [`super::inbound`] path exists to be byte-faithful to the
//! ChatGPT/Codex backend: it relays the Codex CLI's own headers verbatim so the
//! CLI's real client identity reaches the one upstream that gates on it. That is
//! exactly the wrong shape for a third party. A GLM/DeepSeek/OpenRouter endpoint
//! has no use for `originator`, `session-id`, or `x-codex-*`, and forwarding
//! them leaks the operator's client telemetry — and, worse, an inbound
//! `authorization` or `x-api-key` would leak a caller's own secret to a host it
//! was never issued for.
//!
//! So this path inverts the rule: a **fresh allowlist** rather than a strip
//! list. Only `content-type` and `accept` survive, plus the credential shunt
//! resolves for the routed provider. Everything else is dropped by construction,
//! so a header added upstream (by the CLI, or by a future shunt slot) cannot
//! silently start reaching a third party because nobody remembered to add it to
//! a denylist.
//!
//! There is no pool and no failover here: one credential, one attempt, and the
//! upstream response — 200, 429, or 5xx — relayed verbatim with its own
//! `retry-after`, so the Codex CLI backs off against the third party's real
//! signal instead of a rotation shunt invented.

use axum::{
    body::Bytes,
    http::{
        header::{ACCEPT, CONTENT_TYPE},
        HeaderMap, HeaderValue, StatusCode,
    },
};

use crate::{
    adapters::AdapterError, auth::resolve_credential, auth::slots::ShuntCredentials,
    routing::Route, server::AppState,
};

use super::{
    inbound::{apply_credential, relay_passthrough, send_error},
    request::responses_url,
};

/// Serve one inbound Responses request over a routed, non-ChatGPT provider.
///
/// `body` is already identity-encoded by the caller
/// (`codex_endpoint::dispatch_routed`) with its `model` rewritten to the route's
/// `upstream_model`, because a stock Responses API accepts neither the Codex
/// CLI's zstd request encoding nor a model id it does not serve.
pub(crate) async fn forward_codex_routed(
    state: AppState,
    route: Route,
    client_headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let headers = routed_request_headers(&state, &route, &client_headers);

    let request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .headers(headers);
    let upstream = crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        apply_credential(request, credential).body(body).send(),
    )
    .await
    .map_err(send_error)?;

    let status = upstream.status();
    Ok((status, relay_passthrough(upstream)))
}

/// Build the upstream header set for a routed request as an **allowlist**: only
/// `content-type` (defaulted, since the body is always JSON) and `accept` come
/// from the client.
///
/// Nothing else does — not `authorization` or `x-api-key` (a caller's own
/// secret, and never a credential for this host), not `chatgpt-account-id` /
/// `originator` / `version` / `user-agent` / `session-id` / `session_id` /
/// `thread-id` / `x-codex-*` / `openai-beta` (Codex-CLI identity and telemetry a
/// third party has no business seeing), not `x-shunt-*` (shunt's own reserved
/// slots), not `content-encoding` or `accept-encoding` (the body is identity and
/// the reply must stay unbuffered for `relay_passthrough` to stream it), and no
/// hop-by-hop header.
///
/// `OpenAI-Beta: responses=experimental` is added under the same flavor gate
/// `request::request_builder` uses, so an xAI/Grok-flavored route does not
/// receive an OpenAI-only header.
///
/// [`ShuntCredentials::strip_reserved_slots`] runs last even though an allowlist
/// makes it a no-op today. This is deliberately *not* a forward site in the
/// `auth::slots` enumeration — nothing caller-supplied that could carry a
/// credential is a candidate here — but the strip states the invariant in code
/// rather than in a comment a future allowlist entry could quietly invalidate.
fn routed_request_headers(state: &AppState, route: &Route, client: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    out.insert(
        CONTENT_TYPE,
        client
            .get(CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("application/json")),
    );
    if let Some(accept) = client.get(ACCEPT) {
        out.insert(ACCEPT, accept.clone());
    }
    // `OpenAI-Beta: responses=experimental` is an OpenAI/ChatGPT header; xAI's
    // Responses API doesn't expect it and the reference clients don't send it.
    if !matches!(
        state.config.responses_flavor(&route.provider),
        crate::config::ResponsesFlavor::Xai | crate::config::ResponsesFlavor::Grok
    ) {
        out.insert(
            "openai-beta",
            HeaderValue::from_static("responses=experimental"),
        );
    }
    ShuntCredentials::from_state(state).strip_reserved_slots(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use crate::{config::Config, routing::AdapterKind};

    use super::*;

    fn route(provider: &str) -> Route {
        Route {
            provider: provider.to_string(),
            adapter: AdapterKind::Responses,
            model: "glm-5.3".to_string(),
            upstream_model: "glm-5.3".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    fn state() -> AppState {
        AppState::new(Config::default(), reqwest::Client::new()).unwrap()
    }

    #[test]
    fn forwards_only_content_type_and_accept_from_the_client() {
        let mut client = HeaderMap::new();
        client.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        client.insert(ACCEPT, "text/event-stream".parse().unwrap());
        for leaked in [
            "authorization",
            "x-api-key",
            "chatgpt-account-id",
            "originator",
            "version",
            "user-agent",
            "session-id",
            "session_id",
            "thread-id",
            "x-codex-window-id",
            "x-shunt-token",
            "content-encoding",
            "accept-encoding",
            "connection",
        ] {
            client.insert(leaked, "leaked".parse().unwrap());
        }

        let headers = routed_request_headers(&state(), &route("openai"), &client);

        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get(ACCEPT).unwrap(), "text/event-stream");
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        for leaked in headers.keys() {
            assert!(
                matches!(leaked.as_str(), "content-type" | "accept" | "openai-beta"),
                "unexpected header forwarded to a third party: {leaked}"
            );
        }
    }

    #[test]
    fn defaults_the_content_type_when_the_client_sends_none() {
        let headers = routed_request_headers(&state(), &route("openai"), &HeaderMap::new());
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert!(headers.get(ACCEPT).is_none());
    }

    #[test]
    fn omits_the_openai_beta_header_for_an_xai_flavored_route() {
        let mut config = Config::default();
        config.providers.get_mut("openai").unwrap().base_url = "https://api.x.ai/v1".to_string();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let headers = routed_request_headers(&state, &route("openai"), &HeaderMap::new());
        assert!(headers.get("openai-beta").is_none());
    }
}
