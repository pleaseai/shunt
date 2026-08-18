//! Discovery, refresh, and token resolution for a logged-in shunt gateway.
//!
//! Endpoints are never string-concatenated onto the deployment's base URL: they
//! come from `GET {base}/.well-known/oauth-authorization-server`, which is also
//! how a URL that is not a shunt gateway — or one whose `[server.gateway]`
//! section is missing — is detected up front rather than as a puzzling 404 on
//! the token POST.
//!
//! *** CRITICAL: the gateway answers the ordinary, non-terminal device-poll
//! responses (`authorization_pending`, `slow_down`) with **HTTP 400**, and a
//! dead refresh token with **HTTP 401** — neither is distinguishable from a
//! real failure by status alone. Never branch on the HTTP status before parsing
//! the body: parse a token or an OAuth error envelope out of the body first,
//! and fall back to a bare-status message only when the body carries neither.
//! [`super::login`] polls under the same rule.

use std::{
    borrow::Cow,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use serde_json::Value;

use super::store::{self, GatewaySession};

/// Pinned by the server: [`crate::gateway::oauth`] answers any other client id
/// with a 400, so this is not configurable.
pub(crate) const CLIENT_ID: &str = "claude-code";
pub(crate) const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";
const DISCOVERY_PATH: &str = "/.well-known/oauth-authorization-server";
/// The `gateway_protocol_version` this client was written against.
const SUPPORTED_PROTOCOL_VERSION: u64 = 1;
/// Refresh this far ahead of the stored expiry, matching the Claude and Kimi
/// stores: a token that expires mid-request is a failed request.
const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);
/// Fallback lifetime when a token response omits `expires_in` (the gateway's
/// own `token_ttl_seconds` default).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

#[derive(Debug)]
pub(crate) struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

pub(crate) struct TokenResponse {
    pub access_token: String,
    /// Required, not optional: the gateway rotates the refresh token on every
    /// grant and the old one is single-use, so a response without a replacement
    /// would leave the session unable to refresh again.
    pub refresh_token: String,
    pub expires_in: Option<i64>,
}

/// Redacting, deliberately: this type holds a live token pair, and the derived
/// form would print both through any `unwrap`/`expect_err` panic message.
impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Fetch the deployment's OAuth metadata.
pub(crate) async fn discover(
    client: &reqwest::Client,
    gateway_url: &str,
) -> anyhow::Result<Discovery> {
    let url = format!("{}{DISCOVERY_PATH}", gateway_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await
        .with_context(|| format!("failed to reach {url}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(discovery) = parse_discovery(&value) {
        warn_on_unknown_protocol_version(&value);
        return Ok(discovery);
    }
    // A shunt deployment only registers this route when `[server.gateway]` is
    // configured, so a 404 here is the single most likely misconfiguration —
    // report the cause instead of the bare status.
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "{gateway_url} has no OAuth discovery document at {DISCOVERY_PATH} (HTTP 404). \
             The deployment is most likely missing a `[server.gateway]` section in its \
             shunt config, or this URL is not a shunt gateway"
        );
    }
    bail!(
        "unexpected response from {url} (HTTP {status}): {}",
        truncate_for_error(&text)
    )
}

fn parse_discovery(value: &Value) -> Option<Discovery> {
    let endpoint = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    Some(Discovery {
        device_authorization_endpoint: endpoint("device_authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
    })
}

/// A newer gateway is still usable — every field this client reads is present
/// in version 1 — so an unknown version is a note, not a refusal.
fn warn_on_unknown_protocol_version(value: &Value) {
    if let Some(version) = value
        .get("gateway_protocol_version")
        .and_then(Value::as_u64)
    {
        if version != SUPPORTED_PROTOCOL_VERSION {
            eprintln!(
                "Note: this gateway speaks protocol version {version}; shunt was built against \
                 version {SUPPORTED_PROTOCOL_VERSION}. Upgrade shunt if the login misbehaves."
            );
        }
    }
}

pub(crate) fn parse_token_response(value: &Value) -> Option<TokenResponse> {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    Some(TokenResponse {
        access_token: field("access_token")?,
        refresh_token: field("refresh_token")?,
        expires_in: value.get("expires_in").and_then(Value::as_i64),
    })
}

/// Exchange the stored refresh token for a fresh pair.
async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    refresh_token: &str,
) -> anyhow::Result<TokenResponse> {
    let response = client
        .post(token_endpoint)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", REFRESH_GRANT_TYPE),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .with_context(|| format!("failed to reach {token_endpoint}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    // Body before status: a rejected refresh is HTTP 401, which says nothing
    // about *why* on its own.
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(tokens) = parse_token_response(&value) {
        return Ok(tokens);
    }
    match value.get("error").and_then(Value::as_str) {
        Some("invalid_grant") => bail!(
            "the gateway rejected the stored refresh token (invalid_grant). It expired, or \
             another session already used it — refresh tokens are single-use. Run \
             `shunt gateway login <url>` to sign in again"
        ),
        Some(error) => bail!("gateway token refresh failed ({error})"),
        None => bail!(
            "invalid gateway token response (HTTP {status}): {}",
            truncate_for_error(&text)
        ),
    }
}

