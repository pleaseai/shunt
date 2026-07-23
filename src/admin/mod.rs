//! Opt-in admin web surface (M9): browser account provisioning + a read-only
//! account-pool dashboard. Registered only when `[server.admin]` is configured
//! (see `crate::server::build_router`); absent ⇒ none of these routes exist.
//!
//! Two credentials never mix: `[server.auth]` client tokens are handed to devices,
//! `[server.admin]` admin tokens add upstream accounts. Browsers authenticate with
//! a session cookie minted after a login form and are CSRF-protected; API/curl
//! callers pass the admin token header and are CSRF-exempt (no ambient cookie).
//! The provisioning flow reuses provider OAuth internals for Claude full/setup
//! logins and refreshable ChatGPT/Codex logins; token values are never returned
//! to the browser or logged. See `docs/m9-admin-surface.md`.

pub mod session;

mod codex;
mod html;
mod oidc;

use std::{collections::HashSet, io, sync::Arc, time::Duration};

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{
        claude::{auth as claude_auth, login as claude_login, store as claude_store},
        inbound::{constant_time_eq, InboundAuth},
        observation::{self, ObservedCredential, ObservedProvider},
    },
    config::AuthMode,
    error::ShuntError,
    server::AppState,
};

pub use session::AdminStores;

use session::{PendingAttempt, PendingKind};

const SESSION_COOKIE: &str = "shunt_admin_session";

/// Resolved admin credential plus session/pending lifetimes. Held in
/// `RuntimeState` (hot-reloaded so token edits apply); the session and pending
/// stores live in `AppState` (process lifetime). The token check reuses the
/// inbound-auth constant-time compare.
/// No `Debug`: `InboundAuth` holds the raw admin token values, so a derived
/// `Debug` would risk leaking them (matches the secret-carrying `PendingLogin`).
#[derive(Clone)]
pub struct AdminAuth {
    inbound: InboundAuth,
    session_ttl: Duration,
    pending_ttl: Duration,
    oidc: Option<Arc<crate::gateway::ResolvedIdp>>,
    oidc_public_url: Option<String>,
}

impl AdminAuth {
    pub fn new(inbound: InboundAuth, session_ttl: Duration, pending_ttl: Duration) -> Self {
        Self {
            inbound,
            session_ttl,
            pending_ttl,
            oidc: None,
            oidc_public_url: None,
        }
    }

    pub fn with_oidc(mut self, public_url: String, idp: crate::gateway::ResolvedIdp) -> Self {
        self.oidc = Some(Arc::new(idp));
        self.oidc_public_url = Some(public_url);
        self
    }

    pub fn oidc(&self) -> Option<&crate::gateway::ResolvedIdp> {
        self.oidc.as_deref()
    }

    pub(crate) fn oidc_arc(&self) -> Option<Arc<crate::gateway::ResolvedIdp>> {
        self.oidc.clone()
    }

    pub(crate) fn oidc_callback_url(&self) -> Option<String> {
        self.oidc_public_url
            .as_deref()
            .map(|url| format!("{url}/admin/oidc/callback"))
    }

    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    pub fn pending_ttl(&self) -> Duration {
        self.pending_ttl
    }

    /// Whether the request carries a valid admin token in the configured header.
    fn authenticate_header(&self, headers: &HeaderMap) -> bool {
        self.inbound.authenticate(headers).is_some()
    }

    /// Whether a raw token (from the login form) matches a configured admin token.
    fn authenticate_token(&self, token: &str) -> bool {
        self.inbound.authenticate_value(token.as_bytes()).is_some()
    }
}

/// The admin route tree, merged into the main router only when admin is enabled.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(dashboard))
        .route("/admin/login", get(login_page).post(login_submit))
        .route("/admin/oidc/start", post(oidc::start))
        .route("/admin/oidc/callback", get(oidc::callback))
        .route("/admin/logout", post(logout))
        .route("/admin/accounts", get(list_accounts))
        .route("/admin/observed", get(observed_accounts))
        .route("/admin/pool", get(pool))
        .route("/admin/accounts/claude", post(add_account))
        .route(
            "/admin/accounts/claude/{name}/complete",
            post(complete_account),
        )
        .route(
            "/admin/accounts/claude/{name}",
            delete(remove_account_handler),
        )
        .route(
            "/admin/accounts/codex",
            get(codex::list_codex_accounts).post(codex::add_codex_account),
        )
        .route(
            "/admin/accounts/codex/{name}/complete",
            post(codex::complete_codex_account),
        )
        .route(
            "/admin/accounts/codex/{name}",
            delete(codex::remove_codex_account_handler),
        )
}

/// How a request authenticated, which decides whether CSRF applies.
pub(super) enum Authenticated {
    /// Admin token header (API/curl): no ambient cookie, so CSRF-exempt.
    Header,
    /// Session cookie (browser): CSRF-protected.
    Session { csrf: String },
}

pub(super) struct AuthOk {
    pub(super) kind: Authenticated,
    pub(super) auth: Arc<AdminAuth>,
}

/// Resolve the request's admin authentication, or `None` when unauthenticated (or
/// the admin surface has been disabled by a reload).
pub(super) fn authenticate(state: &AppState, headers: &HeaderMap) -> Option<AuthOk> {
    let auth = state.admin_auth.clone()?;
    if auth.authenticate_header(headers) {
        return Some(AuthOk {
            kind: Authenticated::Header,
            auth,
        });
    }
    let sid = session_cookie(headers)?;
    let csrf = state.admin_stores.sessions.csrf_for(&sid)?;
    Some(AuthOk {
        kind: Authenticated::Session { csrf },
        auth,
    })
}

