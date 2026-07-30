//! Prepare the upstream Responses request body: serialize the translated request
//! once per turn and, on the ChatGPT/Codex backend, zstd-compress it (issue #285).
//!
//! Split from the transport itself ([`super::http`]) because both the
//! single-credential path and the account-pool path prepare a body the same way
//! and then reuse it across every attempt — the preparation is a turn-scoped
//! concern, not a per-send one.

use crate::{routing::Route, server::AppState};

/// The upstream request body for one HTTP send: the bytes to put on the wire and
/// whether they are zstd-compressed (and therefore need `content-encoding: zstd`).
/// Cloning is a refcount bump, so every retry attempt, account rotation, and
/// refresh retry reuses one preparation (see [`prepare_body`]).
#[derive(Debug, Clone)]
pub(super) struct PreparedBody {
    bytes: bytes::Bytes,
    zstd: bool,
}

impl PreparedBody {
    fn plain(bytes: bytes::Bytes) -> Self {
        Self { bytes, zstd: false }
    }

    /// Attach these bytes to `request`, announcing the content coding when they
    /// are compressed. Keeping this here rather than in the transport means the
    /// header and the bytes it describes can never be set independently.
    pub(super) fn attach(self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = if self.zstd {
            // Exactly what the Codex CLI sends alongside a compressed Responses
            // request body (codex-rs/http-client/src/request.rs).
            request.header("content-encoding", "zstd")
        } else {
            request
        };
        request.body(self.bytes)
    }
}

