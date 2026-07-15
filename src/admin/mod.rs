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

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::{
        claude::{auth as claude_auth, login as claude_login, store as claude_store},
        inbound::{constant_time_eq, InboundAuth},
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
}

impl AdminAuth {
    pub fn new(inbound: InboundAuth, session_ttl: Duration, pending_ttl: Duration) -> Self {
        Self {
            inbound,
            session_ttl,
            pending_ttl,
        }
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
        .route("/admin/logout", post(logout))
        .route("/admin/accounts", get(list_accounts))
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

/// Drop process-lifetime pool health for `account` across every provider using
/// `auth`, so re-provisioning does not inherit stale cooldown/quota state — while
/// leaving other auth modes' identically-named accounts untouched (the Claude and
/// Codex stores independently allow the same account name).
pub(super) fn forget_pool_health(state: &AppState, auth: AuthMode, account: &str) {
    for (provider_name, provider) in &state.config.providers {
        if provider.auth == auth {
            state.accounts.forget_account(provider_name, account);
        }
    }
}

/// Same-origin check: prefer Fetch Metadata (`Sec-Fetch-Site`), fall back to
/// comparing the `Origin` authority to `Host`. A missing `Origin` (non-browser)
/// is allowed — the CSRF token and `SameSite=Strict` cookie still gate the call.
fn same_origin(headers: &HeaderMap) -> bool {
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
fn secure_cookie(headers: &HeaderMap) -> bool {
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

fn set_cookie(sid: &str, secure: bool, ttl: Duration) -> String {
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
    html_page(html::login_page(None))
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
        return (
            StatusCode::UNAUTHORIZED,
            html_body(html::login_page(Some("Invalid admin token."))),
        )
            .into_response();
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

async fn pool(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    let config = state.config.clone();
    let accounts = state.accounts.clone();
    // scan_accounts does file I/O and snapshot locks a std mutex; run off the
    // async workers. Model is unset — the dashboard governs weekly quota on 7d.
    let result = tokio::task::spawn_blocking(move || {
        let mut providers = Vec::new();
        for (name, provider) in &config.providers {
            if !matches!(
                provider.auth,
                AuthMode::ClaudeOauth | AuthMode::ChatgptOauth
            ) {
                continue;
            }
            let resolved = if provider.accounts.is_empty() {
                // Surface a store read failure as an error (5xx) instead of an
                // empty pool: a permission/I/O problem must not masquerade as
                // "no accounts configured" on the dashboard.
                let scanned = match provider.auth {
                    AuthMode::ClaudeOauth => claude_store::scan_accounts(),
                    AuthMode::ChatgptOauth => crate::auth::codex::store::scan_accounts(),
                    _ => unreachable!("provider auth filtered above"),
                };
                scanned.map_err(|error| {
                    tracing::error!(provider = %name, %error, "admin: failed to scan accounts store");
                    format!("failed to scan accounts store for provider {name}")
                })?
            } else {
                provider.accounts.clone()
            };
            let snapshots = accounts.snapshot(name, &resolved, None, config.server.pool.as_ref());
            providers.push(json!({ "provider": name, "accounts": snapshots }));
        }
        Ok::<_, String>(providers)
    })
    .await;
    match result {
        Ok(Ok(providers)) => json_secure(json!({ "providers": providers })),
        Ok(Err(_)) => internal("failed to read pool state"),
        Err(join_error) => {
            tracing::error!(%join_error, "admin: pool snapshot task panicked");
            internal("failed to read pool state")
        }
    }
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
    // Re-provisioning reuses the account name; clear any process-lifetime pool
    // health carried over from a prior Claude token so the fresh credential is
    // not treated as cooling/near-quota, without touching same-named Codex health.
    forget_pool_health(&state, AuthMode::ClaudeOauth, &name);
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
    let removed = match tokio::task::spawn_blocking(move || claude_store::remove_account(&target))
        .await
    {
        Ok(Ok(removed)) => removed,
        Ok(Err(error)) => {
            tracing::error!(account = %name, %error, "admin: failed to remove account");
            return internal("failed to remove account");
        }
        Err(join_error) => {
            tracing::error!(account = %name, %join_error, "admin: remove_account task panicked");
            return internal("failed to remove account");
        }
    };
    tracing::info!(account = %name, removed, "admin: account removed");
    // Drop process-lifetime Claude pool health for the removed name so a later
    // re-add does not inherit stale cooldown/quota state, without touching Codex.
    forget_pool_health(&state, AuthMode::ClaudeOauth, &name);
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

fn html_page(body: String) -> Response {
    html_body(body).into_response()
}

fn html_body(body: String) -> Response {
    // Defense-in-depth headers for the admin pages: a tight CSP (the pages use
    // only same-origin fetch plus inline script/style, no external resources),
    // clickjacking/sniffing guards, a conservative referrer policy, and
    // `no-store` so the session-specific CSRF token and account data are never
    // cached by the browser or a shared intermediary.
    const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
style-src 'unsafe-inline'; connect-src 'self'; img-src 'self'; form-action 'self'; \
base-uri 'none'; frame-ancestors 'none'";
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
            (header::CACHE_CONTROL, "no-store"),
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

fn not_found() -> Response {
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
}
