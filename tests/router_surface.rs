//! Cross-surface router invariants: every optional surface can be enabled at
//! once, and the registered method+path set stays exactly the one
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
//! **How the router is probed.** `Router` exposes no route table, so the
//! inventory is read back with a method no shunt route registers. axum answers
//! `405` when the path matches a route registered for other methods and `404`
//! when no route matches it at all — the very path-vs-method distinction
//! Decision 3 of that document reasons about, used here as an oracle. The one
//! subtlety is that `spend_router` attaches its own `fallback` to each
//! `MethodRouter`; that fallback also answers `405`, so the oracle holds
//! uniformly.
//!
//! That same `405` carries an `Allow` header listing exactly the methods the
//! path *is* registered for, so the one probe that answers "does this path
//! exist" also answers "with which methods" — no extra request, and no handler
//! ever runs. That is what lets the inventory below pin method+path pairs
//! rather than bare paths: adding or removing a method on an existing path
//! moves the `Allow` value and fails
//! `every_registered_method_set_matches_the_inventory`.

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use shunt::{
    config::{
        AccountConfig, AdminConfig, AuthMode, CodexEndpointConfig, Config, GatewayConfig,
        InboundAuthConfig, OauthUsageConfig, SpendConfig, UsageEndpointConfig,
    },
    server,
};
use std::collections::BTreeSet;
use tower::ServiceExt;

/// A method no route in the crate registers, so a response distinguishes
/// "path exists" (`405`) from "path does not exist" (`404`).
const UNREGISTERED_METHOD: Method = Method::PATCH;

/// Every `(path, allow)` pair the router registers with all optional surfaces
/// enabled, grouped by the config table that gates it. This list **is** the
/// inventory assertion: adding a route without adding it here fails
/// `no_undocumented_path_is_registered`, which is what forces the conflict
/// review `docs/admin-ui-delivery.md` asks for.
///
/// The second element is the method set the path answers, written the way axum
/// spells it in the `Allow` header of a `405` — which means `GET` always brings
/// `HEAD` with it, because axum derives one from the other. Ordering inside the
/// string is not significant; the pairs are compared as sets.
///
/// Paths are written **exactly as the source registers them**, placeholders
/// included, so `every_registered_literal_path_is_documented` can compare
/// against them directly; [`probe_path`] substitutes a concrete segment for the
/// runtime probes.
const BASE_PATHS: [(&str, &str); 7] = [
    ("/", "GET,HEAD"),
    ("/health", "GET,HEAD"),
    ("/protocol", "GET,HEAD"),
    ("/v1/models", "GET,HEAD"),
    ("/routes", "GET,HEAD"),
    ("/v1/messages", "POST"),
    ("/v1/messages/count_tokens", "POST"),
];

/// 16 paths / 18 method+path pairs — the count `docs/admin-ui-delivery.md`
/// records in its "Current surface" table. Counting the `allow` column here
/// (ignoring the `HEAD` axum adds to every `GET`) is what reproduces the 18.
const ADMIN_PATHS: [(&str, &str); 16] = [
    ("/admin", "GET,HEAD"),
    ("/admin/login", "GET,HEAD,POST"),
    ("/admin/oidc/start", "POST"),
    ("/admin/oidc/callback", "GET,HEAD"),
    ("/admin/logout", "POST"),
    ("/admin/accounts", "GET,HEAD"),
    ("/admin/observed", "GET,HEAD"),
    ("/admin/pool", "GET,HEAD"),
    ("/admin/status", "GET,HEAD"),
    ("/admin/accounts/claude", "POST"),
    ("/admin/accounts/claude/{name}/complete", "POST"),
    ("/admin/accounts/claude/{name}/refresh", "POST"),
    ("/admin/accounts/claude/{name}", "DELETE"),
    ("/admin/accounts/codex", "GET,HEAD,POST"),
    ("/admin/accounts/codex/{name}/complete", "POST"),
    ("/admin/accounts/codex/{name}", "DELETE"),
];

