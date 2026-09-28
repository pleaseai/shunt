//! Build the upstream Responses HTTP request (URL, auth, identity headers) and
//! resolve the per-provider Responses endpoint.

use axum::http::{HeaderMap, HeaderValue};

use crate::{auth::Credential, routing::Route, server::AppState};

/// Codex CLI client identity, mirrored from openai/codex rust-v0.156.0.
///
/// The ChatGPT backend routes newer model slugs (e.g. gpt-6-sol and gpt-6-luna,
/// which have `minimal_client_version: 0.155.0`) by client identity and
/// answers "Model not found" — not an entitlement error — when the identity
/// is missing or too old. Per openai/codex#31967 the gate keys on the
/// `originator` + `version` header combination; the `user-agent` is sent for
/// fidelity with Codex, which builds it as
/// `{originator}/{version} ({os} {os_version}; {arch}) {terminal}`
/// (codex-rs/login/src/auth/default_client.rs) and sends the bare CLI
/// version in a `version` header (codex-rs/model-provider-info/src/lib.rs).
/// Bump both together when a new slug requires a newer client version.
/// `pub(crate)`: also reused by `crate::auth::codex::usage` (the wham/usage
/// poller) so its CLI identity headers cannot drift from the Responses
/// adapter's own.
pub(crate) const CODEX_USER_AGENT: &str = "codex_cli_rs/0.156.0";
pub(crate) const CODEX_CLIENT_VERSION: &str = "0.156.0";

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

/// The Grok-CLI identity the subscription chat proxy gates on, sent alongside
/// an `XaiOauth` bearer. `accept: text/event-stream` matches the real Grok CLI;
/// that upstream is always consumed as SSE.
///
/// Shared with `super::inbound_routed`: a `[[server.codex_endpoint.routes]]`
/// entry may name an `xai_oauth` provider, and a routed request that carried
/// only the bearer would be answered as if the caller were an unentitled API
/// client. One owner, or the two call sites drift apart the next time the CLI
/// version moves.
pub(super) fn grok_identity_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("accept", "text/event-stream")
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-identifier", GROK_CLIENT_IDENTIFIER)
        .header("x-grok-client-version", GROK_CLIENT_VERSION)
}

/// The codex identity a DELEGATED turn's chatgpt-flavor headers carry: codex
/// gives each subagent its own thread id, echoes the parent's, and labels the
/// subagent kind (`codex-rs` `responses_metadata.rs`). A non-delegated turn
/// has none of it — `None` means the plain session headers, exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexDelegation {
    /// The thread-id to send in place of the session id (`{session}::{agent}`).
    pub(crate) thread_id: String,
    /// The parent session's thread id (the effective session id).
    pub(crate) parent_thread_id: String,
    /// The subagent kind label: the client's agent-type hint when sent, else
    /// a stable literal (`codex` uses `Other(label)` for a Task child).
    pub(crate) subagent: String,
    /// The raw agent id, for the internal identity key's own component (the
    /// internal keys must not inherit the wire `::{agent}` concatenation —
    /// see `compose_identity_key`).
    pub(crate) agent_id: String,
}

/// The delegated-turn codex identity, derived from the effective session id
/// and the client's gateway hints. `None` for a non-delegated turn, and for a
/// delegated turn that sent no agent id — that turn shares the parent scope
/// and gets the plain parent headers.
pub(crate) fn codex_delegation(headers: &HeaderMap, session_id: &str) -> Option<CodexDelegation> {
    let hints = crate::routing::context::RouterContext::from_headers(headers);
    if !hints.is_delegated() {
        return None;
    }
    let agent_id = hints.pin_agent_id()?;
    Some(CodexDelegation {
        thread_id: format!("{session_id}::{agent_id}"),
        parent_thread_id: session_id.to_string(),
        subagent: hints
            .agent_type
            .filter(|agent_type| !agent_type.trim().is_empty())
            .unwrap_or("subagent")
            .to_string(),
        agent_id: agent_id.to_string(),
    })
}

