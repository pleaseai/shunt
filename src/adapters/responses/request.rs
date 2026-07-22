//! Build the upstream Responses HTTP request (URL, auth, identity headers) and
//! resolve the per-provider Responses endpoint.

use crate::{auth::Credential, routing::Route, server::AppState};

/// Codex CLI client identity, mirrored from openai/codex rust-v0.144.4.
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
pub(super) const CODEX_USER_AGENT: &str = "codex_cli_rs/0.144.4";
pub(super) const CODEX_CLIENT_VERSION: &str = "0.144.4";

/// Grok CLI identity, mirrored from the official Grok CLI (via
/// raine/claude-code-proxy `src/providers/grok/client.rs`). The subscription
/// surface (`cli-chat-proxy.grok.com`) gates on these headers: without them it
/// answers as if the caller were an unentitled API client. Sent only with the
/// `XaiOauth` (subscription bearer) credential.
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";
const GROK_CLIENT_VERSION: &str = "0.2.93";

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
        Credential::CursorOauth { .. } | Credential::Passthrough => {}
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

    use super::{build_test_request, responses_url};

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
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
        }
    }

    fn grok_route() -> Route {
        Route {
            provider: "grok".to_string(),
            adapter: AdapterKind::Responses,
            model: "grok-4.5".to_string(),
            upstream_model: "grok-4.5".to_string(),
            effort: None,
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
}