const GATEWAY_PATHS: [(&str, &str); 10] = [
    ("/.well-known/oauth-authorization-server", "GET,HEAD"),
    ("/oauth/device_authorization", "POST"),
    ("/oauth/token", "POST"),
    ("/device", "GET,HEAD,POST"),
    ("/device/authorize", "POST"),
    ("/device/callback", "GET,HEAD"),
    ("/managed/settings", "GET,HEAD"),
    ("/v1/metrics", "POST"),
    ("/v1/logs", "POST"),
    ("/v1/traces", "POST"),
];

/// The two paths whose `MethodRouter` carries a custom
/// `.fallback(api::method_not_allowed)`. That fallback supplies the `405` body
/// in place of axum's own and sets no header itself
/// (`src/gateway/spend/api.rs:295`); axum attaches the `Allow` header around it
/// regardless, which the probe confirms. So these pairs are gated exactly like
/// the rest.
const SPEND_PATHS: [(&str, &str); 2] = [
    ("/v1/organizations/spend_limits", "GET,HEAD,POST"),
    ("/v1/organizations/spend_limits/{id}", "GET,HEAD,DELETE"),
];

/// Mirrors `codex_endpoint::PATHS` and `codex_analytics::PATHS`, which are
/// `pub(crate)` and so cannot be imported here. Duplicating them is deliberate:
/// `every_indirectly_registered_path_is_documented` reads both constants back
/// out of their defining source and compares them against this list, so a
/// change to either one fails this test and gets re-reviewed against the path
/// split — which is exactly the guard being installed.
const CODEX_ENDPOINT_PATHS: [(&str, &str); 5] = [
    ("/backend-api/codex/responses", "POST"),
    ("/responses", "POST"),
    ("/v1/responses", "POST"),
    ("/backend-api/codex/analytics-events/events", "POST"),
    ("/codex/analytics-events/events", "POST"),
];

const USAGE_PATHS: [(&str, &str); 2] = [("/usage", "GET,HEAD"), ("/api/oauth/usage", "GET,HEAD")];

/// Substitute a concrete segment for each path placeholder, so a template from
/// the inventory can be sent as a real request.
fn probe_path(template: &str) -> String {
    template.replace("{name}", "acct").replace("{id}", "spl_1")
}