/// Enforce CSRF on cookie-authenticated mutations: a same-origin request bearing
/// the session's CSRF token. Header-token callers are exempt. Returns the
/// rejection response when the check fails, or `None` when the request may
/// proceed.
pub(super) fn check_csrf(kind: &Authenticated, headers: &HeaderMap) -> Option<Response> {
    let Authenticated::Session { csrf } = kind else {
        return None;
    };
    if !same_origin(headers) {
        return Some(forbidden("cross-origin admin request rejected"));
    }
    let presented = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if constant_time_eq(presented.as_bytes(), csrf.as_bytes()) {
        None
    } else {
        Some(forbidden("missing or invalid CSRF token"))
    }
}

/// Drop process-lifetime pool health only when no upstream in this store family
/// still references the physical identity. A failed dynamic-store scan preserves
/// state (fail closed).
pub(super) fn forget_pool_health_if_absent(
    state: &AppState,
    auth: AuthMode,
    identity: &str,
    store_scan_others: Option<&HashSet<String>>,
) {
    let mut uncertain = false;
    let still_present = state.config.providers.values().any(|provider| {
        if provider.auth != auth {
            return false;
        }
        let inline_match = provider
            .accounts
            .iter()
            .any(|account| crate::accounts::account_identity(account) == identity);
        if inline_match {
            return true;
        }
        let draws_from_store = !provider.account_scope.is_empty() || provider.accounts.is_empty();
        if !draws_from_store {
            return false;
        }
        match store_scan_others {
            Some(others) => others.contains(identity),
            None => {
                uncertain = true;
                false
            }
        }
    });
    if still_present || uncertain {
        return;
    }
    let family = match auth {
        AuthMode::ClaudeOauth => crate::accounts::StoreFamily::Claude,
        AuthMode::ChatgptOauth => crate::accounts::StoreFamily::Chatgpt,
        _ => return,
    };
    state.accounts.forget_identity(family, identity);
}

pub(super) async fn remaining_account_identities(
    account_name: &str,
    scan: fn() -> io::Result<Vec<crate::config::AccountConfig>>,
) -> Option<HashSet<String>> {
    let excluded_name = account_name.to_string();
    tokio::task::spawn_blocking(move || {
        scan().ok().map(|accounts| {
            accounts
                .into_iter()
                .filter(|account| account.name != excluded_name)
                .map(|account| crate::accounts::account_identity(&account).to_string())
                .collect()
        })
    })
    .await
    .unwrap_or(None)
}

pub(super) fn cleanup_reprovisioned_pool_health(
    state: &AppState,
    auth: AuthMode,
    old_identity: Option<&str>,
    new_identity: &str,
    other_identities: Option<&HashSet<String>>,
) {
    if let Some(old_identity) = old_identity.filter(|old| *old != new_identity) {
        forget_pool_health_if_absent(state, auth, old_identity, other_identities);
    }
    forget_pool_health_if_absent(state, auth, new_identity, other_identities);
}

/// Same-origin check: prefer Fetch Metadata (`Sec-Fetch-Site`), fall back to
/// comparing the `Origin` authority to `Host`. A missing `Origin` (non-browser)
/// is allowed — the CSRF token and `SameSite=Strict` cookie still gate the call.
pub(super) fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        return matches!(site, "same-origin" | "none");
    }
    match headers.get("origin").and_then(|value| value.to_str().ok()) {
        None => true,
        Some(origin) => {
            let (scheme, origin_authority) = origin
                .split_once("://")
                .map_or(("", origin), |(scheme, rest)| (scheme, rest));
            let host = headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            // Compare effective authorities: strip only the scheme's DEFAULT port
            // so a proxy that adds/omits it on one side still matches, while a
            // genuinely different explicit port stays a distinct origin. This is
            // only the fallback — `Sec-Fetch-Site` above is the primary signal.
            let default_port = match scheme {
                "https" => ":443",
                "http" => ":80",
                _ => "",
            };
            !host.is_empty()
                && strip_default_port(origin_authority, default_port)
                    .eq_ignore_ascii_case(strip_default_port(host, default_port))
        }
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{SESSION_COOKIE}=");
    cookies.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

/// Whether the session cookie should carry `Secure`. A TLS-terminating reverse
/// proxy is honored first via `X-Forwarded-Proto: https`; otherwise `Secure` is
/// set unless the request targets a loopback host, so local HTTP dev (and tests)
/// still work while any real deployment host gets a Secure cookie. Mirrors the M8
/// loopback carve-out.
///
/// NOTE: absent `X-Forwarded-Proto`, this trusts the `Host` header — a reverse
/// proxy that rewrites `Host` to a loopback value without forwarding the proto
/// would drop `Secure` on a public HTTPS deployment, so front admin with a proxy
/// that sends `X-Forwarded-Proto` or preserves the external `Host`. Trusting
/// `X-Forwarded-Proto` only ever *adds* `Secure`, so a spoofed value cannot
/// weaken the cookie.
pub(super) fn secure_cookie(headers: &HeaderMap) -> bool {
    if headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    !crate::config::host_is_loopback(host_without_port(host))
}

/// Strip a trailing scheme-default port (`:443` / `:80`) so `example.com` and
/// `example.com:443` compare equal, without collapsing a genuinely different
/// (non-default) port. `default_port` empty ⇒ no stripping.
fn strip_default_port<'a>(authority: &'a str, default_port: &str) -> &'a str {
    if default_port.is_empty() {
        authority
    } else {
        authority.strip_suffix(default_port).unwrap_or(authority)
    }
}

fn host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal `[addr]` or `[addr]:port`.
        return rest.split_once(']').map_or(rest, |(addr, _)| addr);
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

pub(super) fn set_cookie(sid: &str, secure: bool, ttl: Duration) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={}",
        ttl.as_secs()
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0")
}

// --- page routes ---------------------------------------------------------------