/// The four prompt-cache affinity headers the real Codex CLI sends with every
/// request regardless of auth (`codex-rs`
/// `codex-api/src/requests/headers.rs` `build_session_headers`, pinned by its
/// api-key test): `session-id` and `thread-id` carry the conversation id, and
/// the upstream derives prompt-cache affinity from the `session-id` header
/// (`codex-rs` `core/src/client.rs` `responses_session_id`) — so its value must
/// equal the body's `prompt_cache_key`, which derives from the same effective
/// id. `x-client-request-id` and `x-codex-window-id` complete the set.
///
/// A delegated turn swaps `thread-id` for the child's derived id and adds the
/// two subagent markers, and the thread-derived ids (`x-client-request-id`,
/// the window id's identity part) carry the child's too. `window` is the
/// conversation's compaction window — the same counter the websocket
/// handshake reads — so both transports carry the same `{thread}:{window}`.
///
/// `client_ids` passes the caller's OWN codex identity headers through
/// (routed `[server.codex_endpoint]` turns, where the client is the Codex CLI
/// itself): for each of `thread-id`, `x-client-request-id` and
/// `x-codex-window-id`, a header the client sent replaces the generated
/// value — a child thread's ids and its real window are identities shunt
/// cannot recompute. The values arrive as hyper-parsed `HeaderValues`, so
/// they are valid header text by construction; `session-id` is never
/// overridden, because its value must equal the body's `prompt_cache_key`.
/// Sent only when a session id is available: a fabricated value is worse
/// than omitting them. Each caller picks its own upstream gate (the ChatGPT
/// backend, or the stock OpenAI host for api-key) and its own `accept`
/// header.
pub(super) fn session_affinity_headers(
    request: reqwest::RequestBuilder,
    session_id: Option<&str>,
    delegation: Option<&CodexDelegation>,
    window: u64,
    client_ids: Option<&HeaderMap>,
) -> reqwest::RequestBuilder {
    match session_id.filter(|session_id| !session_id.is_empty()) {
        Some(session_id) => {
            // The request id and the window id's identity part are
            // thread-derived like codex's: the child's id on a delegated
            // turn, the session's otherwise.
            let thread_id =
                delegation.map_or(session_id, |delegation| delegation.thread_id.as_str());
            let client =
                |name: &'static str| client_ids.and_then(|headers| headers.get(name)).cloned();
            let mut request = match delegation {
                Some(delegation) => request
                    .header("x-codex-parent-thread-id", &delegation.parent_thread_id)
                    .header("x-openai-subagent", &delegation.subagent),
                None => request,
            };
            request = request.header("session-id", session_id);
            request = match client("thread-id") {
                Some(value) => request.header("thread-id", value),
                None => request.header("thread-id", thread_id),
            };
            request = match client("x-client-request-id") {
                Some(value) => request.header("x-client-request-id", value),
                None => request.header("x-client-request-id", thread_id),
            };
            request = match client("x-codex-window-id") {
                Some(value) => request.header("x-codex-window-id", value),
                None => request.header("x-codex-window-id", format!("{thread_id}:{window}")),
            };
            request
        }
        None => request,
    }
}

/// Redirect policy for the shared Responses HTTP client — the process-wide
/// client, so this governs every request shunt sends through it: Responses
/// turns, the codex usage/wham polls, model discovery, and the admin surface's
/// upstream calls alike. A request chain that
/// started on a host that receives the generated codex identity headers —
/// stock OpenAI on the api-key arm, the ChatGPT backend on the subscription
/// OAuth arm — must not carry them across hosts (reqwest strips only
/// credentials on a host change; the policy sees the chain's URLs, never its
/// headers, so the guard keys on the origin and judges the hop against that
/// origin's own domain predicate), so a cross-host 3xx stops at the hop and
/// the redirect response relays to the client like any other upstream status.
/// Same-host hops still follow, capped like reqwest's default. A loopback
/// chatgpt-oauth origin passes config validation (the https and host checks
/// skip loopback) and stays unguarded; it already holds the plaintext bearer
/// by design, so the guard adds nothing there.
pub(crate) fn codex_identity_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let origin = attempt.previous().first().and_then(|url| url.host_str());
        let target = attempt.url().host_str().unwrap_or_default();
        let crosses = match origin {
            Some(origin) if crate::config::host_is_openai(origin) => {
                !crate::config::host_is_openai(target)
            }
            Some(origin) if crate::config::host_is_chatgpt(origin) => {
                !crate::config::host_is_chatgpt(target)
            }
            _ => false,
        };
        if crosses {
            attempt.stop()
        } else if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    })
}

