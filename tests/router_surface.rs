//! Cross-surface router invariants: every optional surface can be enabled at
//! once, and the registered path set stays exactly the one
//! `docs/admin-ui-delivery.md` documents.
//!
//! `axum::Router::merge` panics at boot on a duplicate path+method, which is a
//! real safety net for a newly added route — but it only fires when both trees
//! are registered, and until now no test built a router with `[server.admin]`,
//! `[server.gateway]`, `[server.spend]`, `[server.codex_endpoint]`,
//! `[server.usage]`, and `[server.oauth_usage]` all present at once. A
//! collision between two optional surfaces would therefore have surfaced on an
//! operator's machine rather than in CI. That gap is recorded under "Risks" in
//! `docs/admin-ui-delivery.md`; these tests close it *before* any UI route
//! claims a path, which is the order that document's "Testing" section asks for.
//!
//! **How path existence is probed.** `Router` exposes no route table, so the
//! inventory is read back with a method no shunt route registers. axum answers
//! `405` when the path matches a route registered for other methods and `404`
//! when no route matches it at all — the very path-vs-method distinction
//! Decision 3 of that document reasons about, used here as an oracle. The one
//! subtlety is that `spend_router` attaches its own `fallback` to each
//! `MethodRouter`; that fallback also answers `405`, so the oracle holds
//! uniformly.

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use shunt::{
    config::{
        AccountConfig, AdminConfig, AuthMode, CodexEndpointConfig, Config, GatewayConfig,
        InboundAuthConfig, OauthUsageConfig, SpendConfig, UsageEndpointConfig,
    },
    server,
};
use tower::ServiceExt;

/// A method no route in the crate registers, so a response distinguishes
/// "path exists" (`405`) from "path does not exist" (`404`).
const UNREGISTERED_METHOD: Method = Method::PATCH;

/// Every path the router registers with all optional surfaces enabled, grouped
/// by the config table that gates it. This list **is** the inventory assertion:
/// adding a route without adding it here fails
/// `no_undocumented_path_is_registered`, which is what forces the conflict
/// review `docs/admin-ui-delivery.md` asks for.
///
/// Paths are written **exactly as the source registers them**, placeholders
/// included, so `every_registered_literal_path_is_documented` can compare
/// against them directly; [`probe_path`] substitutes a concrete segment for the
/// runtime probes.
const BASE_PATHS: [&str; 7] = [
    "/",
    "/health",
    "/protocol",
    "/v1/models",
    "/routes",
    "/v1/messages",
    "/v1/messages/count_tokens",
];

/// 16 paths / 18 method+path pairs — the count `docs/admin-ui-delivery.md`
/// records in its "Current surface" table.
const ADMIN_PATHS: [&str; 16] = [
    "/admin",
    "/admin/login",
    "/admin/oidc/start",
    "/admin/oidc/callback",
    "/admin/logout",
    "/admin/accounts",
    "/admin/observed",
    "/admin/pool",
    "/admin/status",
    "/admin/accounts/claude",
    "/admin/accounts/claude/{name}/complete",
    "/admin/accounts/claude/{name}/refresh",
    "/admin/accounts/claude/{name}",
    "/admin/accounts/codex",
    "/admin/accounts/codex/{name}/complete",
    "/admin/accounts/codex/{name}",
];

const GATEWAY_PATHS: [&str; 10] = [
    "/.well-known/oauth-authorization-server",
    "/oauth/device_authorization",
    "/oauth/token",
    "/device",
    "/device/authorize",
    "/device/callback",
    "/managed/settings",
    "/v1/metrics",
    "/v1/logs",
    "/v1/traces",
];

const SPEND_PATHS: [&str; 2] = [
    "/v1/organizations/spend_limits",
    "/v1/organizations/spend_limits/{id}",
];

/// Mirrors `codex_endpoint::PATHS` and `codex_analytics::PATHS`, which are
/// `pub(crate)` and so cannot be imported here. Duplicating them is deliberate:
/// a change to either constant must fail this test and be re-reviewed against
/// the path split, which is exactly the guard being installed.
const CODEX_ENDPOINT_PATHS: [&str; 5] = [
    "/backend-api/codex/responses",
    "/responses",
    "/v1/responses",
    "/backend-api/codex/analytics-events/events",
    "/codex/analytics-events/events",
];

const USAGE_PATHS: [&str; 2] = ["/usage", "/api/oauth/usage"];