async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Re-snapshot the hot-reloaded runtime state so admin_auth/config track the
    // live config (matches proxy.rs/routes.rs/discovery.rs); without this every
    // admin route would run on the boot-time snapshot forever.
    let state = state.refreshed();
    if state.admin_auth.is_none() {
        return not_found();
    }
    if authenticate(&state, &headers).is_some() {
        return redirect("/admin");
    }
    let sso_label = state
        .admin_auth
        .as_deref()
        .and_then(AdminAuth::oidc)
        .map(crate::gateway::ResolvedIdp::button_label);
    login_response(StatusCode::OK, None, sso_label)
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<LoginForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.admin_auth.clone() else {
        return not_found();
    };
    // Throttle admin-token guessing (defense-in-depth behind the constant-time
    // compare); every POST counts, before the token is checked.
    if !state.admin_stores.login_rate.check() {
        return too_many_requests("too many login attempts; slow down");
    }
    let token = match form {
        Ok(Form(form)) => form.token,
        Err(_) => String::new(),
    };
    if !auth.authenticate_token(&token) {
        return login_response(
            StatusCode::UNAUTHORIZED,
            Some("Invalid admin token."),
            auth.oidc().map(crate::gateway::ResolvedIdp::button_label),
        );
    }
    let (sid, _csrf) = state.admin_stores.sessions.create(auth.session_ttl());
    let cookie = set_cookie(&sid, secure_cookie(&headers), auth.session_ttl());
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/admin".to_string()),
        ],
    )
        .into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    // Logout is a navigation form POST, so it cannot carry the `x-csrf-token`
    // header the JSON mutations use; guard it with a same-origin check instead
    // (plus the `SameSite=Strict` cookie) so a cross-site page cannot force a
    // logout. See docs/m9-admin-surface.md.
    if !same_origin(&headers) {
        return forbidden("cross-origin admin request rejected");
    }
    if let Some(sid) = session_cookie(&headers) {
        state.admin_stores.sessions.remove(&sid);
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, clear_cookie()),
            (header::LOCATION, "/admin/login".to_string()),
        ],
    )
        .into_response()
}

async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return redirect("/admin/login");
    };
    // Header-token callers hitting the HTML page have no CSRF token; render with
    // an empty one (mutations from the page then require a real session).
    let csrf = match authok.kind {
        Authenticated::Session { csrf } => csrf,
        Authenticated::Header => String::new(),
    };
    html_page(html::dashboard_page(&csrf))
}

// --- JSON API routes -----------------------------------------------------------

async fn list_accounts(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    match tokio::task::spawn_blocking(claude_store::list_account_meta).await {
        Ok(Ok(accounts)) => json_secure(json!({ "accounts": accounts })),
        Ok(Err(error)) => {
            tracing::error!(%error, "admin: failed to list account metadata");
            internal("failed to list accounts")
        }
        Err(join_error) => {
            tracing::error!(%join_error, "admin: list_account_meta task panicked");
            internal("failed to list accounts")
        }
    }
}

async fn observed_accounts(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = Arc::new(state.refreshed());
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }

    let discovered = match tokio::task::spawn_blocking(observation::discover).await {
        Ok(discovered) => discovered,
        Err(join_error) => {
            tracing::error!(%join_error, "admin: observed credential discovery task panicked");
            return internal("failed to discover local credentials");
        }
    };

    let rows = futures_util::future::join_all(
        discovered
            .into_iter()
            .map(|observed| build_observed_row(Arc::clone(&state), observed)),
    )
    .await;

    json_secure(json!({ "accounts": rows }))
}

async fn build_observed_row(state: Arc<AppState>, observed: ObservedCredential) -> Value {
    let provider = observed.provider.as_str();
    let source = observed.source.as_str();
    if !observed.valid && observed.provider != ObservedProvider::Kimi {
        return json!({
            "provider": provider,
            "identity": observed.identity,
            "detail": observed.detail,
            "source": source,
            "ownership": "observed",
            "signal": observed.provider.signal(),
            "state": "expired",
            "message": "Re-authenticate with the provider CLI; shunt never refreshes observed credentials."
        });
    }

    match observed.provider {
        ObservedProvider::Claude => {
            build_claude_observed_row(&state, observed, provider, source).await
        }
        ObservedProvider::Codex => build_codex_observed_row(&state, observed, provider, source),
        ObservedProvider::Gemini => {
            build_gemini_observed_row(&state, observed, provider, source).await
        }
        ObservedProvider::Kimi => build_kimi_observed_row(&state, observed, provider, source).await,
        ObservedProvider::Cursor => {
            build_cursor_observed_row(&state, observed, provider, source).await
        }
        ObservedProvider::Grok => build_grok_observed_row(&state, observed, provider, source).await,
    }
}

async fn build_claude_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    let cached = state
        .admin_stores
        .observed_usage
        .claude(&observed.access_token);
    let usage = match cached {
        Some(usage) => Ok(usage),
        None => crate::auth::claude::usage::fetch_usage(
            &state.http_client,
            "https://api.anthropic.com",
            &observed.access_token,
        )
        .await
        .inspect(|usage| {
            state
                .admin_stores
                .observed_usage
                .store_claude(&observed.access_token, usage.clone())
        }),
    };
    match usage {
        Ok(usage) => json!({
            "provider": provider,
            "identity": observed.identity,
            "detail": observed.detail,
            "source": source,
            "ownership": "observed",
            "signal": "quota",
            "state": "available",
            "utilization_5h": usage.five_hour.as_ref().map(|window| window.utilization),
            "reset_5h": usage.five_hour.as_ref().and_then(|window| window.resets_at),
            "utilization_7d": usage.seven_day.as_ref().map(|window| window.utilization),
            "reset_7d": usage.seven_day.as_ref().and_then(|window| window.resets_at),
            "utilization_7d_oi": usage.seven_day_oi.as_ref().map(|window| window.utilization),
            "reset_7d_oi": usage.seven_day_oi.as_ref().and_then(|window| window.resets_at)
        }),
        Err(error) => {
            tracing::debug!(%error, "admin: read-only Claude usage observation failed");
            json!({
                "provider": provider,
                "identity": observed.identity,
                "detail": observed.detail,
                "source": source,
                "ownership": "observed",
                "signal": "quota",
                "state": "unavailable",
                "message": "Usage could not be read with the current Claude Code login."
            })
        }
    }
}

