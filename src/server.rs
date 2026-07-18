use std::sync::Arc;

use axum::{
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::{
    accounts::AccountPool,
    admin::{self, AdminAuth, AdminStores},
    auth::inbound::InboundAuth,
    codex_endpoint,
    config::{Config, ConfigError},
    discovery,
    gateway::{self, GatewayAuth, GatewayStores},
    protocol, proxy,
    reload::{RuntimeState, SharedState},
    routes, usage,
};

#[derive(Clone)]
pub struct AppState {
    /// Per-request config snapshot (see [`AppState::refreshed`]).
    pub config: Arc<Config>,
    pub http_client: reqwest::Client,
    pub accounts: Arc<AccountPool>,
    /// Inbound client-token auth snapshot for this request (None ⇒ open).
    pub inbound_auth: Option<Arc<InboundAuth>>,
    /// Admin-surface auth snapshot for this request (None ⇒ admin disabled).
    /// Re-snapshotted per request so token/header edits hot-apply.
    pub admin_auth: Option<Arc<AdminAuth>>,
    /// Process-lifetime admin session/pending/rate-limit stores. Like
    /// [`AppState::accounts`], created once and kept across reloads so an
    /// operator's browser session is not dropped by an unrelated config edit.
    pub admin_stores: Arc<AdminStores>,
    /// Gateway-login JWT/approval snapshot for this request (None ⇒ disabled).
    pub gateway_auth: Option<Arc<GatewayAuth>>,
    /// Process-lifetime device grants, IdP states/cache, refresh tokens, and limits.
    pub gateway_stores: Arc<GatewayStores>,
    /// The live, hot-swappable runtime state a reload updates. Private so the
    /// only way in is a snapshot method that keeps `config`/`inbound_auth`/
    /// `admin_auth` consistent with it.
    shared: SharedState,
}

impl AppState {
    /// Build state from a config, owning a fresh shared store. Used by tests and
    /// by callers that do not thread an external [`SharedState`].
    pub fn new(config: Config, http_client: reqwest::Client) -> Result<Self, ConfigError> {
        let runtime = RuntimeState::from_config(config)?;
        let shared: SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(runtime));
        Ok(Self::from_shared(
            shared,
            http_client,
            Arc::new(AccountPool::new()),
            Arc::new(AdminStores::new()),
            Arc::new(GatewayStores::new()),
        ))
    }

    /// Snapshot the current runtime state from an existing shared store.
    pub fn from_shared(
        shared: SharedState,
        http_client: reqwest::Client,
        accounts: Arc<AccountPool>,
        admin_stores: Arc<AdminStores>,
        gateway_stores: Arc<GatewayStores>,
    ) -> Self {
        let current = shared.load();
        Self {
            config: current.config.clone(),
            inbound_auth: current.inbound_auth.clone(),
            admin_auth: current.admin_auth.clone(),
            gateway_auth: current.gateway_auth.clone(),
            http_client,
            accounts,
            admin_stores,
            gateway_stores,
            shared,
        }
    }

    /// Re-snapshot the live shared state into a new `AppState`, so a request
    /// entry picks up the latest reloaded config while holding one stable
    /// snapshot for the whole request. Cheap: clones `Arc`s and the client.
    pub(crate) fn refreshed(&self) -> Self {
        Self::from_shared(
            self.shared.clone(),
            self.http_client.clone(),
            self.accounts.clone(),
            self.admin_stores.clone(),
            self.gateway_stores.clone(),
        )
    }
}

