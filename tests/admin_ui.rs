//! The embedded admin SPA (`--features ui`): the bundle is really embedded, the
//! assets are served with the type their extension implies, and the SPA
//! fallback stays inside the `/admin` mount.
//!
//! The mount boundary is the property `docs/admin-ui-delivery.md`'s "Why not
//! the root" argument turns into a test: an unmatched path *under* `/admin` is
//! exactly what must return the shell, while an unmatched path outside it — and
//! an unmatched path under the separate `/admin/api/*` JSON namespace — must
//! still `404`. A blanket assertion in either direction would pass a router
//! that got this wrong.

#![cfg(feature = "ui")]

use std::sync::{Mutex, MutexGuard};

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
    Router,
};
use shunt::{
    admin,
    config::{AdminConfig, Config},
    server,
};
use tower::ServiceExt;

/// Serializes every test in this binary that touches the process environment.
///
/// Unique variable names per test stop two tests from clobbering *each other's
/// variable*, but they do not make `set_var` safe: the hazard is a writer
/// racing a **reader**, and `build_router` reads the environment while a
/// sibling test may be writing it. So the lock has to span the write, the
/// `build_router` that reads, and the [`EnvVar`] cleanup — holding it for the
/// writes alone would exclude nothing. The tests here run in microseconds, so
/// serializing them costs nothing measurable.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A router with `[server.admin]` enabled, which is what registers the UI
/// routes. The env-backed credential gets a name unique to the process *and*
/// the calling test, and the returned [`EnvVar`] holds [`ENV_LOCK`] for the rest
/// of the test body so no sibling reads the environment mid-write.
fn admin_router(label: &str) -> (Router, EnvVar) {
    // A poisoned lock only means some other test panicked while holding it; the
    // environment is still ours to use, so recover rather than cascade.
    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let name = format!("SHUNT_ADMIN_UI_TOKENS_{}_{label}", std::process::id());
    std::env::set_var(&name, "admin:admin-secret");

    let mut config = Config::default();
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: name.clone(),
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        oidc: None,
    });

    let (router, _shared, _state) = server::build_router(config).expect("router builds");
    (
        router,
        EnvVar {
            name,
            _guard: guard,
        },
    )
}

/// Removes the variable on drop, at the end of the test body — never at its
/// start, which is what would break a neighbour mid-run — and releases
/// [`ENV_LOCK`] only after that removal.
///
/// `_guard` is never read: it is held for its `Drop`, which is the whole point,
/// and the leading underscore is what tells `dead_code` so.
struct EnvVar {
    name: String,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var(&self.name);
    }
}

async fn get(router: &Router, path: &str) -> axum::response::Response {
    request_with(router, Method::GET, path).await
}

async fn request_with(router: &Router, method: Method, path: &str) -> axum::response::Response {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("request builds");
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers")
}

fn content_type(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("response carries a Content-Type")
        .to_str()
        .expect("Content-Type is ASCII")
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec()
}

/// A build that silently embedded nothing would serve a blank page rather than
/// fail. Every other test here would still pass against an index-only bundle,
/// so the count is asserted directly.
#[test]
fn the_embedded_bundle_is_not_empty() {
    assert!(
        admin::embedded_file_count() > 0,
        "ui/dist embedded no files — run `npm ci && npm run build` in ui/ before building with \
         --features ui"
    );
}

/// The bytes and the `Content-Type` its extension implies, plus `nosniff`: a
/// browser refuses a module script served as `text/plain`, and sniffing would
/// otherwise mask exactly that bug.
#[tokio::test]
async fn an_asset_is_served_with_its_own_bytes_and_type() {
    let (router, _env) = admin_router("asset");

    // The shell names the hashed entry files, so the asset under test is the
    // real emitted one rather than a name this test invented.
    let shell = String::from_utf8(body_bytes(get(&router, "/admin/pool").await).await)
        .expect("the shell is UTF-8");
    let script = shell
        .split("/admin/assets/")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the shell references a hashed asset under /admin/assets/");
    assert!(script.ends_with(".js"), "expected a script, got {script}");

    let response = get(&router, &format!("/admin/assets/{script}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "text/javascript");
    assert_eq!(
        response
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .map(|value| value.to_str().unwrap()),
        Some("nosniff")
    );
    assert!(!body_bytes(response).await.is_empty());
}

/// The shell is an admin HTML response, so it carries the same defense-in-depth
/// header set the server-rendered pages do
/// (`src/admin/mod.rs`'s `html_body_with_form_action`). Asserted per header
/// rather than as one blob: dropping any single one is a separate regression,
/// and the CSP is pinned to `'self'` for script and style because the bundle is
/// external — a later change that inlines script would have to loosen this
/// value, and should have to say so here.
#[tokio::test]
async fn the_shell_carries_the_admin_security_headers() {
    let (router, _env) = admin_router("shell-headers");

    let response = get(&router, "/admin/pool").await;
    assert_eq!(response.status(), StatusCode::OK);

    let headers = response.headers();
    for (name, expected) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        assert_eq!(
            headers.get(&name).map(|v| v.to_str().unwrap()),
            Some(expected),
            "the SPA shell must carry {name}: {expected}"
        );
    }

    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("the SPA shell must carry a Content-Security-Policy")
        .to_str()
        .unwrap();
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "connect-src 'self'",
        "form-action 'none'",
        "base-uri 'none'",
        "frame-ancestors 'none'",
    ] {
        assert!(
            csp.contains(directive),
            "the shell CSP is missing `{directive}`; it reads {csp:?}"
        );
    }
    assert!(
        !csp.contains("unsafe-inline"),
        "the bundle is external, so the shell CSP must not allow inline code; it reads {csp:?}"
    );
}

