//! Build the upstream Responses HTTP request (URL, auth, identity headers) and
//! resolve the per-provider Responses endpoint.

use axum::http::HeaderValue;

use crate::{auth::Credential, routing::Route, server::AppState};

/// Codex CLI client identity, mirrored from openai/codex rust-v0.148.0.
///
/// The ChatGPT backend routes newer model slugs (e.g. gpt-5.6-luna, which has
/// `minimal_client_version: 0.144.0`) by client identity and answers
/// "Model not found" — not an entitlement error — when the identity is
/// missing or too old. Per openai/codex#31967 the gate keys on the
/// `originator` + `version` header combination; the `user-agent` is sent for
/// fidelity with Codex, which builds it as
/// `{originator}/{version} ({os} {os_version}; {arch}) {terminal}`
/// (codex-rs/login/src/auth/default_client.rs) and sends the bare CLI
/// version in a `version` header (codex-rs/model-provider-info/src/lib.rs).
/// Bump both together when a new slug requires a newer client version.
pub(super) const CODEX_USER_AGENT: &str = "codex_cli_rs/0.148.0";
pub(super) const CODEX_CLIENT_VERSION: &str = "0.148.0";

/// Grok CLI identity, mirrored from the official Grok CLI (via
/// raine/claude-code-proxy `src/providers/grok/client.rs`). The subscription
/// surface (`cli-chat-proxy.grok.com`) gates on these headers: without them it
/// answers as if the caller were an unentitled API client. Sent only with the
/// `XaiOauth` (subscription bearer) credential.
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";
const GROK_CLIENT_VERSION: &str = "0.2.93";

/// Upper bound on the `upstream_model` slug interpolated into the routing hint.
/// Shares the value of `observability::MAX_MODEL_TAG_LEN`, which bounds this
/// same client-supplied string at its other sink, but **not its unit**: that one
/// counts `char`s (`.chars().take(..)`), this one counts bytes (`.len()`). Bytes
/// is the right unit here because the bound exists to cap what goes on the wire,
/// and a header value is measured in bytes; it is also the stricter of the two,
/// since a UTF-8 string is never fewer bytes than `char`s.
pub(super) const MAX_ROUTING_HINT_MODEL_LEN: usize = 128;

/// Whether `model` may be interpolated into the routing hint.
///
/// A model slug is an opaque id. Every slug reachable on this path is drawn from
/// this set — verified against the Codex/OpenAI slugs this repo knows about
/// (`gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.4`,
/// `gpt-5.4-mini`, `gpt-5.2`, `gpt-5.2-codex`, …), which use only `-` and `.`;
/// `_`, `:`, `/` and `+` are headroom for provider-qualified id styles. Anything
/// outside it cannot be a real slug and must not be interpolated into the hint's
/// `model=<m>[;tier=<t>]` grammar, whose server-side parser shunt does not own.
///
/// A positive allowlist rather than a denylist of metacharacters: `HeaderValue`
/// admits every visible ASCII byte plus TAB, so enumerating separators against a
/// remote grammar shunt cannot observe is the guard that erodes — a parser
/// splitting on `,` (the standard HTTP list separator) would read
/// `model=gpt-5,tier=priority` as two fields, the same forge a `;`-only rule was
/// added to stop.
///
/// The emptiness clause is load-bearing, not decoration:
/// `routing::strip_context_window_hint("[1m]")` returns `""` (pinned by its own
/// test), so an empty `upstream_model` is reachable and would otherwise emit a
/// meaningless `model=`.
fn is_hint_safe_slug(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_ROUTING_HINT_MODEL_LEN
        && model.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/' | b'+')
        })
}

