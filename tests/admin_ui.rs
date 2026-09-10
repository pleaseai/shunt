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

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use shunt::{
    admin,
    config::{AdminConfig, Config},
    server,
};
use tower::ServiceExt;

/// A router with `[server.admin]` enabled, which is what registers the UI
/// routes. The env-backed credential gets a name unique to the process *and*
/// the calling test: the process environment is shared across the test binary,
/// so a name shared between two tests lets one test's [`EnvVar`] drop clear the
/// variable another is still building against.
fn admin_router(label: &str) -> (Router, EnvVar) {
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
    (router, EnvVar(name))
}

/// Removes the variable on drop, at the end of the test body — never at its
/// start, which is what would break a neighbour mid-run.
struct EnvVar(String);

impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var(&self.0);
    }
}

async fn get(router: &Router, path: &str) -> axum::response::Response {
    let request = Request::builder()
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