fn build_codex_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    let account = crate::config::AccountConfig {
        name: "local-codex".to_string(),
        uuid: observed.account_id.clone(),
        ..Default::default()
    };
    let snapshot = state
        .config
        .providers
        .iter()
        .filter(|(_, config)| config.auth == AuthMode::ChatgptOauth)
        .filter_map(|(provider_name, _)| {
            state
                .accounts
                .snapshot(
                    provider_name,
                    std::slice::from_ref(&account),
                    None,
                    state.config.server.pool.as_ref(),
                )
                .into_iter()
                .next()
        })
        .find(|snapshot| snapshot.has_state);
    let has_state = snapshot.as_ref().is_some_and(|snapshot| snapshot.has_state);
    json!({
        "provider": provider,
        "identity": observed.identity,
        "detail": observed.detail,
        "source": source,
        "ownership": "observed",
        "signal": "response-derived",
        "state": if has_state { "available" } else { "waiting-for-traffic" },
        "message": if has_state { None } else { Some("Usage appears after a GPT response carries x-codex-* quota headers.") },
        "utilization_5h": snapshot.as_ref().and_then(|snapshot| snapshot.utilization_5h),
        "reset_5h": snapshot.as_ref().and_then(|snapshot| snapshot.reset_5h),
        "utilization_7d": snapshot.as_ref().and_then(|snapshot| snapshot.utilization_7d),
        "reset_7d": snapshot.as_ref().and_then(|snapshot| snapshot.reset_7d),
        "utilization_7d_oi": snapshot.as_ref().and_then(|snapshot| snapshot.utilization_7d_oi),
        "reset_7d_oi": snapshot.as_ref().and_then(|snapshot| snapshot.reset_7d_oi)
    })
}

async fn build_gemini_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    match observation::fetch_gemini_quota(&state.http_client, &observed.access_token).await {
        Ok(snapshot) => json!({
            "provider": provider,
            "identity": snapshot.account_label,
            "detail": snapshot.detail.or(Some(observed.identity)),
            "source": if snapshot.account_label.starts_with("Antigravity") { "Antigravity local service (read-only)" } else { source },
            "ownership": "observed",
            "signal": "quota",
            "state": "available",
            "quota_buckets": snapshot.buckets.into_iter().filter_map(|bucket| {
                let label = bucket.model_id?;
                Some(json!({
                    "label": label.strip_prefix("gemini-").unwrap_or(&label).replace('-', " "),
                    "remaining": bucket.remaining_fraction,
                    "remaining_amount": bucket.remaining_amount,
                    "reset_time": bucket.reset_time
                }))
            }).collect::<Vec<_>>()
        }),
        Err(error) => {
            tracing::debug!(%error, "admin: read-only Gemini quota observation failed");
            json!({
                "provider": provider,
                "identity": observed.identity,
                "detail": observed.detail,
                "source": source,
                "ownership": "observed",
                "signal": "quota",
                "state": "unavailable",
                "message": "Gemini quota could not be read with the current Gemini CLI login."
            })
        }
    }
}

async fn build_kimi_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    let sidecar_base_url = state
        .config
        .providers
        .iter()
        .find(|(name, provider)| {
            name.eq_ignore_ascii_case("kimi") || provider.base_url.contains("/coding")
        })
        .map(|(_, provider)| provider.base_url.as_str());
    match observation::fetch_kimi_quota(
        &state.http_client,
        &observed.access_token,
        sidecar_base_url,
    )
    .await
    {
        Ok(buckets) => json!({
            "provider": provider,
            "identity": "Kimi Code",
            "detail": observed.identity,
            "source": source,
            "ownership": "observed",
            "signal": "quota",
            "state": "available",
            "quota_buckets": buckets
        }),
        Err(error) => {
            tracing::debug!(%error, "admin: read-only Kimi quota observation failed");
            json!({
                "provider": provider,
                "identity": "Kimi Code",
                "detail": observed.identity,
                "source": source,
                "ownership": "observed",
                "signal": "quota",
                "state": "unavailable",
                "message": "Kimi quota could not be read with the current Kimi Code login."
            })
        }
    }
}

async fn build_cursor_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    match observation::fetch_cursor_quota(&state.http_client).await {
        Ok(snapshot) => json!({
            "provider": provider,
            "identity": snapshot.account_label,
            "detail": snapshot.detail.or(observed.detail),
            "source": "Cursor.app session (read-only)",
            "ownership": "observed",
            "signal": "quota",
            "state": "available",
            "quota_buckets": snapshot.buckets
        }),
        Err(error) => {
            tracing::debug!(%error, "admin: read-only Cursor quota observation failed");
            json!({
                "provider": provider,
                "identity": observed.identity,
                "detail": observed.detail,
                "source": source,
                "ownership": "observed",
                "signal": "quota",
                "state": "unavailable",
                "message": "Cursor quota could not be read from the current Cursor.app login."
            })
        }
    }
}