/// Substitute a concrete segment for each path placeholder, so a template from
/// the inventory can be sent as a real request.
fn probe_path(template: &str) -> String {
    template.replace("{name}", "acct").replace("{id}", "spl_1")
}

fn all_registered_paths() -> Vec<&'static str> {
    BASE_PATHS
        .iter()
        .chain(ADMIN_PATHS.iter())
        .chain(GATEWAY_PATHS.iter())
        .chain(SPEND_PATHS.iter())
        .chain(CODEX_ENDPOINT_PATHS.iter())
        .chain(USAGE_PATHS.iter())
        .copied()
        .collect()
}

/// `Config::default()` with **every** optional surface enabled at once.
///
/// Env-backed credentials get per-process-unique names because the process
/// environment is shared across the test binary: a fixed name would let one
/// test's value satisfy another test's config by accident.
///
/// Both `state_path` values are set to an empty path — the documented opt-out
/// — so building a router never reads or writes the operator's real
/// `~/.shunt` state.
fn all_surfaces_config(label: &str) -> Config {
    let suffix = format!("{}_{label}", std::process::id());
    let admin_env = format!("SHUNT_ROUTER_SURFACE_ADMIN_{suffix}");
    let client_env = format!("SHUNT_ROUTER_SURFACE_CLIENT_{suffix}");
    let jwt_env = format!("SHUNT_ROUTER_SURFACE_JWT_{suffix}");
    let users_env = format!("SHUNT_ROUTER_SURFACE_USERS_{suffix}");
    std::env::set_var(&admin_env, "admin:admin-secret");
    std::env::set_var(&client_env, "tester:client-secret");
    std::env::set_var(&jwt_env, "0123456789abcdef0123456789abcdef");
    std::env::set_var(&users_env, "dev@example.com:password");

    let mut config = Config::default();

    // `[server.auth]` is not itself a route, but `[server.usage]` requires it.
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env,
    });

    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: admin_env,
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        oidc: None,
    });

    config.server.gateway = Some(GatewayConfig {
        public_url: "https://gateway.example".to_string(),
        jwt_secret_env: Some(jwt_env),
        users_env,
        token_ttl_seconds: Some(3600),
        trust_forwarded_for: false,
        policies: None,
        telemetry: None,
        state_path: Some(std::path::PathBuf::new()),
        oidc: None,
        session: None,
    });

    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        ..SpendConfig::default()
    });

    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: Vec::new(),
    });

    config.server.usage = Some(UsageEndpointConfig::default());
    config.server.oauth_usage = Some(OauthUsageConfig::default());

    // A pool account is only valid on an OAuth provider, and giving `codex` one
    // explicit account keeps router construction off the real account store.
    let anthropic = config
        .providers
        .get_mut("anthropic")
        .expect("built-in anthropic provider");
    anthropic.auth = AuthMode::ClaudeOauth;
    let codex = config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider");
    codex.accounts = vec![AccountConfig {
        name: "acct".to_string(),
        ..AccountConfig::default()
    }];

    config
}

/// `true` when the router has any route registered at `path`.
async fn path_is_registered(router: &Router, path: &str) -> bool {
    let request = Request::builder()
        .method(UNREGISTERED_METHOD)
        .uri(path)
        .body(Body::empty())
        .expect("probe request builds");
    let status = router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers the probe")
        .status();
    match status {
        StatusCode::METHOD_NOT_ALLOWED => true,
        StatusCode::NOT_FOUND => false,
        other => panic!(
            "probing {path} with {UNREGISTERED_METHOD} answered {other}; the 404-vs-405 oracle \
             only holds while no route accepts that method and no layer answers ahead of routing"
        ),
    }
}

/// The gap recorded under "Risks" in `docs/admin-ui-delivery.md`: `Router::merge`
/// panics on a duplicate path+method, but only when both trees are registered.
#[tokio::test]
async fn every_optional_surface_can_be_enabled_at_once() {
    let config = all_surfaces_config("builds");
    let (_router, _shared, _state) =
        server::build_router(config).expect("a config enabling every optional surface builds");
}

#[tokio::test]
async fn every_documented_path_is_registered_when_all_surfaces_are_enabled() {
    let (router, _shared, _state) =
        server::build_router(all_surfaces_config("registered")).expect("router builds");

    for path in all_registered_paths() {
        assert!(
            path_is_registered(&router, &probe_path(path)).await,
            "{path} is documented as registered but no route answers it"
        );
    }
}