/// Build the `x-codex-routing-hint` value the Codex CLI sends on the ChatGPT
/// backend (`X_CODEX_ROUTING_HINT_HEADER` / `build_routing_hint_header`,
/// codex-rs/core/src/client.rs): the upstream model slug, plus `;tier=<tier>`
/// when a service tier is on the wire. The tier predicate mirrors the request
/// body's exactly (see `model/responses_request.rs`) — `"default"` is a
/// client-only sentinel stripped before the body is serialized, so a hint built
/// from a *route-configured* tier can never advertise a tier the body omitted.
///
/// Returns `None` — omit the header — rather than failing the turn, because
/// unlike upstream codex (which builds the hint from its own local config)
/// shunt's `upstream_model` is **client-controlled**: a prefix route or the
/// default provider passes the request's raw `model` string straight through
/// (`routing.rs`, only a trailing `[1m]` stripped). Omitted when the slug fails
/// [`is_hint_safe_slug`], and — belt-and-braces behind that allowlist — when the
/// assembled value is not a valid header value. Building it unconditionally
/// would defer a `HeaderValue` rejection to reqwest's builder, surfacing at
/// `.send()` as a non-transient error that the Codex OAuth pool classifies as an
/// account transport failure, cooling *every* account for 30s off one malformed
/// client string.
///
/// Failing closed by omission is what upstream does too (`…from_str(&hint).ok()`
/// behind an `if let Some`), and matches this repo's own `stamp_gateway_headers`
/// (`proxy/failover.rs`) for the equally client-derived `x-gateway-model`.
pub(super) fn routing_hint(route: &Route) -> Option<HeaderValue> {
    let model = route.upstream_model.as_str();
    if !is_hint_safe_slug(model) {
        // Debug, never warn/info: this is client-triggerable, so a higher level
        // is a log-flood vector. The model itself is unvalidated client free
        // text and is never logged — only its length, which is enough to tell
        // empty from over-long from a bad character.
        tracing::debug!(
            model_len = model.len(),
            "routing hint omitted: upstream model is not usable in the hint grammar"
        );
        return None;
    }
    let hint = match route
        .service_tier
        .as_deref()
        .filter(|tier| *tier != "default")
    {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    };
    match HeaderValue::from_str(&hint) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::debug!(
                model_len = model.len(),
                "routing hint omitted: not a valid header value"
            );
            None
        }
    }
}

pub(super) fn request_builder(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .header("content-type", "application/json");
    // `OpenAI-Beta: responses=experimental` is an OpenAI/ChatGPT header; xAI's
    // Responses API doesn't expect it and the reference clients don't send it.
    if !matches!(
        state.config.responses_flavor(&route.provider),
        crate::config::ResponsesFlavor::Xai | crate::config::ResponsesFlavor::Grok
    ) {
        request = request.header("OpenAI-Beta", "responses=experimental");
    }
    match credential {
        // The Responses API is always Bearer-authenticated; the configured
        // api_key_header only governs the Anthropic passthrough adapter.
        Credential::ApiKey { value, .. } => {
            request = request.bearer_auth(value);
        }
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            request = request
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id)
                .header("originator", "codex_cli_rs")
                .header("user-agent", CODEX_USER_AGENT)
                .header("version", CODEX_CLIENT_VERSION);
            // Omitted, never fatal, when the client-controlled model makes an
            // unusable value — see [`routing_hint`].
            if let Some(hint) = routing_hint(route) {
                request = request.header("x-codex-routing-hint", hint);
            }
            // Session/identity headers the real Codex CLI sends alongside the
            // client identity above (raine/claude-code-proxy build_codex_headers,
            // cross-checked against codex-rs/login/src/auth/default_client.rs).
            // Only sent when a session id is available; xAI/OpenAI-compatible
            // upstreams never reach this branch.
            if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
                request = request
                    .header("accept", "text/event-stream")
                    .header("session_id", session_id)
                    .header("x-client-request-id", session_id)
                    .header("x-codex-window-id", format!("{session_id}:0"));
            }
        }
        // xAI subscription OAuth: the subscription bearer plus the Grok-CLI
        // identity headers the CLI chat proxy expects (no ChatGPT/Codex
        // account-id/originator headers). `accept: text/event-stream` matches
        // the real Grok CLI; the upstream is always consumed as SSE.
        Credential::XaiOauth { access_token } => {
            request = request
                .bearer_auth(access_token)
                .header("accept", "text/event-stream")
                .header("x-xai-token-auth", "xai-grok-cli")
                .header("x-grok-client-identifier", GROK_CLIENT_IDENTIFIER)
                .header("x-grok-client-version", GROK_CLIENT_VERSION);
        }
        Credential::ClaudeOauth { access_token, .. }
        | Credential::GoogleOauth { access_token, .. } => {
            request = request.bearer_auth(access_token);
        }
        // A Responses provider configured with passthrough auth is a
        // misconfiguration; send no credential and let the upstream reject it.
        // Kimi's coding API speaks the Anthropic Messages shape, so a
        // `kimi_oauth` provider is always `kind = "anthropic"` and never
        // reaches the Responses adapter in practice.
        //
        // An Antigravity token belongs to the same class: config validation
        // pins `antigravity_oauth` to `kind = "antigravity"`, so it cannot
        // legitimately reach a Responses upstream. Fail closed rather than
        // bearer either one — the hosts on this path (OpenAI, xAI, Cursor) are
        // not the origin those subscription tokens were issued for, so a
        // reachable bug here would be a credential leak rather than a 401.
        Credential::CursorOauth { .. }
        | Credential::KimiOauth { .. }
        | Credential::AntigravityOauth { .. }
        | Credential::Passthrough => {}
    }
    request
}