/// Build the router and return it alongside the [`SharedState`] it reads and a
/// clone of the request [`AppState`], so the caller can spawn reload watchers
/// that hot-swap the same store and background tasks (the usage poller) that
/// share the same [`AccountPool`] the request handlers use.
pub fn build_router(config: Config) -> Result<(Router, SharedState, AppState), ConfigError> {
    // Whether the admin surface exists is decided once here, from the initial
    // config: a reload cannot add or drop routes (it only re-resolves tokens).
    let admin_enabled = config.server.admin.is_some();
    // Gateway-login routes are likewise fixed at boot; signing/user edits are
    // re-resolved through `gateway_auth`, while toggling requires a restart.
    let gateway_enabled = config.server.gateway.is_some();
    // The inbound Responses (Codex) routes are likewise registered once from the
    // initial config; a reload can only change the target provider, not add or
    // drop the routes.
    let codex_endpoint_enabled = config.server.codex_endpoint.is_some();
    // The client-facing usage endpoint (`GET /usage`) is likewise registered once
    // from the initial config; a reload only re-resolves the client tokens it
    // authenticates against, it cannot add or drop the route.
    let usage_enabled = config.server.usage.is_some();
    let runtime = RuntimeState::from_config(config)?;
    let shared: SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(runtime));
    let state = AppState::from_shared(
        shared.clone(),
        reqwest::Client::new(),
        Arc::new(AccountPool::new()),
        Arc::new(AdminStores::new()),
        Arc::new(GatewayStores::new()),
    );

    // `/` and `/health` stay unauthenticated even when `[server.auth]` is
    // configured (healthcheck tools rarely carry tokens); they must never
    // expose config, credentials, or upstream details — only version, status,
    // and the already-public endpoint list. Discovery handlers enforce their
    // own endpoint-specific auth policy against the same refreshed state.
    let mut router = Router::new()
        .route("/", get(root_index))
        .route("/health", get(health))
        .route("/protocol", get(protocol::get))
        .route("/v1/models", get(discovery::get))
        .route("/routes", get(routes::get))
        .route("/v1/messages", post(proxy::post))
        .route("/v1/messages/count_tokens", post(proxy::post));

    // Opt-in admin surface (M9): registered only when `[server.admin]` is set,
    // so the default HTTP surface is unchanged. Its handlers authenticate every
    // request against the separate `[server.admin]` credential.
    if admin_enabled {
        router = router.merge(admin::admin_router());
    }

    // Opt-in Claude apps gateway surface (M-A/M-B): registered only when
    // `[server.gateway]` was present at boot. OAuth handlers remain unauthenticated;
    // minted JWTs authenticate managed settings and injected-credential inference.
    // Static client tokens are a separate alternative, and `/v1/models` keeps its
    // existing endpoint-specific authentication behavior.
    if gateway_enabled {
        router = router.merge(gateway::gateway_router());
    }

    // Opt-in inbound Responses (Codex) endpoint: registered only when
    // `[server.codex_endpoint]` is set, so the default HTTP surface is unchanged.
    // The Codex CLI appends `/responses` to whatever base_url it is pointed at, so
    // all three paths (the ChatGPT-backend mirror plus the custom-provider forms)
    // map to one passthrough handler. Gated by `[server.auth]` like the other
    // injected-credential routes.
    if codex_endpoint_enabled {
        router = router
            .route("/backend-api/codex/responses", post(codex_endpoint::post))
            .route("/responses", post(codex_endpoint::post))
            .route("/v1/responses", post(codex_endpoint::post));
    }

    // Opt-in client-facing usage endpoint (`GET /usage`): registered only when
    // `[server.usage]` is set, so the default HTTP surface is unchanged. The
    // handler authenticates every request against `[server.auth]` client tokens
    // (validation guarantees that table is present) and returns a sanitized,
    // aggregated pool-quota view — never per-account identity or capacity.
    if usage_enabled {
        router = router.route("/usage", get(usage::get));
    }

    // Clone the state into the router; the returned clone shares the same
    // `AccountPool`/`SharedState` Arcs, so a background poller populating quota
    // writes to the very pool the handlers read.
    Ok((router.with_state(state.clone()), shared, state))
}

/// Human-facing landing page; axum also serves HEAD `/` from this handler,
/// which keeps the pre-existing liveness probe working.
async fn root_index() -> String {
    format!(
        "shunt v{} — Anthropic Messages proxy. Endpoints: /v1/models, /routes, /v1/messages, /v1/messages/count_tokens, /protocol, /health\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

/// Machine-facing liveness endpoint: the process is up and config loaded
/// (the router cannot exist otherwise). Deliberately does not check upstream
/// connectivity — that is decided per request and would only cause flapping.
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use crate::config::{AccountConfig, Config, InboundAuthConfig, UsageEndpointConfig};

    use super::build_router;

    /// `Config::default()` with `[server.auth]` bound to a unique env var and
    /// `[server.usage]` enabled, plus the built-in `codex` provider given one
    /// explicit account so the request does not touch the account store
    /// (mirrors `usage::tests::state_with_auth_and_seeded_pool`).
    fn config_with_usage_enabled(label: &str) -> (Config, String) {
        // Per-test-unique name: tests share the process env, and one test's
        // `remove_var` must not race another's construction-time resolve.
        let env = format!("SHUNT_SERVER_TEST_TOKENS_{}_{label}", std::process::id());
        std::env::set_var(&env, "tester:tok-secret");
        let mut config = Config::default();
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".to_string(),
            tokens_env: env.clone(),
        });
        config.server.usage = Some(UsageEndpointConfig::default());
        config
            .providers
            .get_mut("codex")
            .expect("built-in codex provider")
            .accounts = vec![AccountConfig {
            name: "acct-a".to_string(),
            ..AccountConfig::default()
        }];
        (config, env)
    }

    #[tokio::test]
    async fn usage_route_is_registered_and_answers_when_enabled_with_valid_auth() {
        let (config, env) = config_with_usage_enabled("registered");
        let (router, _shared, _state) = build_router(config).unwrap();

        let request = Request::builder()
            .uri("/usage")
            .header("x-api-key", "tok-secret")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        std::env::remove_var(&env);

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn usage_route_is_404_when_server_usage_is_not_configured() {
        let (router, _shared, _state) = build_router(Config::default()).unwrap();

        let request = Request::builder()
            .uri("/usage")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