/// The other half of the inventory: paths adjacent to real ones, and the
/// namespace reserved for a later milestone, must still be absent. Without this
/// the test above would pass just as well against a catch-all fallback.
#[tokio::test]
async fn no_undocumented_path_is_registered() {
    let (router, _shared, _state) =
        server::build_router(all_surfaces_config("undocumented")).expect("router builds");

    // Deliberately near-misses of registered paths, plus `/v1/organizations/*`
    // members shunt does not implement (`docs/admin-ui-delivery.md` reserves
    // that namespace for the Anthropic-shaped Admin API).
    for path in [
        "/nope",
        "/v1/nope",
        "/admin/nope",
        "/admin/accounts/gemini",
        "/admin/accounts/codex/acct/refresh",
        "/oauth/authorize",
        "/v1/organizations/spend_limits/spl_1/effective",
        "/v1/organizations/spend_limits/spl_1/audit",
    ] {
        assert!(
            !path_is_registered(&router, path).await,
            "{path} answers but is not in the documented inventory"
        );
    }
}

/// `/` is a liveness probe target as well as a landing page, so any UI work
/// that later claims a path must leave its `HEAD` answer intact.
#[tokio::test]
async fn root_still_answers_head_with_every_surface_enabled() {
    let (router, _shared, _state) =
        server::build_router(all_surfaces_config("head")).expect("router builds");

    let request = Request::builder()
        .method(Method::HEAD)
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Every router source file that registers a path as a string literal. Scanned
/// at compile time, so this test needs no filesystem access at runtime.
const ROUTER_SOURCES: [(&str, &str); 4] = [
    ("src/server.rs", include_str!("../src/server.rs")),
    ("src/admin/mod.rs", include_str!("../src/admin/mod.rs")),
    ("src/gateway/mod.rs", include_str!("../src/gateway/mod.rs")),
    (
        "src/gateway/spend/mod.rs",
        include_str!("../src/gateway/spend/mod.rs"),
    ),
];

/// Extract the literal first argument of every `.route("…"` call in `source`.
fn registered_literal_paths(source: &str) -> Vec<&str> {
    source
        .match_indices(".route(")
        .filter_map(|(index, marker)| {
            let rest = source[index + marker.len()..].trim_start();
            // A non-literal first argument (`telemetry_ingest::Signal::…path()`,
            // or the `path` loop variable in `server.rs`) is out of this scan's
            // reach by construction; those paths are covered by the runtime
            // probes above instead.
            let literal = rest.strip_prefix('"')?;
            let end = literal.find('"')?;
            Some(&literal[..end])
        })
        .collect()
}

/// The half of the inventory the runtime probes cannot supply: `Router` exposes
/// no route table, so a route added at a path this file has never heard of
/// would answer requests without failing any probe. Reading the registrations
/// back out of the source closes that, which is what makes the inventory an
/// actual gate rather than a spot check.
///
/// Residual gap, stated rather than hidden: a route whose path is not a string
/// literal at the call site is invisible here. There are two such sites today —
/// the OTLP signals in `gateway_router` and the `codex_endpoint::PATHS` /
/// `codex_analytics::PATHS` loops in `build_router` — and both are pinned by
/// `every_documented_path_is_registered_when_all_surfaces_are_enabled`.
#[test]
fn every_registered_literal_path_is_documented() {
    let inventory = all_registered_paths();
    for (file, source) in ROUTER_SOURCES {
        for path in registered_literal_paths(source) {
            assert!(
                inventory.contains(&path),
                "{file} registers {path}, which is not in this test's inventory. Adding a route \
                 means adding it here too — and, per docs/admin-ui-delivery.md, reviewing it \
                 against the /admin, /admin/api and reserved /v1/organizations path split first."
            );
        }
    }
}

/// The scan is only a gate while it actually finds the registrations; a
/// refactor that changed the `.route("…"` spelling would otherwise leave
/// `every_registered_literal_path_is_documented` vacuously green.
#[test]
fn the_source_scan_finds_every_literal_registration() {
    let found: usize = ROUTER_SOURCES
        .iter()
        .map(|(_, source)| registered_literal_paths(source).len())
        .sum();
    // 9 in `server.rs` (7 base + `/usage` + `/api/oauth/usage`), 16 admin,
    // 7 gateway (its 3 OTLP paths come from `Signal::path()`), 2 spend.
    assert_eq!(
        found, 34,
        "the literal-path scan found {found} registrations, not 34; either a route was added or \
         removed, or `.route(\"…\"` is no longer how they are spelled"
    );
}