async fn build_grok_observed_row(
    state: &AppState,
    observed: ObservedCredential,
    provider: &str,
    source: &str,
) -> Value {
    match observation::fetch_grok_quota(&state.http_client, &observed.access_token).await {
        Ok(snapshot) => json!({
            "provider": provider,
            "identity": snapshot.account_label,
            "detail": snapshot.detail.or(observed.detail),
            "source": source,
            "ownership": "observed",
            "signal": "quota",
            "state": "available",
            "quota_buckets": snapshot.buckets
        }),
        Err(error) => {
            tracing::debug!(%error, "admin: read-only Grok quota observation failed");
            json!({
                "provider": provider,
                "identity": observed.identity,
                "detail": observed.detail,
                "source": source,
                "ownership": "observed",
                "signal": "quota",
                "state": "unavailable",
                "message": "Grok quota could not be read with the current Grok CLI login."
            })
        }
    }
}

async fn pool(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    let mut providers = Vec::new();
    for (name, provider) in &state.config.providers {
        if !matches!(
            provider.auth,
            AuthMode::ClaudeOauth | AuthMode::ChatgptOauth
        ) {
            continue;
        }
        let resolved = match provider.auth {
            AuthMode::ClaudeOauth => {
                crate::auth::shared::resolve_pool_accounts(
                    "Claude",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Claude,
                    claude_store::default_accounts_dir(),
                    claude_store::scan_accounts,
                )
                .await
            }
            AuthMode::ChatgptOauth => {
                crate::auth::shared::resolve_pool_accounts(
                    "codex",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Chatgpt,
                    crate::auth::codex::store::default_accounts_dir(),
                    crate::auth::codex::store::scan_accounts,
                )
                .await
            }
            _ => unreachable!("provider auth filtered above"),
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::error!(provider = %name, %error, "admin: failed to resolve account scope");
                return internal("failed to read pool state");
            }
        };
        let snapshots =
            state
                .accounts
                .snapshot(name, &resolved, None, state.config.server.pool.as_ref());
        providers.push(json!({ "provider": name, "accounts": snapshots }));
    }
    json_secure(json!({ "providers": providers }))
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AddMode {
    #[default]
    SetupToken,
    Oauth,
}

impl AddMode {
    fn pending_kind(self) -> PendingKind {
        match self {
            Self::SetupToken => PendingKind::SetupToken,
            Self::Oauth => PendingKind::FullOauth,
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::SetupToken => claude_login::SETUP_TOKEN_SCOPE,
            Self::Oauth => claude_auth::SCOPE,
        }
    }
}

#[derive(Deserialize)]
struct AddBody {
    name: String,
    #[serde(default)]
    mode: AddMode,
}

async fn add_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AddBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if claude_store::validate_account_name(&body.name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let pending_kind = body.mode.pending_kind();
    let pkce = claude_login::generate_pkce();
    let authorize_url = match claude_login::build_authorize_url(
        &pkce.challenge,
        &pkce.state,
        body.mode.scope(),
        claude_login::MANUAL_REDIRECT_URL,
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(account = %body.name, %error, "admin: failed to build authorize URL");
            return internal("failed to build authorize URL");
        }
    };
    state.admin_stores.pending.start(
        &body.name,
        pending_kind,
        pkce.verifier,
        pkce.state,
        authok.auth.pending_ttl(),
    );
    tracing::info!(account = %body.name, mode = ?body.mode, "admin: account provisioning started");
    json_secure(json!({ "name": body.name, "authorize_url": authorize_url.to_string() }))
}

#[derive(Deserialize)]
struct CompleteBody {
    code: String,
}

