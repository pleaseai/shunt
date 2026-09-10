//! The embedded admin SPA bundle (`--features ui`).
//!
//! `ui/` is a React + Vite package built to `ui/dist`; this module embeds that
//! directory at compile time and serves it from the `/admin` mount, which keeps
//! the one-binary guarantee — no separate static host, no runtime asset
//! directory (`docs/admin-ui-delivery.md`, Decision 4). A default `cargo build`
//! has no Node toolchain and no bundle; release CI builds `ui/dist` and enables
//! the feature.
//!
//! The three routes this module backs are registered in [`super::admin_router`]:
//!
//! - `/admin/assets/{*path}` — the hashed bundle files, from `ui/dist/assets`.
//! - `/admin/api/{*path}` — an unmatched JSON path, answered `404` in the
//!   gateway's error shape. `/admin/api/*` is a different namespace from the
//!   UI's `/admin/*` (Decision 3), and returning the shell there is exactly the
//!   HTML-instead-of-`404` failure "Why not the root" rules out.
//! - `/admin/{*path}` — every other unmatched path under the mount, answered
//!   with the SPA shell so deep links survive a reload. Confined to the mount:
//!   an unmatched path outside `/admin` still `404`s.
//!
//! `GET /admin` itself is untouched — it still serves the server-rendered
//! dashboard (`super::dashboard`). Porting those views onto this bundle is the
//! next step of the track.
//!
//! The shell and the assets carry no operator data, so they are served without
//! admin authentication, exactly like `/admin/login`. Everything the SPA will
//! read lives under `/admin/api/*`, which authenticates every request.

use axum::{
    extract::Path,
    http::header,
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

/// `ui/dist`, embedded at compile time. Building with `--features ui` fails when
/// the directory is absent, which is why `ui/README.md` and the CI workflows
/// build the bundle before any cargo step.
#[derive(RustEmbed)]
#[folder = "ui/dist"]
struct Bundle;

/// The file the SPA shell is served from, and the entry point Vite emits.
const INDEX: &str = "index.html";

/// How many files the bundle embedded. A build that silently embedded nothing
/// would otherwise serve a blank page rather than fail; this is what lets a test
/// assert the bundle is non-empty.
pub fn embedded_file_count() -> usize {
    Bundle::iter().count()
}

/// `GET /admin/assets/{*path}` — one bundle file, with the `Content-Type` its
/// extension implies. A stylesheet or module script served as `text/plain` is
/// refused by browsers, and `nosniff` removes the sniffing that would otherwise
/// mask a wrong type.
pub(super) async fn asset(Path(path): Path<String>) -> Response {
    match Bundle::get(&format!("assets/{path}")) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => super::not_found(),
    }
}

/// The SPA shell's Content-Security-Policy.
///
/// Tighter than the server-rendered pages' policy
/// (`super::html_body_with_form_action`), and deliberately so: those pages
/// inline their script and style, so their policy has to allow
/// `'unsafe-inline'`. Vite emits the bundle as an external module script and an
/// external stylesheet under `/admin/assets/`, with nothing inline, so `'self'`
/// is enough for both — verified against the emitted `ui/dist/index.html`, not
/// assumed. `form-action 'none'` because the shell posts no forms; the
/// server-rendered login flow that does is a different response.
const SHELL_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
connect-src 'self'; img-src 'self'; form-action 'none'; base-uri 'none'; \
frame-ancestors 'none'";

/// `GET /admin/{*path}` — the bundle's `index.html` as the SPA shell for an
/// unmatched path under the mount, so a client-side route survives a reload.
///
/// Carries the same defense-in-depth header set as every other admin HTML
/// response (`super::html_body_with_form_action`): a tight CSP, the
/// clickjacking and sniffing guards, the admin surface's referrer policy, and
/// `no-store`. `Referrer-Policy` is `strict-origin-when-cross-origin` to match
/// the admin pages rather than the `no-referrer` used on the gateway device
/// page — under `no-referrer` a browser sends `Origin: null` even on a
/// same-origin form POST, which is what broke that page's CSRF guard
/// (`src/gateway/device.rs:171`).
pub(super) async fn shell() -> Response {
    match Bundle::get(INDEX) {
        Some(file) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CONTENT_SECURITY_POLICY, SHELL_CSP),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::X_FRAME_OPTIONS, "DENY"),
                (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        // Unreachable with a real bundle — `embedded_bundle_is_not_empty` and
        // the asset tests would both fail first — but a missing shell must not
        // be a blank `200`.
        None => super::internal("the embedded admin UI bundle has no index.html"),
    }
}

/// `/admin/api/{*path}` — an unmatched admin JSON path. Registered for every
/// method so the `404` does not become a `405`, and answered in the same error
/// shape as every other admin JSON handler.
pub(super) async fn api_not_found() -> Response {
    super::not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn embedded_bundle_is_not_empty() {
        assert!(
            embedded_file_count() > 0,
            "the ui/dist bundle embedded no files; a build that embeds nothing must fail rather \
             than serve a blank page — run `npm ci && npm run build` in ui/"
        );
    }

    #[test]
    fn the_bundle_carries_an_index_shell() {
        let index = Bundle::get(INDEX).expect("ui/dist/index.html is embedded");
        assert!(!index.data.is_empty(), "the embedded shell is empty");
    }

    #[tokio::test]
    async fn an_unknown_asset_is_not_found() {
        let response = asset(Path("does-not-exist.js".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