pub(super) fn request_builder(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    delegation: Option<&CodexDelegation>,
    window: u64,
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
            // Stock OpenAI is the one api-key Responses upstream codex sends its
            // session-affinity headers to (pinned by codex-rs's api-key test);
            // third-party OpenAI-compatible hosts and xAI keep them absent.
            if state.config.is_openai_backend(&route.provider) {
                // Stock OpenAI gets the four affinity headers and nothing more:
                // the subagent markers are a codex-backend shape.
                request = session_affinity_headers(request, session_id, None, window, None);
            }
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
            // The ChatGPT backend derives prompt-cache affinity from the shared
            // session headers; this branch is the only one that reaches it
            // (xAI/OpenAI-compatible upstreams never do).
            if session_id.is_some_and(|session_id| !session_id.is_empty()) {
                request = request.header("accept", "text/event-stream");
            }
            request = session_affinity_headers(request, session_id, delegation, window, None);
        }
        // xAI subscription OAuth: the subscription bearer plus the Grok-CLI
        // identity headers the CLI chat proxy expects (no ChatGPT/Codex
        // account-id/originator headers).
        Credential::XaiOauth { access_token } => {
            request = grok_identity_headers(request.bearer_auth(access_token));
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
    request_builder(state, route, credential, session_id, None, 0)
        .body("{}")
        .build()
        .expect("test request should build")
}

