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
//! was never issued for. Stock OpenAI is the narrow exception: shunt generates
//! its session-affinity headers from the already-resolved conversation id.
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
//! There is no failover here: one credential, one attempt, and the upstream
//! response — 200, 429, or 5xx — relayed verbatim with its own `retry-after`, so
//! the Codex CLI backs off against the third party's real signal instead of a
//! rotation shunt invented. An `xai_oauth` route with named accounts takes that
//! one credential from the first account in pool selection order; it never
//! rotates or cools an account down.

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
    config::AuthMode,
    routing::Route,
    server::AppState,
};

use super::{
    error::own_error,
    inbound::{apply_credential, relay_passthrough, send_error},
    request::{grok_identity_headers, responses_url, session_affinity_headers},
};

/// Serve one inbound Responses request over a routed, non-ChatGPT provider.
///
/// `body` is already identity-encoded by the caller
/// (`codex_endpoint::dispatch_routed`) with its `model` rewritten to the route's
/// `upstream_model`, because a stock Responses API accepts neither the Codex
/// CLI's zstd request encoding nor a model id it does not serve.
///
/// `pool_key` is the session-sticky account-selection key, namespaced with the
/// authenticated inbound client (`codex_endpoint::pool_sticky_key`) so one
/// client cannot steer another's session onto an account; `session_id` stays
/// the raw id for the upstream's own affinity headers.
pub(crate) async fn forward_codex_routed(
    state: AppState,
    route: Route,
    client_headers: HeaderMap,
    body: Bytes,
    pool_key: Option<String>,
    session_id: Option<String>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let credential = match routed_xai_pool_credential(&state, &route, pool_key.as_deref()).await? {
        Some(credential) => credential,
        None => resolve_credential(&state.config, &route, &state.http_client).await?,
    };
    let request = routed_request(
        &state,
        &route,
        credential,
        &client_headers,
        session_id.as_deref(),
    );
    let upstream = crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        request.body(body).send(),
    )
    .await
    .map_err(send_error)?;

    let status = upstream.status();
    Ok((status, relay_passthrough(upstream)))
}