/// An unmatched path under the mount is a client-side route, so it must survive
/// a reload as the shell rather than `404`.
#[tokio::test]
async fn an_unmatched_path_under_the_mount_returns_the_shell() {
    let (router, _env) = admin_router("shell");

    let response = get(&router, "/admin/pool").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(content_type(&response).starts_with("text/html"));
    assert!(
        String::from_utf8(body_bytes(response).await)
            .expect("the shell is UTF-8")
            .contains("/admin/assets/"),
        "the shell must be the built index.html, which links the hashed bundle"
    );
}

/// `/admin/api/*` is a separate namespace from the UI's `/admin/*`
/// (`docs/admin-ui-delivery.md`, Decision 3). Answering it with the shell is the
/// HTML-instead-of-`404` failure "Why not the root" exists to avoid, so the JSON
/// namespace keeps its own catch-all — in the gateway's error shape, like every
/// other admin JSON handler.
#[tokio::test]
async fn an_unmatched_path_under_the_json_namespace_is_a_json_404() {
    let (router, _env) = admin_router("json404");

    let response = get(&router, "/admin/api/nope").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        content_type(&response).starts_with("application/json"),
        "expected JSON, got {}",
        content_type(&response)
    );

    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes(response).await).expect("the body is JSON");
    assert_eq!(body["error"]["type"], "not_found_error");
}

/// The JSON catch-all is registered with `any`, and that is load-bearing: under
/// a method-specific registration an unmatched `/admin/api/*` path would answer
/// `405` for every other method instead of the `404` contract — and `405` is
/// what the SPA fallback was separated from this namespace to avoid.
///
/// The `404`-vs-`405` probe in `tests/router_surface.rs` cannot see this: to
/// that oracle a path answering `404` for every method is indistinguishable
/// from an unregistered one, which is why the catch-all is excluded from its
/// method-aware inventory. Probing the methods directly is what covers the gap.
#[tokio::test]
async fn the_json_namespace_catch_all_answers_every_method() {
    let (router, _env) = admin_router("json404methods");

    for method in [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
    ] {
        let response = request_with(&router, method.clone(), "/admin/api/nope").await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} /admin/api/nope must be a 404, not a 405 or the shell"
        );
        assert!(
            content_type(&response).starts_with("application/json"),
            "{method} /admin/api/nope expected JSON, got {}",
            content_type(&response)
        );
    }
}

/// The namespace *root* is the case a single `/admin/api/{*path}` catch-all
/// misses: a wildcard segment must match at least one character, so `/admin/api`
/// and `/admin/api/` fall through to the broader `/admin/{*path}` and would
/// answer HTML `200`. That is the same failure the test above rules out, one
/// path segment shorter, so the roots are registered explicitly.
#[tokio::test]
async fn the_json_namespace_root_is_a_json_404_too() {
    let (router, _env) = admin_router("json404root");

    for path in ["/admin/api", "/admin/api/"] {
        let response = get(&router, path).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must not answer the SPA shell"
        );
        assert!(
            content_type(&response).starts_with("application/json"),
            "{path} expected JSON, got {}",
            content_type(&response)
        );
    }
}

/// The fallback is confined to its mount: `/v1/` has externally-specified
/// owners, and turning a clean "not implemented" into an HTML `200` is what
/// makes a catch-all at the root unacceptable.
#[tokio::test]
async fn an_unmatched_path_outside_the_mount_still_404s() {
    let (router, _env) = admin_router("outside");

    for path in ["/nope", "/v1/nope", "/adminx"] {
        let response = get(&router, path).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must stay a 404 once the SPA fallback exists"
        );
    }
}

/// `GET /admin` is still the server-rendered dashboard, which sends an
/// unauthenticated browser to the login page. The SPA shell answers only the
/// paths *below* the mount until the views are ported, so the dashboard is not
/// broken between the two steps — a shell here would be a `200` instead.
#[tokio::test]
async fn the_mount_root_still_serves_the_server_rendered_dashboard() {
    let (router, _env) = admin_router("root");

    let response = get(&router, "/admin").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .map(|value| value.to_str().unwrap()),
        Some("/admin/login"),
        "/admin must keep serving the string-literal dashboard, not the SPA shell"
    );
}

/// The router-level twin of `admin::ui::tests::a_traversal_path_finds_nothing`.
///
/// The unit test proves the handler resolves nothing for a traversal segment;
/// this proves the same probe cannot reach a *different* handler on the way in.
/// Both matter: a router that normalized `/admin/assets/../..` before matching
/// would leave the unit test green while serving something else entirely, and
/// what a caller actually sends is a URI, not a `Path` extractor argument.
#[tokio::test]
async fn a_traversal_probe_under_the_asset_mount_never_serves_a_file() {
    let (router, _env) = admin_router("traversal");

    for probe in [
        "/admin/assets/../../../../etc/passwd",
        "/admin/assets/..%2f..%2f..%2fetc%2fpasswd",
        "/admin/assets/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/admin/assets/....//....//etc/passwd",
    ] {
        let response = get(&router, probe).await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");

        assert!(
            !body.windows(5).any(|window| window == b"root:"),
            "{probe} answered {status} with something that looks like /etc/passwd"
        );
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{probe} must resolve to nothing, not to a file"
        );
    }
}