/// Absolute expiry for a freshly issued token, in epoch milliseconds.
pub(crate) fn expires_at_ms(expires_in: Option<i64>, now: SystemTime) -> i64 {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    // Saturate rather than overflow: a pathological `expires_in` must not panic
    // under overflow checks, nor wrap into a negative `expiresAt` that reads as
    // permanently expired and drives a refresh on every single call.
    now_ms.saturating_add(
        expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
            .saturating_mul(1000),
    )
}

fn is_valid_at(session: &GatewaySession, now: SystemTime) -> bool {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    session.expires_at_ms.saturating_sub(now_ms) > EXPIRY_BUFFER.as_millis() as i64
}

/// Cap a raw upstream body before it reaches an error message: a proxy or WAF
/// can answer with a whole HTML page. Truncates on a char boundary.
pub(crate) fn truncate_for_error(text: &str) -> Cow<'_, str> {
    const LIMIT: usize = 200;
    match text.char_indices().nth(LIMIT) {
        Some((byte_index, _)) => Cow::Owned(format!("{}\u{2026}", &text[..byte_index])),
        None => Cow::Borrowed(text),
    }
}

/// `shunt gateway token`: the stored access token, refreshed first if it is
/// inside the expiry buffer.
pub async fn resolve_token() -> anyhow::Result<String> {
    resolve_token_at(&store::session_path()).await
}

pub(crate) async fn resolve_token_at(path: &Path) -> anyhow::Result<String> {
    let session = read_session_or_bail(path)?;
    if is_valid_at(&session, SystemTime::now()) {
        return Ok(session.access_token);
    }

    // Cross-process single flight. Claude Code runs `apiKeyHelper` per session,
    // so the losing racer's replay of a single-use refresh token would revoke
    // the whole family; re-reading under the lock means the waiter usually just
    // picks up the token the winner already persisted and makes no call at all.
    let _lock = store::lock_session(path).await?;
    let session = read_session_or_bail(path)?;
    if is_valid_at(&session, SystemTime::now()) {
        return Ok(session.access_token);
    }

    let client = crate::auth::shared::token_refresh_client();
    let discovery = discover(&client, &session.gateway_url).await?;
    let tokens = refresh(&client, &discovery.token_endpoint, &session.refresh_token).await?;
    let refreshed = GatewaySession {
        gateway_url: session.gateway_url,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: expires_at_ms(tokens.expires_in, SystemTime::now()),
    };
    // Hard failure, deliberately: the refresh token just presented is already
    // burned, so returning the access token while losing its replacement would
    // strand the user at the next refresh with no way back but a fresh login.
    store::write_session(path, &refreshed).context(
        "the gateway issued a new token pair but it could not be saved; the previous refresh \
         token is now spent, so run `shunt gateway login <url>` to sign in again",
    )?;
    Ok(refreshed.access_token)
}