async fn complete_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CompleteBody>, JsonRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if !state.admin_stores.complete_rate.check() {
        return too_many_requests("too many completion attempts; slow down");
    }
    let Ok(Json(body)) = body else {
        return bad_request("invalid JSON body");
    };
    if claude_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let pending = match state.admin_stores.pending.attempt(&name) {
        PendingAttempt::Ready(pending) => pending,
        PendingAttempt::NotFound => {
            return bad_request("no pending login for this account; start again")
        }
        PendingAttempt::TooManyAttempts => return bad_request("too many attempts; start again"),
    };

    let pasted = body.code.trim();
    let Some((code, returned_state)) = pasted.split_once('#') else {
        return bad_request("authorization code must have the form <code>#<state>");
    };
    if code.is_empty() || !constant_time_eq(returned_state.as_bytes(), pending.state.as_bytes()) {
        return bad_request("invalid authorization code or OAuth state mismatch");
    }

    let expires_in = match pending.kind {
        PendingKind::SetupToken => Some(claude_login::SETUP_TOKEN_EXPIRES_SECS),
        PendingKind::FullOauth => None,
        PendingKind::CodexOauth => return internal("unexpected codex pending on the claude route"),
    };
    let token_url = admin_token_url();
    let tokens = match claude_login::exchange_code(
        &state.http_client,
        code,
        &pending.state,
        &pending.verifier,
        &token_url,
        claude_login::MANUAL_REDIRECT_URL,
        expires_in,
    )
    .await
    {
        Ok(tokens) => tokens,
        // Log full detail server-side; keep the browser response deliberately
        // generic (never echo upstream detail, which may carry hints).
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: Claude token exchange failed");
            return bad_gateway("Claude token exchange failed");
        }
    };
    let account_uuid = tokens
        .account
        .as_ref()
        .map(|account| account.uuid.as_str())
        .filter(|uuid| !uuid.is_empty())
        .map(ToOwned::to_owned);
    if matches!(pending.kind, PendingKind::SetupToken) && account_uuid.is_none() {
        tracing::warn!(account = %name, "admin: Claude token exchange did not return an account UUID");
        return bad_gateway("Claude token exchange did not return an account UUID");
    }

    // Capture the pre-store identity before it is overwritten below, so a
    // reprovision that changes the upstream identity (A -> B) can clean up A's
    // now-orphaned health entry too, instead of leaving it stranded (only the
    // newly stored B was ever cleared previously).
    let old_identity_name = name.clone();
    let old_identity =
        tokio::task::spawn_blocking(move || claude_store::account_identity(&old_identity_name))
            .await
            .unwrap_or(None);

    let account_name = name.clone();
    let stored = match pending.kind {
        PendingKind::SetupToken => {
            let access_token = tokens.access_token;
            let account_uuid = account_uuid.expect("setup-token UUID validated above");
            tokio::task::spawn_blocking(move || {
                claude_store::store_setup_token(&account_name, &access_token, Some(&account_uuid))
            })
            .await
        }
        PendingKind::FullOauth => {
            let Some(refresh_token) = tokens
                .refresh_token
                .as_deref()
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(ToOwned::to_owned)
            else {
                tracing::warn!(account = %name, "admin: full Claude OAuth token exchange did not return a refresh token");
                return bad_gateway("Claude token exchange did not return a refresh token");
            };
            if account_uuid.is_none() {
                // Not fatal for OAuth accounts (unlike setup tokens), but mirror the
                // CLI's `persist_oauth_tokens` warning so an operator can see why the
                // account_uuid rewrite is skipped for a web-provisioned account.
                tracing::warn!(account = %name, "admin: full Claude OAuth token exchange did not return an account UUID; the account_uuid rewrite will be skipped for this account");
            }
            let access_token = tokens.access_token;
            let expires_at_ms = claude_login::oauth_expires_at_ms(tokens.expires_in);
            tokio::task::spawn_blocking(move || {
                claude_store::store_oauth_tokens(
                    &account_name,
                    &access_token,
                    &refresh_token,
                    expires_at_ms,
                    account_uuid.as_deref(),
                )
            })
            .await
        }
        PendingKind::CodexOauth => return internal("unexpected codex pending on the claude route"),
    };
    // The OAuth code is already consumed (single-use) by the time we get here, so
    // a persist failure is unrecoverable for this attempt — log the real cause
    // (disk full, permission denied, serialization) instead of swallowing it.
    match stored {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to persist account after successful token exchange");
            return internal("failed to store account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: account persistence task panicked");
            return internal("failed to store account");
        }
    }
    state.admin_stores.pending.remove(&name);
    // Re-provisioning reuses the account name; clear any process-lifetime Claude
    // pool health carried over for the newly stored upstream identity, and for
    // the identity it replaced (if any). Pool health is keyed by identity, not
    // name, and may be shared by other stored aliases, so only clear an
    // identity when no other stored account still resolves to it.
    let identity_name = name.clone();
    let new_identity = tokio::task::spawn_blocking(move || {
        claude_store::account_uuid(&identity_name).unwrap_or(identity_name)
    })
    .await
    .unwrap_or_else(|_| name.clone());
    let other_identities =
        remaining_account_identities(&name, claude_store::scan_accounts_strict).await;
    if other_identities.is_none() {
        tracing::warn!(account = %name, "admin: failed to scan Claude account store during reprovision cleanup; preserving dynamic-discovery-provider pool health for the old and new identities");
    }
    cleanup_reprovisioned_pool_health(
        &state,
        AuthMode::ClaudeOauth,
        old_identity.as_deref(),
        &new_identity,
        other_identities.as_ref(),
    );
    tracing::info!(account = %name, "admin: account stored");

    // Empty-accounts providers scan the store per request → live immediately;
    // otherwise the operator must add a name-only entry and reload.
    let live = state
        .config
        .providers
        .values()
        .any(|provider| provider.auth == AuthMode::ClaudeOauth && provider.accounts.is_empty());
    let message = match (pending.kind, live) {
        (PendingKind::SetupToken, true) => {
            "Account stored and live now (an empty-accounts provider scans the store each request)."
        }
        (PendingKind::SetupToken, false) => {
            "Account stored. Add a name-only [[providers.<name>.accounts]] entry and reload to activate it."
        }
        (PendingKind::FullOauth, true) => {
            "Refreshable OAuth login stored and live now (an empty-accounts provider scans the store each request)."
        }
        (PendingKind::FullOauth, false) => {
            "Refreshable OAuth login stored. Add a name-only [[providers.<name>.accounts]] entry and reload to activate it."
        }
        (PendingKind::CodexOauth, _) => {
            return internal("unexpected codex pending on the claude route")
        }
    };
    json_secure(json!({ "name": name, "stored": true, "live": live, "message": message }))
}

async fn remove_account_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    let Some(authok) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    if let Some(response) = check_csrf(&authok.kind, &headers) {
        return response;
    }
    if claude_store::validate_account_name(&name).is_err() {
        return bad_request("account name must match [a-z0-9-]+");
    }
    let target = name.clone();
    let removed = match tokio::task::spawn_blocking(move || {
        let identity = claude_store::account_uuid(&target).unwrap_or_else(|| target.clone());
        claude_store::remove_account(&target).map(|removed| (removed, identity))
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to remove account");
            return internal("failed to remove account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: remove_account task panicked");
            return internal("failed to remove account");
        }
    };
    let (removed, identity) = removed;
    tracing::info!(account = %name, removed, "admin: account removed");
    // Drop process-lifetime Claude pool health for the removed identity so a
    // later re-add does not inherit stale cooldown/quota state, without
    // touching Codex. Dynamic-discovery providers are only cleared once a
    // scan confirms no remaining stored alias still resolves to this identity
    // (a scan failure fails closed to preserving their health); explicitly
    // configured providers are checked against their own account list
    // regardless, since a store scan cannot see `credentials`/`token_env`
    // aliases at all (see `forget_pool_health_if_absent`).
    let store_scan_others = match tokio::task::spawn_blocking(claude_store::scan_accounts_strict)
        .await
    {
        Ok(Ok(remaining)) => Some(
            remaining
                .into_iter()
                .map(|account| crate::accounts::account_identity(&account).to_string())
                .collect::<std::collections::HashSet<String>>(),
        ),
        Ok(Err(error)) => {
            tracing::warn!(account = %name, %error, "admin: failed to scan Claude account store after removal; preserving dynamic-discovery-provider pool health for the removed identity");
            None
        }
        Err(join_error) => {
            tracing::warn!(account = %name, %join_error, "admin: Claude account store scan task panicked after removal; preserving dynamic-discovery-provider pool health for the removed identity");
            None
        }
    };
    forget_pool_health_if_absent(
        &state,
        AuthMode::ClaudeOauth,
        &identity,
        store_scan_others.as_ref(),
    );
    json_secure(json!({ "name": name, "removed": removed }))
}