pub(super) fn responses_url(config: &crate::config::Config, provider: &str) -> String {
    let base = config
        .provider(provider)
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("https://api.openai.com/v1")
        .trim_end_matches('/');
    // The ChatGPT/Codex backend serves the Responses API under /codex/responses;
    // a plain OpenAI-compatible upstream uses /responses.
    if config.is_chatgpt_backend(provider) {
        format!("{base}/codex/responses")
    } else {
        format!("{base}/responses")
    }
}

#[cfg(test)]
fn build_test_request(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
) -> reqwest::Request {
    request_builder(state, route, credential, session_id)
        .body("{}")
        .build()
        .expect("test request should build")
}

#[cfg(test)]
mod tests {
    use crate::{
        auth::Credential,
        config::Config,
        routing::{AdapterKind, Route},
        server::AppState,
    };

    use super::{build_test_request, request_builder, responses_url};

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    #[test]
    fn builds_codex_url_and_headers_without_sending() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
        );

        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", "access-token").as_str()
        );
        assert_eq!(
            request.headers().get("chatgpt-account-id").unwrap(),
            "account-id"
        );
        assert_eq!(request.headers().get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            request.headers().get("user-agent").unwrap(),
            super::CODEX_USER_AGENT
        );
        assert_eq!(
            request.headers().get("version").unwrap(),
            super::CODEX_CLIENT_VERSION
        );
        // No service tier on the route ⇒ the routing hint carries the model alone.
        assert_eq!(
            request.headers().get("x-codex-routing-hint").unwrap(),
            "model=gpt-5.2-codex"
        );
        assert_eq!(
            request.headers().get("OpenAI-Beta").unwrap(),
            "responses=experimental"
        );
        // No session id was supplied: the session/identity headers must not
        // be sent, since a fabricated value would be worse than omitting them.
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
        assert!(request.headers().get("accept").is_none());
    }

    #[test]
    fn routing_hint_appends_a_configured_service_tier() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let route = Route {
            service_tier: Some("priority".to_string()),
            ..codex_route()
        };

        let request = build_test_request(
            &state,
            &route,
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
        );

        assert_eq!(
            request.headers().get("x-codex-routing-hint").unwrap(),
            "model=gpt-5.2-codex;tier=priority"
        );
    }

    #[test]
    fn routing_hint_omits_the_default_service_tier_sentinel() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let route = Route {
            service_tier: Some("default".to_string()),
            ..codex_route()
        };

        let request = build_test_request(
            &state,
            &route,
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
        );

        // `"default"` is stripped from the request body, so the hint must not
        // advertise a tier the body never sent.
        assert_eq!(
            request.headers().get("x-codex-routing-hint").unwrap(),
            "model=gpt-5.2-codex"
        );
    }

    #[test]
    fn routing_hint_is_absent_for_an_api_key_credential() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ApiKey {
                value: "api-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            None,
        );

        // Upstream suppresses the hint for api-key/bearer/aws providers.
        assert!(request.headers().get("x-codex-routing-hint").is_none());
    }

    /// The route a prefix/default-provider match produces: `upstream_model` is
    /// the client's raw `model` string (`routing.rs`), so these are the values a
    /// request body can put there.
    fn client_model_route(model: &str) -> Route {
        Route {
            model: model.to_string(),
            upstream_model: model.to_string(),
            ..codex_route()
        }
    }

    fn codex_oauth() -> Credential {
        Credential::ChatGptOAuth {
            access_token: "access-token".to_string(),
            account_id: "account-id".to_string(),
        }
    }

    #[test]
    fn routing_hint_is_omitted_for_a_model_outside_the_slug_allowlist() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        // Each case carries exactly ONE character outside the allowlist, so
        // widening the allowlist by one byte reddens exactly one case. (A
        // realistic forge like `gpt-5,tier=priority` is rejected by its `=`
        // before the `,` is ever reached, which would make it useless for
        // pinning the `,` rule specifically — the full forges are asserted
        // separately below.)
        let one_bad_character = [
            "gpt-5;codex",  // the hint's own segment delimiter
            "gpt-5,codex",  // the standard HTTP list separator
            "gpt-5=codex",  // the hint's own key/value delimiter
            "gpt-5 codex",  // SP — accepted by `HeaderValue`
            "gpt-5\tcodex", // TAB — likewise accepted by `HeaderValue`
            "gpt-5\"codex", // quoting, for a parser that unquotes
            "gpt-5\ncodex", // control character: not a valid header value at all
        ];
        // The forges these rules exist to stop, asserted as whole strings.
        let forged = [
            "gpt-5.2-codex;tier=priority",
            "gpt-5,tier=priority",
            "gpt-5 tier=priority",
            "gpt-5\ttier=priority",
            // Empty is reachable: `strip_context_window_hint("[1m]") == ""`.
            "",
        ];
        let outside_the_allowlist = one_bad_character.iter().chain(forged.iter()).copied();
        for model in outside_the_allowlist {
            let request =
                build_test_request(&state, &client_model_route(model), codex_oauth(), None);
            assert!(
                request.headers().get("x-codex-routing-hint").is_none(),
                "model {model:?} must not produce a routing hint"
            );
        }

        // The allowlist still admits every real slug shape.
        for model in ["gpt-5.6-sol", "gpt-5.2-codex", "gpt-5.4-mini"] {
            let request =
                build_test_request(&state, &client_model_route(model), codex_oauth(), None);
            assert_eq!(
                request.headers().get("x-codex-routing-hint").unwrap(),
                format!("model={model}").as_str()
            );
        }
    }

    #[test]
    fn routing_hint_is_omitted_for_an_over_long_model() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let model = "g".repeat(super::MAX_ROUTING_HINT_MODEL_LEN + 1);

        let request = build_test_request(&state, &client_model_route(&model), codex_oauth(), None);

        assert!(request.headers().get("x-codex-routing-hint").is_none());
        // The bound is inclusive: exactly the limit still sends a hint, so the
        // test above is failing on the length rule and not on some other guard.
        let at_limit = "g".repeat(super::MAX_ROUTING_HINT_MODEL_LEN);
        let request =
            build_test_request(&state, &client_model_route(&at_limit), codex_oauth(), None);
        assert_eq!(
            request.headers().get("x-codex-routing-hint").unwrap(),
            format!("model={at_limit}").as_str()
        );
    }

    #[test]
    fn a_control_character_model_omits_the_hint_and_still_builds_the_request() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        // Regression guard for the pool-cooldown defect: interpolating a control
        // character into the header would defer a `HeaderValue` rejection to
        // reqwest's builder, surfacing at `.send()` as a non-transient error that
        // the Codex OAuth pool charges to the account as a 30s transport
        // cooldown — deterministic, so it would cool every account in turn.
        // `build` must therefore still succeed, with the hint simply absent.
        let request = request_builder(
            &state,
            &client_model_route("gpt-5\n-sol"),
            codex_oauth(),
            None,
        )
        .body("{}")
        .build()
        .expect("a malformed model must not fail the request build");

        assert!(request.headers().get("x-codex-routing-hint").is_none());
        // The rest of the identity is unaffected — only the hint is dropped.
        assert_eq!(request.headers().get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            request.headers().get("version").unwrap(),
            super::CODEX_CLIENT_VERSION
        );
    }

    #[test]
    fn forwards_session_headers_on_codex_backend_when_session_id_present() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some("session-123"),
        );

        assert_eq!(
            request.headers().get("accept").unwrap(),
            "text/event-stream"
        );
        assert_eq!(request.headers().get("session_id").unwrap(), "session-123");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "session-123:0"
        );
    }

    #[test]
    fn omits_session_headers_when_session_id_is_empty_string() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some(""),
        );

        assert!(request.headers().get("accept").is_none());
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    #[test]
    fn builds_openai_responses_url() {
        assert_eq!(
            responses_url(&Config::default(), "openai"),
            "https://api.openai.com/v1/responses"
        );
    }

    fn xai_route() -> Route {
        Route {
            provider: "xai".to_string(),
            adapter: AdapterKind::Responses,
            model: "grok-4.3".to_string(),
            upstream_model: "grok-4.3".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    fn grok_route() -> Route {
        Route {
            provider: "grok".to_string(),
            adapter: AdapterKind::Responses,
            model: "grok-4.5".to_string(),
            upstream_model: "grok-4.5".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    #[test]
    fn builds_grok_oauth_request_with_cli_identity_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &grok_route(),
            Credential::XaiOauth {
                access_token: "xai-access".to_string(),
            },
            Some("session-123"),
        );

        // The subscription OAuth path targets the Grok CLI chat proxy, not
        // api.x.ai, and carries the Grok-CLI identity headers it gates on.
        assert_eq!(
            request.url().as_str(),
            "https://cli-chat-proxy.grok.com/v1/responses"
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            format!("Bearer {}", "xai-access").as_str()
        );
        assert_eq!(
            request.headers().get("x-xai-token-auth").unwrap(),
            "xai-grok-cli"
        );
        assert_eq!(
            request.headers().get("x-grok-client-identifier").unwrap(),
            "grok-shell"
        );
        assert_eq!(
            request.headers().get("x-grok-client-version").unwrap(),
            "0.2.93"
        );
        assert_eq!(
            request.headers().get("accept").unwrap(),
            "text/event-stream"
        );
        // No ChatGPT/Codex headers and no OpenAI-Beta for the xai flavor, even
        // when a session id is present on the request.
        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get("originator").is_none());
        assert!(request.headers().get("user-agent").is_none());
        assert!(request.headers().get("version").is_none());
        assert!(request.headers().get("OpenAI-Beta").is_none());
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    #[test]
    fn builds_xai_api_key_request_bearer_only_without_cli_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &xai_route(),
            Credential::ApiKey {
                value: "xai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            None,
        );

        // The API-key path stays on the developer API and sends the bearer
        // only — no Grok-CLI identity headers, no OpenAI-Beta (xai flavor).
        assert_eq!(request.url().as_str(), "https://api.x.ai/v1/responses");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            format!("Bearer {}", "xai-key").as_str()
        );
        assert!(request.headers().get("x-xai-token-auth").is_none());
        assert!(request.headers().get("x-grok-client-identifier").is_none());
        assert!(request.headers().get("x-grok-client-version").is_none());
        assert!(request.headers().get("OpenAI-Beta").is_none());
    }

    #[test]
    fn builds_claude_oauth_request_with_bearer_only() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ClaudeOauth {
                access_token: "claude-token".to_string(),
                account_uuid: None,
            },
            None,
        );

        // A Claude OAuth credential on a Responses provider sends only the bearer
        // — none of the ChatGPT/Codex account-id or identity headers.
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", "claude-token").as_str()
        );
        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get("originator").is_none());
        assert!(request.headers().get("version").is_none());
    }

    #[test]
    fn antigravity_oauth_sends_no_credential_on_the_responses_path() {
        // Config validation pins `antigravity_oauth` to `kind = "antigravity"`,
        // so this arm is unreachable in a valid config — but if it were ever
        // reached, it must fail closed rather than bearer a subscription token
        // issued for a different origin (OpenAI/xAI/Cursor are not it).
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::AntigravityOauth {
                access_token: "antigravity-token".to_string(),
                project_id: "proj-1".to_string(),
            },
            None,
        );

        assert!(request.headers().get("authorization").is_none());
        assert!(request.headers().get("x-api-key").is_none());
    }
}