/// The credential for a route to an `xai_oauth` provider whose account pool is
/// non-empty: the first account in pool selection order whose credential
/// loads. The order honors the provider's configured `account`/`accounts`
/// scope, `priority`, `disabled`, and the admin pause; an account whose
/// credential fails to load is cooled down and skipped, as on the outbound
/// pool. `None` for any other provider, or for an empty xAI pool, where
/// `resolve_credential`'s single-account file applies — the same rule as the
/// outbound pool (docs/m18-xai-multi-account.md), mirroring
/// `adapters::gemini::forward`'s empty-pool fallback. One send: a request that
/// fails upstream is not retried on another account. When `max_utilization`
/// caps exclude every account, the pool's cap-exhaustion 429 is returned, as on
/// the ChatGPT inbound path.
async fn routed_xai_pool_credential(
    state: &AppState,
    route: &Route,
    pool_key: Option<&str>,
) -> Result<Option<Credential>, AdapterError> {
    let Some(provider) = state.config.provider(&route.provider) else {
        return Ok(None);
    };
    if provider.auth != AuthMode::XaiOauth {
        return Ok(None);
    }
    let accounts = provider.resolve_pool_accounts().await.map_err(own_error)?;
    if accounts.is_empty() {
        return Ok(None);
    }
    let (order, cap) = state.accounts.select_order_with_cap(
        &route.provider,
        &accounts,
        pool_key,
        Some(route.upstream_model.as_str()),
        state.config.server.pool.as_ref(),
    );
    if order.is_empty() {
        if let Some(error) = crate::adapters::cap_exhausted(route, cap) {
            return Err(error);
        }
    }
    // An empty order relays this; otherwise the last account's own resolution
    // error, which names the cause (a missing file, an unset `token_env`).
    let mut last_error = own_error(format!(
        "provider '{}' has {} account(s) but none are selectable",
        route.provider,
        accounts.len()
    ));
    for index in order {
        match super::pool::resolve_xai_or_cooldown(state, route, &accounts[index]).await {
            Ok(credential) => return Ok(Some(credential)),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Compose the routed upstream request: the allowlisted header set, the
/// resolved credential, and whatever client identity the routed upstream itself
/// gates on.
///
/// The Grok subscription proxy answers a bare bearer as if the caller were an
/// unentitled API client, so a route to an `xai_oauth` provider must carry the
/// same four Grok-CLI headers the outbound `request::request_builder` sends —
/// hence the shared [`grok_identity_headers`]. An api-key route to the stock
/// OpenAI host instead adds the generated session-affinity headers, which that
/// host derives prompt-cache affinity from. [`apply_credential`] is deliberately
/// *not* widened to do either: its `XaiOauth` arm belongs to the pool path,
/// where it is an unreachable defensive fallback that must stay bearer-only.
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
    session_id: Option<&str>,
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
    let api_key = matches!(credential, Credential::ApiKey { .. });
    let request = apply_credential(request, credential);
    // Stock OpenAI is the one api-key routed upstream that derives prompt-cache
    // affinity from the codex session headers, so generate them here (the
    // allowlist never forwards them verbatim) and nowhere else — with the
    // client's own thread-derived ids passed through where the CLI sent them,
    // since those are identities shunt cannot recompute.
    if api_key && state.config.is_openai_backend(&route.provider) {
        session_affinity_headers(request, session_id, None, 0, Some(client_headers))
    } else {
        request
    }
}

/// Build the upstream header set for a routed request as an **allowlist**: only
/// `content-type` (defaulted, since the body is always JSON) and `accept` come
/// from the client.
///
/// Nothing else does — not `authorization` or `x-api-key` (a caller's own
/// secret, and never a credential for this host), not `chatgpt-account-id` /
/// `originator` / `version` / `user-agent` / `session-id` / `session_id` /
/// `thread-id` / `x-codex-*` / `openai-beta` (the caller's Codex-CLI identity
/// and telemetry), not `x-shunt-*` (shunt's own reserved
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
#[allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
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
            "x-client-request-id",
            "x-codex-window-id",
            "openai-beta",
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
            None,
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

    /// A route's config with the `openai` provider re-pointed at the Grok
    /// subscription host as an `xai_oauth` provider, the shape the routed xAI
    /// selection needs.
    fn xai_config() -> Config {
        let mut config = Config::default();
        let provider = config.providers.get_mut("openai").unwrap();
        provider.base_url = "https://cli-chat-proxy.grok.com/v1".to_string();
        provider.auth = crate::config::AuthMode::XaiOauth;
        config
    }

    /// A far-future JWT-shaped access token, as T6's `xai_token` builds: the
    /// xAI store reads `exp` from the payload.
    fn xai_token(tag: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let payload = serde_json::json!({ "exp": 4_000_000_000u64, "sub": tag });
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    fn fresh_accounts_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-routed-xai-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn an_xai_route_with_named_accounts_uses_the_first_selected_account() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("first");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);
        let (token_a, token_b) = (xai_token("a"), xai_token("b"));
        crate::auth::xai::store::store_oauth_tokens("a", &token_a, "refresh", None).unwrap();
        crate::auth::xai::store::store_oauth_tokens("b", &token_b, "refresh", None).unwrap();

        let state = AppState::new(xai_config(), reqwest::Client::new()).unwrap();
        let Ok(Some(Credential::XaiOauth { access_token })) =
            routed_xai_pool_credential(&state, &route("openai"), None).await
        else {
            panic!("expected the first xAI pool account");
        };
        assert_eq!(access_token, token_a);

        let mut config = xai_config();
        config.providers.get_mut("openai").unwrap().account_scope = vec!["b".into()];
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let Ok(Some(Credential::XaiOauth { access_token })) =
            routed_xai_pool_credential(&state, &route("openai"), None).await
        else {
            panic!("expected the scoped xAI pool account");
        };
        assert_eq!(access_token, token_b);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// One broken login must not blackhole the route: an account whose
    /// credential cannot be loaded is cooled down and the next account serves.
    #[tokio::test]
    async fn an_xai_route_skips_an_account_whose_login_cannot_be_loaded() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("skip");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);
        let token_b = xai_token("b");
        crate::auth::xai::store::store_oauth_tokens("a", &xai_token("a"), "refresh", None).unwrap();
        crate::auth::xai::store::store_oauth_tokens("b", &token_b, "refresh", None).unwrap();
        // `a` still scans as an account, but its file no longer loads.
        std::fs::write(dir.join("a.json"), "not json").unwrap();

        let state = AppState::new(xai_config(), reqwest::Client::new()).unwrap();
        let Ok(Some(Credential::XaiOauth { access_token })) =
            routed_xai_pool_credential(&state, &route("openai"), None).await
        else {
            panic!("expected the next xAI pool account");
        };
        assert_eq!(access_token, token_b);

        // `a` is cooled down, so selection no longer leads with it; a cooling
        // account stays in the order only as a last resort.
        let accounts = state
            .config
            .provider("openai")
            .unwrap()
            .resolve_pool_accounts()
            .await
            .unwrap();
        let (order, _) = state
            .accounts
            .select_order_with_cap("openai", &accounts, None, None, None);
        let names: Vec<&str> = order
            .iter()
            .map(|&index| accounts[index].name.as_str())
            .collect();
        assert_eq!(names, ["b", "a"]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_xai_route_with_an_empty_store_falls_back_to_the_single_account_file() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("empty");
        // The directory is never created, so the store is empty.
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let state = AppState::new(xai_config(), reqwest::Client::new()).unwrap();
        let credential = routed_xai_pool_credential(&state, &route("openai"), None).await;
        assert!(matches!(credential, Ok(None)));
    }

    #[tokio::test]
    async fn a_non_xai_route_never_consults_the_xai_store() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("non-xai");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);
        crate::auth::xai::store::store_oauth_tokens("a", &xai_token("a"), "refresh", None).unwrap();

        // The default `openai` provider authenticates with an api key.
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let credential = routed_xai_pool_credential(&state, &route("openai"), None).await;
        assert!(matches!(credential, Ok(None)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_xai_route_whose_accounts_are_all_disabled_is_a_gateway_error() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("disabled");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let mut config = xai_config();
        config.providers.get_mut("openai").unwrap().accounts = vec![crate::config::AccountConfig {
            name: "off".into(),
            disabled: true,
            ..Default::default()
        }];
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let Err(error) = routed_xai_pool_credential(&state, &route("openai"), None).await else {
            panic!("expected a gateway error for an unselectable pool");
        };
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
    }

    /// Stickiness keys on the client-namespaced pool key, not the raw session
    /// id: two clients sending the same session id can land on different
    /// accounts, so neither can steer the other's session onto an account.
    #[tokio::test]
    async fn an_xai_route_selects_by_the_client_namespaced_pool_key() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("pool-key");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);
        let (token_a, token_b) = (xai_token("a"), xai_token("b"));
        crate::auth::xai::store::store_oauth_tokens("a", &token_a, "refresh", None).unwrap();
        crate::auth::xai::store::store_oauth_tokens("b", &token_b, "refresh", None).unwrap();

        let state = AppState::new(xai_config(), reqwest::Client::new()).unwrap();
        let route = route("openai");
        let accounts = state
            .config
            .provider("openai")
            .unwrap()
            .resolve_pool_accounts()
            .await
            .unwrap();
        let first = |key: &str| {
            let (order, _) = state.accounts.select_order_with_cap(
                "openai",
                &accounts,
                Some(key),
                Some(route.upstream_model.as_str()),
                None,
            );
            accounts[order[0]].name.clone()
        };
        let session = (0..64)
            .map(|index| format!("sess-{index}"))
            .find(|session| {
                first(&format!("client-x:{session}")) != first(&format!("client-y:{session}"))
            })
            .expect("some session id hashes the two clients onto different accounts");

        for client in ["client-x", "client-y"] {
            let key = format!("{client}:{session}");
            let Ok(Some(Credential::XaiOauth { access_token })) =
                routed_xai_pool_credential(&state, &route, Some(&key)).await
            else {
                panic!("expected an xAI pool account for {key}");
            };
            let expected = if first(&key) == "a" {
                &token_a
            } else {
                &token_b
            };
            assert_eq!(&access_token, expected, "{key}");
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    /// When `max_utilization` caps hold every account out, the route answers
    /// with the pool's cap-exhaustion 429 (and its `retry-after`), not the
    /// generic "none are selectable" 502.
    #[tokio::test]
    async fn an_xai_route_whose_accounts_are_all_capped_returns_the_cap_429() {
        let _guard = crate::config::test_env_lock();
        let dir = fresh_accounts_dir("capped");
        let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let mut config = xai_config();
        config.providers.get_mut("openai").unwrap().accounts = ["a", "b"]
            .into_iter()
            .map(|name| crate::config::AccountConfig {
                name: name.into(),
                token_env: Some("SHUNT_TEST_ROUTED_XAI_CAPPED_UNSET".into()),
                max_utilization_5h: Some(0.5),
                ..Default::default()
            })
            .collect();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let accounts = state
            .config
            .provider("openai")
            .unwrap()
            .resolve_pool_accounts()
            .await
            .unwrap();
        let reset = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        let mut quota = HeaderMap::new();
        quota.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.9"),
        );
        quota.insert(
            "anthropic-ratelimit-unified-5h-reset",
            HeaderValue::from(reset),
        );
        for account in &accounts {
            state.accounts.note_quota("openai", account, &quota);
        }

        let Err(error) = routed_xai_pool_credential(&state, &route("openai"), None).await else {
            panic!("expected the cap-exhaustion error");
        };
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            error.message.contains("max_utilization"),
            "{}",
            error.message
        );
        assert!(error
            .response
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER));
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
            None,
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
    fn routed_api_key_requests_to_stock_openai_generate_the_affinity_headers() {
        let mut client = HeaderMap::new();
        client.insert("authorization", "Bearer client-secret".parse().unwrap());
        client.insert("x-api-key", "client-api-key".parse().unwrap());
        client.insert("originator", "codex_cli_rs".parse().unwrap());
        client.insert("session-id", "client-session".parse().unwrap());

        let request = built(routed_request(
            &state(),
            &route("openai"),
            Credential::ApiKey {
                value: "openai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            &client,
            Some("sess-routed"),
        ));

        // The client sent none of the three thread-derived ids, so all four
        // affinity headers generate from the threaded effective id — never the
        // client's own `session-id: client-session`. (The pass-through case —
        // the client DID send its thread-derived ids — is the F7 test above.)
        assert_eq!(request.headers().get("session-id").unwrap(), "sess-routed");
        assert_eq!(request.headers().get("thread-id").unwrap(), "sess-routed");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "sess-routed"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "sess-routed:0"
        );
        // The allowlist still swaps in the resolved credential and drops the
        // caller's own secret and telemetry.
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer openai-key"
        );
        assert!(request.headers().get("x-api-key").is_none());
        assert!(request.headers().get("originator").is_none());
    }

    /// The client's own thread-derived codex ids pass through to the stock
    /// OpenAI api-key arm when the CLI sent them (F7): a child thread's
    /// `thread-id`, its request id, and its real window id are identities
    /// shunt cannot recompute — regenerating them from the session id alone
    /// collapsed a child thread onto its parent's cache namespace. Only the
    /// three thread-derived ids pass; `session-id` stays the threaded
    /// effective id, which is what the body's `prompt_cache_key` carries.
    #[test]
    fn the_routed_api_key_arm_passes_the_clients_codex_identity_through() {
        let mut client = HeaderMap::new();
        client.insert(
            "thread-id",
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7"
                .parse()
                .unwrap(),
        );
        client.insert(
            "x-client-request-id",
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7"
                .parse()
                .unwrap(),
        );
        client.insert(
            "x-codex-window-id",
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7:1"
                .parse()
                .unwrap(),
        );

        let request = built(routed_request(
            &state(),
            &route("openai"),
            Credential::ApiKey {
                value: "openai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            &client,
            Some("sess-routed"),
        ));

        assert_eq!(
            request.headers().get("thread-id").unwrap(),
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7"
        );
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f::agent-7:1"
        );
        // `session-id` is the one header shunt still generates: its value must
        // equal the body's prompt_cache_key, which derives from the threaded
        // effective id.
        assert_eq!(request.headers().get("session-id").unwrap(), "sess-routed");
    }

    /// A partially-sent set passes through per header: the missing ones are
    /// still generated, so a CLI that predates one of the ids loses nothing.
    #[test]
    fn the_routed_api_key_arm_generates_only_the_absent_identity_headers() {
        let mut client = HeaderMap::new();
        client.insert(
            "x-codex-window-id",
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f:1".parse().unwrap(),
        );

        let request = built(routed_request(
            &state(),
            &route("openai"),
            Credential::ApiKey {
                value: "openai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            &client,
            Some("sess-routed"),
        ));

        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "0f9c7b2e-4d51-7000-8000-1a2b3c4d5e6f:1"
        );
        assert_eq!(request.headers().get("thread-id").unwrap(), "sess-routed");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "sess-routed"
        );
    }

    #[test]
    fn routed_api_key_requests_to_a_third_party_host_omit_the_affinity_headers() {
        let mut config = Config::default();
        config.providers.get_mut("openai").unwrap().base_url =
            "https://relay.example/v1".to_string();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut client = HeaderMap::new();
        client.insert("session-id", "client-session".parse().unwrap());

        let request = built(routed_request(
            &state,
            &route("openai"),
            Credential::ApiKey {
                value: "openai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            &client,
            Some("sess-routed"),
        ));

        assert!(request.headers().get("session-id").is_none());
        assert!(request.headers().get("thread-id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
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