#[cfg(test)]
mod tests {
    #[test]
    fn codex_delegation_derives_only_for_a_child_turn() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("sess-1"),
        );
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        headers.insert(
            "x-claude-code-agent-type",
            HeaderValue::from_static("Explore"),
        );
        let delegation = codex_delegation(&headers, "sess-1").expect("a child turn delegates");
        assert_eq!(delegation.thread_id, "sess-1::agent-7");
        assert_eq!(delegation.parent_thread_id, "sess-1");
        assert_eq!(delegation.subagent, "Explore");

        // The class is authoritative in the positive direction too: a
        // `subagent` class with an agent id is a child.
        let mut classed = HeaderMap::new();
        classed.insert(
            "x-claude-code-request-class",
            HeaderValue::from_static("subagent"),
        );
        classed.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        let delegation =
            codex_delegation(&classed, "sess-1").expect("a subagent-class turn delegates");
        assert_eq!(delegation.thread_id, "sess-1::agent-7");

        // No agent id -> the turn shares the parent scope, plain headers.
        let mut plain = HeaderMap::new();
        plain.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("sess-1"),
        );
        assert_eq!(codex_delegation(&plain, "sess-1"), None);

        // The class is authoritative: `main` with an agent id is not a child.
        let mut main = HeaderMap::new();
        main.insert(
            "x-claude-code-request-class",
            HeaderValue::from_static("main"),
        );
        main.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        assert_eq!(codex_delegation(&main, "sess-1"), None);

        // No agent-type hint -> the stable literal, codex's Other(label) analog.
        let mut untyped = HeaderMap::new();
        untyped.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        assert_eq!(
            codex_delegation(&untyped, "sess-1").unwrap().subagent,
            "subagent"
        );
    }

    #[test]
    fn delegated_turns_carry_the_child_thread_and_subagent_markers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        let delegation = codex_delegation(&headers, "session-123").expect("a child turn delegates");

        let request = request_builder(
            &state,
            &codex_route(),
            codex_oauth(),
            Some("session-123"),
            Some(&delegation),
            0,
        )
        .body("{}")
        .build()
        .expect("test request should build");

        // The child's derived thread id replaces the parent's, the two markers
        // ride along, and every thread-derived id — request id and window id
        // included — carries the child identity; only `session-id` (and the
        // body's prompt cache key) stay the parent's.
        assert_eq!(request.headers().get("session-id").unwrap(), "session-123");
        assert_eq!(
            request.headers().get("thread-id").unwrap(),
            "session-123::agent-7"
        );
        assert_eq!(
            request.headers().get("x-codex-parent-thread-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-openai-subagent").unwrap(),
            "subagent"
        );
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123::agent-7"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "session-123::agent-7:0"
        );

        // A non-delegated turn sends the pre-change header set: no markers,
        // and thread-id stays the parent's.
        let plain = request_builder(
            &state,
            &codex_route(),
            codex_oauth(),
            Some("session-123"),
            None,
            0,
        )
        .body("{}")
        .build()
        .expect("test request should build");
        assert_eq!(plain.headers().get("thread-id").unwrap(), "session-123");
        assert!(plain.headers().get("x-codex-parent-thread-id").is_none());
        assert!(plain.headers().get("x-openai-subagent").is_none());
    }

    use crate::{
        auth::Credential,
        config::{Config, ResponsesFlavor},
        routing::{AdapterKind, Route},
        server::AppState,
    };

    use axum::http::{HeaderMap, HeaderValue};

    use super::{build_test_request, codex_delegation, request_builder, responses_url};

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

    /// The upstream `session-id`/`thread-id` headers and the body
    /// `prompt_cache_key` must carry one conversation id: the backend derives
    /// cache affinity from the header, and a header/key mismatch caches
    /// nothing (measured 2026-09-20, openai/codex#44716). Pinned across the
    /// two modules so a divergence of their id sources reds here.
    #[test]
    fn the_session_headers_and_the_body_prompt_cache_key_share_one_id() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
        let route = codex_route();
        let request = build_test_request(
            &state,
            &route,
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some("session-123"),
        );
        assert_eq!(request.headers().get("session-id").unwrap(), "session-123");
        assert_eq!(request.headers().get("thread-id").unwrap(), "session-123");

        let body = serde_json::to_vec(&serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "{\"session_id\":\"meta_sess\"}"}
        }))
        .unwrap();

        // Header-carrying client: header and body key derive from one id.
        let translated = crate::model::responses_request::translate_request(
            &body,
            &route,
            ResponsesFlavor::Chatgpt,
            false,
            Some("session-123"),
            true,
        )
        .unwrap();
        assert_eq!(translated["prompt_cache_key"], "session-123");

        // Metadata-only client: the derived id the adapter emits as the header
        // must equal the key the translator derives from metadata alone.
        let derived = crate::model::responses_request::effective_session_id(
            &serde_json::json!({"metadata": {"user_id": "{\"session_id\":\"meta_sess\"}"}}),
            None,
        )
        .unwrap();
        assert_eq!(derived, "meta_sess");
        let translated = crate::model::responses_request::translate_request(
            &body,
            &route,
            ResponsesFlavor::Chatgpt,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(translated["prompt_cache_key"], "meta_sess");

        // A plain (non-JSON) user_id: the hash fallback feeds both sides, so
        // the headers and the key still share one value.
        let plain_body = serde_json::to_vec(&serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "plain-user"}
        }))
        .unwrap();
        let derived = crate::model::responses_request::effective_session_id(
            &serde_json::json!({"metadata": {"user_id": "plain-user"}}),
            None,
        )
        .unwrap();
        assert_eq!(derived.len(), 16);
        assert!(derived.chars().all(|c| c.is_ascii_hexdigit()));
        let translated = crate::model::responses_request::translate_request(
            &plain_body,
            &route,
            ResponsesFlavor::Chatgpt,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(translated["prompt_cache_key"], derived);

        // An escaped control character in the metadata session cannot be a
        // header value; the hash fallback keeps header and key equal instead
        // of failing the request.
        let derived = crate::model::responses_request::effective_session_id(
            &serde_json::json!({"metadata": {"user_id": "{\"session_id\":\"bad\\nid\"}"}}),
            None,
        )
        .unwrap();
        assert_eq!(derived.len(), 16);
        assert!(derived.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A cross-host 3xx from stock OpenAI must not carry the generated codex
    /// identity headers onto the redirect target: the hardened policy stops at
    /// the hop, the relay host sees nothing, and the 307 reaches shunt to be
    /// relayed like any other upstream status.
    #[tokio::test]
    async fn a_cross_host_redirect_never_carries_the_identity_headers() {
        use crate::config::ApiKeyHeader;
        use wiremock::{
            matchers::{header, method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let stock = MockServer::start().await;
        let relay = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("session-id", "sess-redir"))
            .respond_with(
                ResponseTemplate::new(reqwest::StatusCode::TEMPORARY_REDIRECT)
                    .insert_header("location", format!("{}/v1/responses", relay.uri())),
            )
            .expect(1)
            .mount(&stock)
            .await;

        let client = reqwest::Client::builder()
            .redirect(crate::adapters::responses::request::codex_identity_redirect_policy())
            .resolve("api.openai.com", *stock.address())
            .build()
            .unwrap();

        let mut config = Config::default();
        let openai = config.providers.get_mut("openai").unwrap();
        openai.base_url = "http://api.openai.com/v1".to_string();
        let state = AppState::new(config, client).unwrap();
        let route = Route {
            provider: "openai".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.6-sol".to_string(),
            upstream_model: "gpt-5.6-sol".to_string(),
            effort: None,
            service_tier: None,
        };
        let credential = Credential::ApiKey {
            value: "openai-key".to_string(),
            header: ApiKeyHeader::Bearer,
        };
        let response = request_builder(&state, &route, credential, Some("sess-redir"), None, 0)
            .body("{}")
            .send()
            .await
            .expect("the redirect stops at the hop and returns the 307");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT,
            "a refused cross-host hop returns the upstream redirect"
        );
        assert!(
            relay.received_requests().await.unwrap().is_empty(),
            "the generated identity headers must not follow a cross-host redirect"
        );
        stock.verify().await;
    }

    /// Same guard, subscription OAuth arm: a ChatGPT-backend turn carries the
    /// account id on every request and the four codex identity headers
    /// whenever a session id exists, so a cross-host 3xx from it stops at the
    /// hop exactly like stock OpenAI's, while a same-domain hop still follows.
    /// The policy is the unit here: the production client wiring in server.rs
    /// is not driven through a 3xx by the suite, and a loopback chatgpt-oauth
    /// origin would be unguarded anyway, so the origin host is pinned to the
    /// mock with `resolve`.
    #[tokio::test]
    async fn a_cross_host_redirect_from_the_chatgpt_backend_never_carries_the_identity_headers() {
        use wiremock::{
            matchers::{header, method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let backend = MockServer::start().await;
        let relay = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .and(header("session-id", "sess-redir"))
            .respond_with(
                ResponseTemplate::new(reqwest::StatusCode::TEMPORARY_REDIRECT).insert_header(
                    "location",
                    format!("{}/backend-api/codex/responses", relay.uri()),
                ),
            )
            .expect(1)
            .mount(&backend)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/follow"))
            .and(header("session-id", "sess-follow"))
            .respond_with(
                ResponseTemplate::new(reqwest::StatusCode::TEMPORARY_REDIRECT).insert_header(
                    "location",
                    "http://chatgpt.com/backend-api/landed".to_string(),
                ),
            )
            .expect(1)
            .mount(&backend)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/landed"))
            .respond_with(ResponseTemplate::new(reqwest::StatusCode::OK))
            .expect(1)
            .mount(&backend)
            .await;

        let client = reqwest::Client::builder()
            .redirect(crate::adapters::responses::request::codex_identity_redirect_policy())
            .resolve("chatgpt.com", *backend.address())
            .build()
            .unwrap();

        let response = client
            .post("http://chatgpt.com/backend-api/codex/responses")
            .header("session-id", "sess-redir")
            .header("thread-id", "sess-redir")
            .header("chatgpt-account-id", "acct-redir")
            .body("{}")
            .send()
            .await
            .expect("the redirect stops at the hop and returns the 307");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT,
            "a refused cross-host hop returns the upstream redirect"
        );
        let followed = client
            .post("http://chatgpt.com/backend-api/codex/follow")
            .header("session-id", "sess-follow")
            .header("chatgpt-account-id", "acct-follow")
            .body("{}")
            .send()
            .await
            .expect("the same-domain hop follows to the landing path");
        assert_eq!(
            followed.status(),
            reqwest::StatusCode::OK,
            "a same-domain hop follows to the landing path"
        );
        assert!(
            relay.received_requests().await.unwrap().is_empty(),
            "the identity headers and account id must not follow a cross-host redirect"
        );
        backend.verify().await;
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
            None,
            0,
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
        assert_eq!(request.headers().get("session-id").unwrap(), "session-123");
        assert_eq!(request.headers().get("thread-id").unwrap(), "session-123");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "session-123:0"
        );
        // The old underscore spelling is not part of the Codex CLI header set.
        assert!(request.headers().get("session_id").is_none());
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
        assert!(request.headers().get("session-id").is_none());
        assert!(request.headers().get("thread-id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    fn openai_api_key() -> Credential {
        Credential::ApiKey {
            value: "api-key".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }
    }

    fn openai_route() -> Route {
        Route {
            provider: "openai".to_string(),
            ..codex_route()
        }
    }

    #[test]
    fn api_key_requests_to_stock_openai_carry_the_session_affinity_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &openai_route(),
            openai_api_key(),
            Some("session-123"),
        );

        // Stock OpenAI is the one api-key Responses upstream codex sends its
        // session-affinity headers to; the values match the OAuth branch's.
        assert_eq!(request.headers().get("session-id").unwrap(), "session-123");
        assert_eq!(request.headers().get("thread-id").unwrap(), "session-123");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "session-123:0"
        );
        // The subagent markers are a codex-backend shape; stock OpenAI never
        // carries them, delegated turn or not.
        assert!(request.headers().get("x-codex-parent-thread-id").is_none());
        assert!(request.headers().get("x-openai-subagent").is_none());
        // Even handed a delegation, the api-key arm ignores it.
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-7"),
        );
        let delegation = codex_delegation(&headers, "session-123").unwrap();
        let delegated = request_builder(
            &state,
            &openai_route(),
            openai_api_key(),
            Some("session-123"),
            Some(&delegation),
            0,
        )
        .body("{}")
        .build()
        .expect("test request should build");
        assert!(delegated
            .headers()
            .get("x-codex-parent-thread-id")
            .is_none());
        assert!(delegated.headers().get("x-openai-subagent").is_none());
        assert_eq!(
            delegated.headers().get("thread-id").unwrap(),
            "session-123",
            "no child split on stock OpenAI"
        );
        assert!(request.headers().get("accept").is_none());
    }

    #[test]
    fn api_key_requests_without_a_session_id_omit_the_affinity_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        for session_id in [None, Some("")] {
            let request = build_test_request(&state, &openai_route(), openai_api_key(), session_id);
            assert!(request.headers().get("session-id").is_none());
            assert!(request.headers().get("thread-id").is_none());
            assert!(request.headers().get("x-client-request-id").is_none());
            assert!(request.headers().get("x-codex-window-id").is_none());
        }
    }

    #[test]
    fn api_key_requests_to_xai_omit_the_affinity_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request =
            build_test_request(&state, &xai_route(), openai_api_key(), Some("session-123"));

        assert!(request.headers().get("session-id").is_none());
        assert!(request.headers().get("thread-id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    #[test]
    fn api_key_requests_off_the_stock_openai_host_omit_the_affinity_headers() {
        // The stock-host gate is exact: a third-party relay and an openai.com
        // subdomain both stay header-free, so widening the host predicate past
        // `api.openai.com` exactly reddens this test.
        for base_url in ["https://relay.example/v1", "https://chat.openai.com/v1"] {
            let mut config = Config::default();
            config.providers.get_mut("openai").unwrap().base_url = base_url.to_string();
            let state = AppState::new(config, reqwest::Client::new()).unwrap();

            let request = build_test_request(
                &state,
                &openai_route(),
                openai_api_key(),
                Some("session-123"),
            );

            assert!(request.headers().get("session-id").is_none());
            assert!(request.headers().get("thread-id").is_none());
            assert!(request.headers().get("x-client-request-id").is_none());
            assert!(request.headers().get("x-codex-window-id").is_none());
        }
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
        assert!(request.headers().get("session-id").is_none());
        assert!(request.headers().get("thread-id").is_none());
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