/// The Claude token-exchange endpoint, honoring `SHUNT_CLAUDE_TOKEN_URL` (used by
/// `ClaudeAuthStore` and integration tests) so the completion flow is mockable.
///
/// The override is validated as an SSRF guard: only `https`, or `http` to a
/// loopback host (the integration tests point it at a local wiremock). Anything
/// else is ignored with a warning and the built-in endpoint is used instead.
fn admin_token_url() -> String {
    crate::auth::shared::admin_token_url_override("SHUNT_CLAUDE_TOKEN_URL", claude_auth::TOKEN_URL)
}

// --- response helpers ----------------------------------------------------------

pub(super) fn html_page(body: String) -> Response {
    html_body(body).into_response()
}

pub(super) fn html_body(body: String) -> Response {
    html_body_with_form_action(body, "'self'")
}

/// The login page, with the CSP `form-action` widened to the identity provider
/// when the SSO form is present: Chrome and WebKit enforce `form-action`
/// against the post-submission redirect chain (w3c/webappsec-csp#8), so the
/// strict `'self'` policy would block the
/// `POST /admin/oidc/start` -> `302` -> IdP hop. The discovered authorization
/// endpoint is not known when this page renders, so allow what
/// `idp_client::validate_endpoint` accepts: any `https` origin plus loopback
/// `http` (IPv6 loopback is not expressible as a CSP host-source).
pub(super) fn login_response(
    status: StatusCode,
    error: Option<&str>,
    sso_label: Option<&str>,
) -> Response {
    let form_action = if sso_label.is_some() {
        "'self' https: http://127.0.0.1:* http://localhost:*"
    } else {
        "'self'"
    };
    let mut response = html_body_with_form_action(html::login_page(error, sso_label), form_action);
    *response.status_mut() = status;
    response
}