/// Serialize the translated request and, on the ChatGPT/Codex backend,
/// zstd-compress it (issue #285) — the same wire shape the Codex CLI sends. A
/// long agentic turn re-uploads its whole history every request, and JSON of that
/// shape compresses several times over, so the saving grows with the conversation.
///
/// Called at most once per turn, before the bounded-retry and account-rotation
/// loops, so neither a retry nor a rotation repeats the work (the same
/// serialize-once discipline as issue #251). A compression failure is not fatal:
/// the uncompressed body is always acceptable to the backend, so it is logged and
/// sent as-is rather than failing the turn.
pub(super) async fn prepare_body(
    state: &AppState,
    route: &Route,
    upstream_body: &serde_json::Value,
) -> PreparedBody {
    // `to_vec` serializes straight into the byte buffer `Bytes` takes ownership
    // of, skipping the `fmt` machinery and UTF-8 round-trip `Value::to_string()`
    // pays for. Serializing a `Value` cannot fail, so the fallback is
    // unreachable — it just keeps the old path.
    let bytes = serde_json::to_vec(upstream_body)
        .map(bytes::Bytes::from)
        .unwrap_or_else(|_| bytes::Bytes::from(upstream_body.to_string()));
    if !state.config.responses_request_compression(&route.provider) {
        return PreparedBody::plain(bytes);
    }
    match crate::compression::compress_request_body(bytes.clone()).await {
        Ok(Some(compressed)) => {
            tracing::debug!(
                provider = %route.provider,
                body_bytes = bytes.len(),
                compressed_bytes = compressed.len(),
                "compressed responses request body"
            );
            PreparedBody {
                bytes: compressed,
                zstd: true,
            }
        }
        // Below the size where compression pays for itself.
        Ok(None) => PreparedBody::plain(bytes),
        Err(error) => {
            tracing::warn!(
                provider = %route.provider,
                body_bytes = bytes.len(),
                error = %error,
                "failed to compress responses request body; sending it uncompressed"
            );
            PreparedBody::plain(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::http::http_send;
    use super::*;
    use crate::{auth::Credential, config::Config, routing::AdapterKind};
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A translated request body large enough to be worth compressing (a real
    /// turn's instructions and history are far larger still).
    pub(super) fn upstream_body() -> Value {
        serde_json::json!({
            "model": "gpt-5.2-codex",
            "instructions": "be brief".repeat(200),
            "input": [{"role": "user", "content": "hello"}],
            "stream": true,
        })
    }

    fn route_for(provider: &str) -> Route {
        Route {
            provider: provider.to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
        }
    }

    fn state_for(config: Config) -> AppState {
        AppState::new(config, reqwest::Client::new()).expect("state should build")
    }

    /// Point `provider`'s base_url at a mock server so a prepared body can be
    /// sent through the real transport and inspected off the wire.
    fn state_at(provider: &str, base_url: String) -> AppState {
        let mut config = Config::default();
        config
            .providers
            .get_mut(provider)
            .expect("built-in provider should exist")
            .base_url = base_url;
        state_for(config)
    }

    /// Serve a `200` from `path` and return the single request the mock recorded.
    async fn record_send(
        provider: &str,
        endpoint: &str,
        credential: Credential,
    ) -> wiremock::Request {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_string(String::new()))
            .mount(&server)
            .await;
        let state = state_at(provider, server.uri());
        let route = route_for(provider);
        let body = prepare_body(&state, &route, &upstream_body()).await;
        http_send(&state, &route, credential, None, body)
            .await
            .expect("mock request should succeed");
        server
            .received_requests()
            .await
            .expect("mock server should record requests")
            .pop()
            .expect("exactly one request should have been sent")
    }

    /// The ChatGPT/Codex backend gets the same wire shape real Codex sends: a
    /// zstd-compressed body announced with `content-encoding: zstd` (issue #285).
    #[tokio::test]
    async fn compresses_the_request_body_on_the_chatgpt_backend() {
        let request = record_send(
            "codex",
            "/codex/responses",
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
        )
        .await;

        assert_eq!(
            request
                .headers
                .get("content-encoding")
                .expect("a compressed body must announce its encoding"),
            "zstd"
        );
        let plain = serde_json::to_vec(&upstream_body()).unwrap();
        assert!(
            request.body.len() < plain.len(),
            "compressed body ({}) should be smaller than the JSON ({})",
            request.body.len(),
            plain.len()
        );
        // The upstream must be able to recover the exact translated request.
        let decoded = crate::compression::decode_zstd_within(
            bytes::Bytes::from(request.body.clone()),
            plain.len(),
        )
        .await
        .expect("the sent body should be valid zstd")
        .expect("the decoded body should fit its own size");
        assert_eq!(decoded, plain);
    }

    /// No other flavor has been verified to accept a compressed request body, so
    /// a stock OpenAI provider keeps sending plain JSON and no encoding header.
    #[tokio::test]
    async fn leaves_the_request_body_uncompressed_on_other_flavors() {
        let request = record_send(
            "openai",
            "/responses",
            Credential::ApiKey {
                value: "sk-test".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
        )
        .await;

        assert!(request.headers.get("content-encoding").is_none());
        assert_eq!(request.body, serde_json::to_vec(&upstream_body()).unwrap());
    }

    /// The ChatGPT/Codex flavor compresses, and the bytes the upstream receives
    /// decode back to exactly the translated request.
    #[tokio::test]
    async fn compresses_on_the_chatgpt_flavor() {
        let state = state_for(Config::default());
        let body = prepare_body(&state, &route_for("codex"), &upstream_body()).await;

        assert!(body.zstd);
        let plain = serde_json::to_vec(&upstream_body()).unwrap();
        assert!(
            body.bytes.len() < plain.len(),
            "compressed body ({}) should be smaller than the JSON ({})",
            body.bytes.len(),
            plain.len()
        );
        let decoded = crate::compression::decode_zstd_within(body.bytes, plain.len())
            .await
            .expect("the prepared body should be valid zstd")
            .expect("the decoded body should fit its own size");
        assert_eq!(decoded, plain);
    }

    /// No other flavor is verified to accept a compressed request body, so they
    /// keep sending plain JSON.
    #[tokio::test]
    async fn leaves_other_flavors_uncompressed() {
        let state = state_for(Config::default());
        for provider in ["openai", "xai", "grok"] {
            let body = prepare_body(&state, &route_for(provider), &upstream_body()).await;
            assert!(!body.zstd, "{provider} should not be compressed");
            assert_eq!(body.bytes, serde_json::to_vec(&upstream_body()).unwrap());
        }
    }

    /// The per-provider opt-out returns the ChatGPT path to plain JSON.
    #[tokio::test]
    async fn honors_the_per_provider_opt_out() {
        let mut config = Config::default();
        config
            .providers
            .get_mut("codex")
            .expect("built-in provider should exist")
            .request_compression = false;
        let state = state_for(config);

        let body = prepare_body(&state, &route_for("codex"), &upstream_body()).await;
        assert!(!body.zstd);
        assert_eq!(body.bytes, serde_json::to_vec(&upstream_body()).unwrap());
    }
}
