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
//! resolves for the routed provider and whatever client identity *that specific
//! upstream* gates on — `OpenAI-Beta` for an OpenAI-flavored host, and the
//! Grok-CLI identity headers for an `xai_oauth` one, which the subscription
//! chat proxy rejects the request without. Everything else is dropped by
//! construction, so a header added upstream (by the CLI, or by a future shunt
//! slot) cannot silently start reaching a third party because nobody remembered
//! to add it to a denylist.
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
    adapters::AdapterError,
    auth::{resolve_credential, slots::ShuntCredentials, Credential},
    routing::Route,
    server::AppState,
};

use super::{
    inbound::{apply_credential, relay_passthrough, send_error},
    request::{grok_identity_headers, responses_url},
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
    let request = routed_request(&state, &route, credential, &client_headers);
    let upstream = crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        request.body(body).send(),
    )
    .await
    .map_err(send_error)?;

    let status = upstream.status();
    Ok((status, relay_passthrough(upstream)))
}

/// Compose the routed upstream request: the allowlisted header set, the
/// resolved credential, and whatever client identity the routed upstream itself
/// gates on.
///
/// Only the `XaiOauth` case adds identity. The Grok subscription proxy answers
/// a bare bearer as if the caller were an unentitled API client, so a route to
/// an `xai_oauth` provider must carry the same four Grok-CLI headers the
/// outbound `request::request_builder` sends — hence the shared
/// [`grok_identity_headers`]. [`apply_credential`] is deliberately *not*
/// widened to do this: its `XaiOauth` arm belongs to the pool path, where it is
/// an unreachable defensive fallback that must stay bearer-only.
///
/// Split out of [`forward_codex_routed`] so the composed request can be
/// inspected in a unit test — config validation pins an `xai_oauth` provider to
/// an https grok/xai host, so this shape is not reachable from a wiremock
/// integration test.
fn routed_request(
    state: &AppState,
    route: &Route,
    credential: Credential,
    client_headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut headers = routed_request_headers(state, route, client_headers);
    let grok = matches!(credential, Credential::XaiOauth { .. });
    if grok {
        // `grok_identity_headers` pins `accept: text/event-stream`, and
        // `RequestBuilder::header` *appends* rather than replaces — so drop the
        // client's `accept` first rather than sending the field twice.
        headers.remove(ACCEPT);
    }
    let request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .headers(headers);
    let request = if grok {
        grok_identity_headers(request)
    } else {
        request
    };
    apply_credential(request, credential)
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

    fn built(request: reqwest::RequestBuilder) -> reqwest::Request {
        request.body("{}").build().expect("request should build")
    }

    #[test]
    fn an_xai_oauth_route_carries_the_grok_cli_identity() {
        // The Grok subscription proxy gates on the CLI identity, not just the
        // bearer: a routed request without these four headers is answered as if
        // the caller were an unentitled API client.
        // The realistic shape of such a route: an `xai_oauth` provider on the
        // Grok subscription host, which is also what makes the flavor `Grok`.
        let mut config = Config::default();
        let provider = config.providers.get_mut("openai").unwrap();
        provider.base_url = "https://cli-chat-proxy.grok.com/v1".to_string();
        provider.auth = crate::config::AuthMode::XaiOauth;
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut client = HeaderMap::new();
        client.insert(ACCEPT, "application/json".parse().unwrap());

        let request = built(routed_request(
            &state,
            &route("openai"),
            Credential::XaiOauth {
                access_token: "grok-token".to_string(),
            },
            &client,
        ));

        let headers = request.headers();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer grok-token");
        assert_eq!(headers.get("x-xai-token-auth").unwrap(), "xai-grok-cli");
        assert!(headers.get("x-grok-client-identifier").is_some());
        assert!(headers.get("x-grok-client-version").is_some());
        // The Grok identity pins `accept`, and `RequestBuilder::header` appends —
        // so the client's own `accept` must have been dropped, not doubled up.
        assert_eq!(
            headers.get_all(ACCEPT).iter().collect::<Vec<_>>(),
            vec!["text/event-stream"]
        );
        // Grok-flavored, so the OpenAI-only beta header stays off.
        assert!(headers.get("openai-beta").is_none());
    }

    #[test]
    fn an_api_key_route_carries_no_grok_identity() {
        // The positive twin: the Grok headers are credential-gated, not sent to
        // every routed upstream.
        let request = built(routed_request(
            &state(),
            &route("openai"),
            Credential::ApiKey {
                value: "third-party-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            &HeaderMap::new(),
        ));

        let headers = request.headers();
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer third-party-key"
        );
        for grok in [
            "x-xai-token-auth",
            "x-grok-client-identifier",
            "x-grok-client-version",
        ] {
            assert!(headers.get(grok).is_none(), "{grok} must not be sent");
        }
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