fn html_body_with_form_action(body: String, form_action: &str) -> Response {
    // Defense-in-depth headers for the admin pages: a tight CSP (the pages use
    // only same-origin fetch plus inline script/style, no external resources),
    // clickjacking/sniffing guards, a conservative referrer policy, and
    // `no-store` so the session-specific CSRF token and account data are never
    // cached by the browser or a shared intermediary.
    let csp = format!(
        "default-src 'none'; script-src 'unsafe-inline'; \
style-src 'unsafe-inline'; connect-src 'self'; img-src 'self'; form-action {form_action}; \
base-uri 'none'; frame-ancestors 'none'"
    );
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::X_FRAME_OPTIONS, "DENY".to_string()),
            (
                header::REFERRER_POLICY,
                "strict-origin-when-cross-origin".to_string(),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

/// A JSON API response carrying admin data, with the same no-sniff / no-store
/// guards as the HTML pages — account metadata and pool state are sensitive and
/// must not be MIME-sniffed or cached by the browser or a shared intermediary.
pub(super) fn json_secure(value: serde_json::Value) -> Response {
    (
        [
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(value),
    )
        .into_response()
}

fn redirect(location: &'static str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

pub(super) fn unauthorized() -> Response {
    ShuntError::new(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "admin authentication required",
    )
    .into_response()
}

fn forbidden(message: &str) -> Response {
    ShuntError::new(StatusCode::FORBIDDEN, "permission_error", message).into_response()
}

pub(super) fn bad_request(message: &str) -> Response {
    ShuntError::new(StatusCode::BAD_REQUEST, "invalid_request_error", message).into_response()
}

pub(super) fn internal(message: &str) -> Response {
    ShuntError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", message).into_response()
}

pub(super) fn bad_gateway(message: &str) -> Response {
    ShuntError::bad_gateway(message).into_response()
}

pub(super) fn too_many_requests(message: &str) -> Response {
    ShuntError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message).into_response()
}

pub(super) fn not_found() -> Response {
    ShuntError::new(StatusCode::NOT_FOUND, "not_found_error", "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn parses_session_cookie() {
        let h = headers(&[("cookie", "a=1; shunt_admin_session=abc; b=2")]);
        assert_eq!(session_cookie(&h).as_deref(), Some("abc"));
        assert!(session_cookie(&headers(&[("cookie", "other=1")])).is_none());
        assert!(session_cookie(&HeaderMap::new()).is_none());
    }

    #[test]
    fn secure_cookie_off_for_loopback_only() {
        assert!(!secure_cookie(&headers(&[("host", "127.0.0.1:3001")])));
        assert!(!secure_cookie(&headers(&[("host", "localhost:3001")])));
        assert!(!secure_cookie(&headers(&[("host", "[::1]:3001")])));
        assert!(secure_cookie(&headers(&[("host", "shunt.example.com")])));
        assert!(secure_cookie(&headers(&[("host", "10.0.0.5:8080")])));
    }

    #[test]
    fn same_origin_honors_fetch_metadata_then_origin() {
        assert!(same_origin(&headers(&[("sec-fetch-site", "same-origin")])));
        assert!(same_origin(&headers(&[("sec-fetch-site", "none")])));
        assert!(!same_origin(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(same_origin(&headers(&[
            ("origin", "https://admin.example.com"),
            ("host", "admin.example.com"),
        ])));
        assert!(!same_origin(&headers(&[
            ("origin", "https://evil.example.com"),
            ("host", "admin.example.com"),
        ])));
        // Scheme default port on one side only still matches (proxy normalization).
        assert!(same_origin(&headers(&[
            ("origin", "https://admin.example.com:443"),
            ("host", "admin.example.com"),
        ])));
        assert!(same_origin(&headers(&[
            ("origin", "http://admin.example.com"),
            ("host", "admin.example.com:80"),
        ])));
        // A genuinely different (non-default) port is a distinct origin.
        assert!(!same_origin(&headers(&[
            ("origin", "https://admin.example.com:8443"),
            ("host", "admin.example.com:9000"),
        ])));
        assert!(same_origin(&headers(&[
            ("origin", "https://admin.example.com:8443"),
            ("host", "admin.example.com:8443"),
        ])));
        // No Origin at all (non-browser client): allowed; CSRF token still gates.
        assert!(same_origin(&HeaderMap::new()));
    }

    #[test]
    fn host_without_port_strips_port_and_ipv6_brackets() {
        assert_eq!(host_without_port("127.0.0.1:3001"), "127.0.0.1");
        assert_eq!(host_without_port("localhost"), "localhost");
        assert_eq!(host_without_port("[::1]:3001"), "::1");
        assert_eq!(host_without_port("[::1]"), "::1");
        assert_eq!(host_without_port("example.com"), "example.com");
    }

    fn explicit_account(name: &str, uuid: Option<&str>) -> crate::config::AccountConfig {
        crate::config::AccountConfig {
            name: name.to_string(),
            uuid: uuid.map(str::to_string),
            ..Default::default()
        }
    }

    fn state_with_explicit_provider(
        provider_name: &str,
        auth: AuthMode,
        accounts: Vec<crate::config::AccountConfig>,
        account_scope: Vec<String>,
    ) -> AppState {
        let mut providers = crate::config::ProvidersConfig::new();
        providers.insert(
            provider_name.to_string(),
            crate::config::ProviderConfig {
                kind: crate::config::ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                auth,
                api_key_env: None,
                api_key_header: Default::default(),
                effort: None,
                count_tokens: Default::default(),
                accounts,
                account_scope,
                websocket: false,
                tool_search: false,
                retry: Default::default(),
            },
        );
        let config = crate::config::Config {
            providers,
            ..crate::config::Config::default()
        };
        AppState::new(config, reqwest::Client::new()).unwrap()
    }

    // Regression test: an explicitly configured `[[providers.accounts]]` entry
    // must count as "still present" for its identity even when its name happens
    // to match the store-account name an admin call just changed/removed —
    // unlike a store-scanned alias, an explicit entry's identity never
    // re-derives from the store file, so it can never be "the same entry" the
    // caller is cleaning up after (see the doc comment on
    // `forget_pool_health_if_absent`).
    #[test]
    fn explicit_configured_account_liveness_is_not_excluded_by_name() {
        // The explicit account is deliberately named the same as the
        // store-account name that would have been passed as `changed_name`
        // before that parameter was removed, to prove the coincidence no
        // longer causes false exclusion.
        let account = explicit_account("alice", None); // identity falls back to "alice"
        let state = state_with_explicit_provider(
            "anthropic",
            AuthMode::ClaudeOauth,
            vec![account.clone()],
            Vec::new(),
        );
        state
            .accounts
            .cooldown("anthropic", &account, Duration::from_secs(60), "transport");

        forget_pool_health_if_absent(&state, AuthMode::ClaudeOauth, "alice", None);

        let snapshot = state.accounts.snapshot("anthropic", &[account], None, None);
        assert!(
            snapshot[0].has_state,
            "a live explicitly configured account's health must survive cleanup for a same-named store account"
        );
    }

    #[test]
    fn scoped_store_account_preserves_health_alongside_inline_accounts() {
        let inline = explicit_account("inline", Some("inline-uuid"));
        let scoped = explicit_account("stored", Some("stored-uuid"));
        let state = state_with_explicit_provider(
            "anthropic",
            AuthMode::ClaudeOauth,
            vec![inline],
            vec!["team-*".to_string()],
        );
        state
            .accounts
            .cooldown("anthropic", &scoped, Duration::from_secs(60), "transport");
        let stored_identities = HashSet::from(["stored-uuid".to_string()]);

        forget_pool_health_if_absent(
            &state,
            AuthMode::ClaudeOauth,
            "stored-uuid",
            Some(&stored_identities),
        );

        let snapshot = state.accounts.snapshot("anthropic", &[scoped], None, None);
        assert!(
            snapshot[0].has_state,
            "a scoped store identity must preserve shared health when inline accounts also exist"
        );
    }

    #[test]
    fn explicit_configured_provider_still_clears_health_for_a_genuinely_absent_identity() {
        let live = explicit_account("bob", Some("bob-uuid"));
        let state = state_with_explicit_provider(
            "anthropic",
            AuthMode::ClaudeOauth,
            vec![live.clone()],
            Vec::new(),
        );
        // Seed health for an identity that is not among "configured"'s accounts.
        let orphan = explicit_account("orphan", Some("orphan-uuid"));
        state
            .accounts
            .cooldown("anthropic", &orphan, Duration::from_secs(60), "transport");
        state
            .accounts
            .cooldown("anthropic", &live, Duration::from_secs(60), "transport");

        forget_pool_health_if_absent(&state, AuthMode::ClaudeOauth, "orphan-uuid", None);

        let orphan_snapshot = state.accounts.snapshot("anthropic", &[orphan], None, None);
        assert!(
            !orphan_snapshot[0].has_state,
            "an identity absent from every configured account must still be cleared"
        );
        let live_snapshot = state.accounts.snapshot("anthropic", &[live], None, None);
        assert!(
            live_snapshot[0].has_state,
            "a different, still-configured identity's health must be untouched"
        );
    }
}