/// Every inventory entry, in the order the groups above declare them.
fn all_registered_entries() -> Vec<(&'static str, &'static str)> {
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

/// Just the paths of [`all_registered_entries`], for the source scans — they
/// read path literals out of the source and have no method to compare against.
fn all_registered_paths() -> Vec<&'static str> {
    all_registered_entries()
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// Split an `Allow` value into a set, so a difference in the order axum happens
/// to list the methods in is not a difference in what the router allows.
fn method_set(allow: &str) -> BTreeSet<&str> {
    allow
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty())
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
fn all_surfaces_config(label: &str) -> (Config, EnvVars) {
    let suffix = format!("{}_{label}", std::process::id());
    let admin_env = format!("SHUNT_ROUTER_SURFACE_ADMIN_{suffix}");
    let client_env = format!("SHUNT_ROUTER_SURFACE_CLIENT_{suffix}");
    let jwt_env = format!("SHUNT_ROUTER_SURFACE_JWT_{suffix}");
    let users_env = format!("SHUNT_ROUTER_SURFACE_USERS_{suffix}");
    std::env::set_var(&admin_env, "admin:admin-secret");
    std::env::set_var(&client_env, "tester:client-secret");
    std::env::set_var(&jwt_env, "0123456789abcdef0123456789abcdef");
    std::env::set_var(&users_env, "dev@example.com:password");

    // The config takes ownership of these names below, so the guard needs its
    // own copies to remove them by.
    let admin_env_name = admin_env.clone();
    let client_env_name = client_env.clone();
    let jwt_env_name = jwt_env.clone();
    let users_env_name = users_env.clone();

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

    (
        config,
        EnvVars(vec![
            admin_env_name,
            client_env_name,
            jwt_env_name,
            users_env_name,
        ]),
    )
}

/// Removes the env vars [`all_surfaces_config`] set once the test holding it
/// finishes. The per-process-unique names already stop one test's value from
/// satisfying another's config, so this is hygiene rather than isolation — but
/// it matches the set/remove pairing every other test file here uses
/// (`tests/admin_surface.rs` pairs all 91 of its `set_var` calls), and keeps the
/// variables from outliving their test for the rest of the binary's run.
///
/// Removal happens on drop, at the end of the test body, never at its start:
/// clearing shared globals on entry is what breaks a neighbour mid-run.
struct EnvVars(Vec<String>);

impl Drop for EnvVars {
    fn drop(&mut self) {
        for name in &self.0 {
            std::env::remove_var(name);
        }
    }
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

/// The `Allow` header axum puts on the `405` answer to [`UNREGISTERED_METHOD`],
/// listing the methods the path is registered for.
async fn allowed_methods(router: &Router, path: &str) -> String {
    let request = Request::builder()
        .method(UNREGISTERED_METHOD)
        .uri(path)
        .body(Body::empty())
        .expect("probe request builds");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers the probe");
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "probing {path} with {UNREGISTERED_METHOD} did not answer 405, so it carries no Allow \
         header to read the registered methods off"
    );
    response
        .headers()
        .get(header::ALLOW)
        .unwrap_or_else(|| panic!("the 405 for {path} carries an Allow header"))
        .to_str()
        .expect("Allow is ASCII")
        .to_string()
}

/// The direction the path-only inventory left open: adding or removing a method
/// on a path that already exists changes no path string, so every other test in
/// this file stays green while the surface has actually moved. The `405` probe
/// already names the registered methods in its `Allow` header, so pinning them
/// costs no extra request and runs no handler.
#[tokio::test]
async fn every_registered_method_set_matches_the_inventory() {
    let (config, _env) = all_surfaces_config("methods");
    let (router, _shared, _state) = server::build_router(config).expect("router builds");

    for (path, documented) in all_registered_entries() {
        let allowed = allowed_methods(&router, &probe_path(path)).await;
        assert_eq!(
            method_set(&allowed),
            method_set(documented),
            "{path} answers {allowed:?} but this test's inventory documents {documented:?}. \
             Adding or removing a method on an existing path means updating it here too — and, \
             per docs/admin-ui-delivery.md, reviewing it against the /admin, /admin/api and \
             reserved /v1/organizations path split first."
        );
    }
}

/// The gap recorded under "Risks" in `docs/admin-ui-delivery.md`: `Router::merge`
/// panics on a duplicate path+method, but only when both trees are registered.
#[tokio::test]
async fn every_optional_surface_can_be_enabled_at_once() {
    let (config, _env) = all_surfaces_config("builds");
    let (_router, _shared, _state) =
        server::build_router(config).expect("a config enabling every optional surface builds");
}

#[tokio::test]
async fn every_documented_path_is_registered_when_all_surfaces_are_enabled() {
    let (config, _env) = all_surfaces_config("registered");
    let (router, _shared, _state) = server::build_router(config).expect("router builds");

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
    let (config, _env) = all_surfaces_config("undocumented");
    let (router, _shared, _state) = server::build_router(config).expect("router builds");

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
    let (config, _env) = all_surfaces_config("head");
    let (router, _shared, _state) = server::build_router(config).expect("router builds");

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
/// A route whose path is not a string literal at the call site is invisible to
/// this scan. There are two such sites today — the OTLP signals in
/// `gateway_router` and the `codex_endpoint::PATHS` / `codex_analytics::PATHS`
/// loops in `build_router`. `every_documented_path_is_registered_when_all_surfaces_are_enabled`
/// catches a removal or rename in either set but **not** an addition, so
/// [`INDIRECT_PATH_SOURCES`] scans those definitions to close that direction.
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

/// The path sets `build_router` and `gateway_router` register *indirectly*,
/// paired with the marker that opens their definition and the marker that
/// closes it. `registered_literal_paths` cannot see these — the call site
/// passes a loop variable or a method call, not a literal — so an **addition**
/// to one of them would otherwise register a live route while every other test
/// in this file stayed green.
const INDIRECT_PATH_SOURCES: [(&str, &str, &str, &str); 3] = [
    (
        "src/codex_endpoint.rs",
        include_str!("../src/codex_endpoint.rs"),
        "const PATHS: [&str; ",
        "];",
    ),
    (
        "src/codex_analytics.rs",
        include_str!("../src/codex_analytics.rs"),
        "const PATHS: [&str; ",
        "];",
    ),
    (
        "src/gateway/telemetry_ingest.rs",
        include_str!("../src/gateway/telemetry_ingest.rs"),
        "const fn path(self) -> &'static str {",
        "\n    }",
    ),
];

/// Extract every string literal between `open` and the first `close` after it.
fn string_literals_in_block<'a>(source: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let start = source
        .find(open)
        .unwrap_or_else(|| panic!("marker {open:?} not found; the definition was reshaped"))
        + open.len();
    let end = start
        + source[start..]
            .find(close)
            .unwrap_or_else(|| panic!("terminator {close:?} not found after {open:?}"));

    let mut literals = Vec::new();
    let mut rest = &source[start..end];
    while let Some(open_quote) = rest.find('"') {
        rest = &rest[open_quote + 1..];
        let Some(close_quote) = rest.find('"') else {
            break;
        };
        literals.push(&rest[..close_quote]);
        rest = &rest[close_quote + 1..];
    }
    literals
}

/// The direction `every_documented_path_is_registered_when_all_surfaces_are_enabled`
/// cannot cover. That test proves each *documented* path is live, so removing or
/// renaming a member of one of these sets fails it — but appending a member
/// leaves it green, because nothing probes a path this file has never heard of.
/// Reading the definitions back out of their own source closes that direction.
#[test]
fn every_indirectly_registered_path_is_documented() {
    let inventory = all_registered_paths();
    for (file, source, open, close) in INDIRECT_PATH_SOURCES {
        for path in string_literals_in_block(source, open, close) {
            assert!(
                inventory.contains(&path),
                "{file} registers {path} indirectly, which is not in this test's inventory. \
                 Adding a path to one of these sets means adding it here too — and, per \
                 docs/admin-ui-delivery.md, reviewing it against the /admin, /admin/api and \
                 reserved /v1/organizations path split first."
            );
        }
    }
}

/// The indirect scan is only a gate while it actually finds the definitions; a
/// refactor that reshaped one of them would otherwise leave
/// `every_indirectly_registered_path_is_documented` vacuously green.
#[test]
fn the_indirect_scan_finds_every_definition() {
    let found: usize = INDIRECT_PATH_SOURCES
        .iter()
        .map(|(_, source, open, close)| string_literals_in_block(source, open, close).len())
        .sum();
    // 3 `codex_endpoint::PATHS` + 2 `codex_analytics::PATHS` + 3 OTLP signals.
    assert_eq!(
        found, 8,
        "the indirect-path scan found {found} definitions, not 8; either a path was added or \
         removed, or one of these sets is no longer spelled the way the scan expects"
    );
}

/// How many router trees each scanned source composes in, as
/// `(file, merges, nests)`. Only `build_router` composes anything today: the
/// four `.merge(` calls that bring in `admin_router`, `gateway_router`,
/// `spend_router`, and the liveness tree built in `src/server.rs` itself.
const COMPOSITION_COUNTS: [(&str, usize, usize); 4] = [
    ("src/server.rs", 4, 0),
    ("src/admin/mod.rs", 0, 0),
    ("src/gateway/mod.rs", 0, 0),
    ("src/gateway/spend/mod.rs", 0, 0),
];

/// The outer edge of the two scans above. They read paths out of four fixed
/// source files, which covers everything registered *in* them — but a future
/// module could register its own tree and have one of these files compose it in,
/// and nothing would notice, because that module's source is never read. A child
/// router counts as much as `build_router` here: `gateway_router` merging a new
/// module hides it just as effectively.
///
/// This is a boundary guard, not a discovery mechanism: it does not find the new
/// module. It fails when one is composed in anywhere in the scanned set, so the
/// addition gets the path-split review `docs/admin-ui-delivery.md` asks for and
/// `ROUTER_SOURCES` gets extended in the same change. A bare `.route(` added to
/// any of these files needs no guard — they are all scanned.
#[test]
fn no_router_tree_is_composed_in_from_an_unscanned_module() {
    for (file, source) in ROUTER_SOURCES {
        let (_, merges, nests) = COMPOSITION_COUNTS
            .iter()
            .find(|(name, _, _)| *name == file)
            .unwrap_or_else(|| panic!("{file} has no entry in COMPOSITION_COUNTS"));

        assert_eq!(
            source.matches(".merge(").count(),
            *merges,
            "{file} composes a different number of router trees than the {merges} this test knows \
             about. If a new one was added, add its source file to ROUTER_SOURCES so its paths are \
             scanned, and review the routes it brings against the /admin, /admin/api and reserved \
             /v1/organizations path split in docs/admin-ui-delivery.md."
        );
        assert_eq!(
            source.matches(".nest(").count(),
            *nests,
            "{file} now nests a router tree. Nesting rewrites the paths its routes answer on, so \
             neither the literal scan nor the inventory above describes the served surface any \
             more — extend both before adopting it."
        );

        // `.route(` is not a substring of `.route_service(`, so the counts above
        // see neither of axum's service-based registrations. They register paths
        // exactly like their handler-based twins, which is why they are pinned at
        // zero here rather than left out.
        for api in [".route_service(", ".nest_service("] {
            assert_eq!(
                source.matches(api).count(),
                0,
                "{file} now registers a path with `{api}`, which none of the scans above read. \
                 Its paths would be served without appearing in the inventory — add it to the \
                 scans before adopting it."
            );
        }
    }
}

/// The last way a registration can go unseen: [`registered_literal_paths`] skips
/// any `.route(` whose first argument is not a string literal, and skipping
/// *silently* is the hazard — a future `.route(NEW_PATH, ...)` would change
/// neither the literal count nor the inventory.
///
/// Counting the skips closes that. Together with the two count assertions above
/// and the composition guard, the invariant across the scanned files is that no
/// registration is silently dropped: every `.route(` either resolves to a literal
/// path that must appear in the inventory, or is one of these five indirect sites
/// whose definitions [`INDIRECT_PATH_SOURCES`] reads, and the four remaining ways
/// axum can register a path — `.route_service(`, `.nest(`, `.nest_service(`, and
/// composing another tree in with `.merge(` — are each counted.
///
/// `Router::fallback` needs no count of its own: it claims no path, and a
/// catch-all would make [`path_is_registered`] answer something other than `404`
/// or `405` for an unrouted path, which it panics on. The two `.fallback(` calls
/// in `spend_router` are `MethodRouter::fallback`, a different thing — they
/// answer an unregistered *method* on a path that does exist, which the `Allow`
/// probe already covers.
#[test]
fn every_nonliteral_route_call_is_one_this_test_already_tracks() {
    let calls: usize = ROUTER_SOURCES
        .iter()
        .map(|(_, source)| source.matches(".route(").count())
        .sum();
    let literals: usize = ROUTER_SOURCES
        .iter()
        .map(|(_, source)| registered_literal_paths(source).len())
        .sum();

    // The two `codex_endpoint::PATHS` / `codex_analytics::PATHS` loops in
    // `build_router`, and the three `Signal::path()` calls in `gateway_router`.
    assert_eq!(
        calls - literals,
        5,
        "the scanned sources make {calls} `.route(` calls of which {literals} pass a string \
         literal, so {} are registered indirectly — not the 5 this test tracks through \
         INDIRECT_PATH_SOURCES. A new indirect registration must be added there, or its paths go \
         unscanned.",
        calls - literals
    );
}