fn read_session_or_bail(path: &Path) -> anyhow::Result<GatewaySession> {
    store::read_session(path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no gateway session at {}; run `shunt gateway login <url>` first",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::gateway::store::{temp_dir, test_session};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn now_plus_ms(seconds: i64) -> i64 {
        expires_at_ms(Some(seconds), SystemTime::now())
    }

    async fn mount_discovery(server: &MockServer) {
        let token_endpoint = format!("{}/oauth/token", server.uri());
        Mock::given(method("GET"))
            .and(path(DISCOVERY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": server.uri(),
                "device_authorization_endpoint": format!("{}/oauth/device_authorization", server.uri()),
                "token_endpoint": token_endpoint,
                "grant_types_supported": [DEVICE_CODE_GRANT_TYPE, "refresh_token"],
                "response_types_supported": [],
                "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ["openid", "profile", "email"],
                "gateway_protocol_version": 1
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn discovery_404_blames_the_missing_server_gateway_section() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DISCOVERY_PATH))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let error = discover(&client, &server.uri())
            .await
            .expect_err("a 404 discovery must not be treated as a usable gateway");
        let message = error.to_string();
        assert!(
            message.contains("[server.gateway]"),
            "the 404 must name the likely cause: {message}"
        );
    }

    #[tokio::test]
    async fn discovery_uses_the_documents_endpoints_not_concatenated_paths() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DISCOVERY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_authorization_endpoint": "https://elsewhere.example/da",
                "token_endpoint": "https://elsewhere.example/tok"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        // A trailing slash on the base URL must not double up in the path, and
        // the endpoints must come from the document rather than from the base.
        let discovery = discover(&client, &format!("{}/", server.uri()))
            .await
            .expect("discovery should parse");
        assert_eq!(discovery.token_endpoint, "https://elsewhere.example/tok");
        assert_eq!(
            discovery.device_authorization_endpoint,
            "https://elsewhere.example/da"
        );
    }

    #[tokio::test]
    async fn refresh_persists_the_rotated_refresh_token() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let dir = temp_dir("refresh-rotate");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let token = resolve_token_at(&session_path).await.unwrap();
        assert_eq!(token, "access-2");

        // The rotated token is single-use upstream: what is on disk afterwards
        // must be the new one, or the next refresh replays a spent token and
        // revokes the whole family.
        let stored = store::read_session(&session_path).unwrap().unwrap();
        assert_eq!(stored.refresh_token, "refresh-2");
        assert_eq!(stored.access_token, "access-2");
        assert!(is_valid_at(&stored, SystemTime::now()));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refresh_401_invalid_grant_points_at_a_fresh_login() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant"})),
            )
            .mount(&server)
            .await;

        let dir = temp_dir("refresh-denied");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let error = resolve_token_at(&session_path)
            .await
            .expect_err("a revoked refresh token must not resolve");
        let message = error.to_string();
        assert!(message.contains("shunt gateway login"), "got: {message}");
        // The stored session is left alone: nothing was rotated.
        let stored = store::read_session(&session_path).unwrap().unwrap();
        assert_eq!(stored.refresh_token, "refresh-1");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_token_outside_the_expiry_buffer_makes_no_http_calls() {
        // No mocks are mounted at all: any request would 404 and fail the
        // refresh, but the assertion below is on the request log itself so the
        // test fails even if a stray call somehow succeeded.
        let server = MockServer::start().await;

        let dir = temp_dir("cached");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(3600);
        store::write_session(&session_path, &session).unwrap();

        let token = resolve_token_at(&session_path).await.unwrap();
        assert_eq!(token, "access-1");
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(
            requests.is_empty(),
            "a still-valid token must not touch the network, got {} request(s)",
            requests.len()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_token_inside_the_expiry_buffer_refreshes_before_it_expires() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-buffered",
                "refresh_token": "refresh-buffered",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let dir = temp_dir("buffered");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        // Still valid by the wire clock, but inside the 5-minute buffer: it
        // would expire mid-request, so it must be refreshed now.
        session.expires_at_ms = now_plus_ms(60);
        store::write_session(&session_path, &session).unwrap();

        let token = resolve_token_at(&session_path).await.unwrap();
        assert_eq!(token, "access-buffered");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolving_without_a_session_says_how_to_log_in() {
        let dir = temp_dir("absent");
        let error = resolve_token_at(&dir.join("session.json"))
            .await
            .expect_err("no session means no token");
        assert!(
            error.to_string().contains("shunt gateway login"),
            "got: {error}"
        );
    }

    #[test]
    fn parse_token_response_requires_both_tokens() {
        assert!(parse_token_response(&json!({
            "access_token": "a",
            "refresh_token": "r",
            "expires_in": 3600
        }))
        .is_some());
        // A rotation-less response would leave the session unable to refresh
        // again, so it is rejected rather than silently reusing the old token.
        assert!(parse_token_response(&json!({"access_token": "a"})).is_none());
        assert!(parse_token_response(&json!({"access_token": "", "refresh_token": "r"})).is_none());
    }

    #[test]
    fn expiry_buffer_governs_validity_and_expires_at_saturates() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let mut session = test_session("https://gateway.example", 0);

        session.expires_at_ms = 10_000_000 + EXPIRY_BUFFER.as_millis() as i64 + 1;
        assert!(
            is_valid_at(&session, now),
            "just outside the buffer is valid"
        );
        session.expires_at_ms = 10_000_000 + EXPIRY_BUFFER.as_millis() as i64;
        assert!(
            !is_valid_at(&session, now),
            "exactly at the buffer refreshes"
        );

        assert_eq!(expires_at_ms(Some(120), now), 10_000_000 + 120_000);
        assert_eq!(
            expires_at_ms(None, now),
            10_000_000 + DEFAULT_EXPIRES_IN_SECS * 1000
        );
        assert_eq!(expires_at_ms(Some(i64::MAX), now), i64::MAX);
    }
}
