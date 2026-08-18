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
/// Upper clamp on a gateway-supplied `expires_in`.
///
/// Nothing hostile is needed to hit this: a gateway that reports `expires_in`
/// in *milliseconds* caches a token as valid for ~41 days, so `resolve_token_at`
/// takes the fast path forever, and once the real token is revoked Claude Code
/// 401s, re-runs the helper, gets the same dead token back, and loops — with no
/// recovery short of deleting `session.json` by hand.
///
/// A week is far above the gateway's own one-hour default and above any
/// plausible *access*-token lifetime (longevity is the refresh token's job).
/// Clamping is safe even where it is wrong: a too-short cached expiry only
/// triggers an earlier refresh, which succeeds — it can never turn a working
/// session into a failing one.
const MAX_EXPIRES_IN_SECS: i64 = 7 * 24 * 60 * 60;
/// Bound on a single gateway round-trip taken while the refresh lock is held.
///
/// Deliberately applied here with `tokio::time::timeout` rather than as
/// `reqwest`'s `.timeout()`: that one is process-wide on the shared refresh
/// client and covers the response *body*, which is why streaming paths in this
/// repo must never set it. These two calls exchange small JSON documents, so
/// bounding the whole call is right for them and only for them.
pub(crate) const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
/// Claude Code 2.1.234 trims `apiKeyHelper` stdout and then rejects the value
/// outright if it holds a line break, a NUL, a space or tab, any other control
/// character, or any byte above 126.
pub const MAX_HELPER_OUTPUT: usize = 16_384;

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
    // Propagate rather than defaulting to "": a body that could not be read is
    // a local read failure, and reporting it as an empty document would blame
    // the gateway for sending garbage it never sent.
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read the discovery response from {url}"))?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match parse_discovery(&value) {
        Ok(discovery) => {
            warn_on_unknown_protocol_version(&value);
            return Ok(discovery);
        }
        Err(DiscoveryProblem::Unsafe(endpoint)) => bail!(
            "{url} advertised {endpoint}, which is neither https nor http to a loopback address. \
             shunt will not send the device code or the refresh token over that transport"
        ),
        // An absent endpoint is not a discovery document at all, so it falls
        // through to the 404 / bare-status reporting below.
        Err(DiscoveryProblem::Absent) => {}
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
        sanitize_for_error(&text)
    )
}

/// Why a discovery document yielded no usable [`Discovery`]. The two cases are
/// deliberately distinct: an absent endpoint means "not a discovery document",
/// while an unsafe one is a document shunt understood and refused.
enum DiscoveryProblem {
    Absent,
    /// The offending endpoint, already named and truncated for an error message.
    Unsafe(String),
}

fn parse_discovery(value: &Value) -> Result<Discovery, DiscoveryProblem> {
    let endpoint = |name: &str| -> Result<String, DiscoveryProblem> {
        let raw = value
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(DiscoveryProblem::Absent)?;
        // The device code and the long-lived refresh token are POSTed to
        // whichever URL this document names, and the document comes from the
        // network. `token_refresh_client()`'s hardened policy only inspects
        // *redirect* targets, so with no redirect in play it never fires and
        // this is the only place the transport floor can be applied at all.
        // Same predicate as that policy, not a second copy of it.
        if !raw
            .parse::<reqwest::Url>()
            .is_ok_and(|url| crate::auth::shared::is_safe_refresh_url(&url))
        {
            // `{:?}` rather than `sanitize_for_error`: Debug already escapes
            // control characters, and it quotes the endpoint so an empty or
            // whitespace value is still visible in the message.
            return Err(DiscoveryProblem::Unsafe(format!(
                "{name} {:?}",
                truncate_for_error(raw)
            )));
        }
        Ok(raw.to_string())
    };
    Ok(Discovery {
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
    // Propagate rather than defaulting to "": an unreadable body is a local
    // read failure, not a malformed token response, and the two have different
    // remedies.
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read the token response from {token_endpoint}"))?;
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
        Some(error) => bail!(
            "gateway token refresh failed ({})",
            sanitize_for_error(error)
        ),
        None => bail!(
            "invalid gateway token response (HTTP {status}): {}",
            sanitize_for_error(&text)
        ),
    }
}

/// Absolute expiry for a freshly issued token, in epoch milliseconds.
pub(crate) fn expires_at_ms(expires_in: Option<i64>, now: SystemTime) -> i64 {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    // Clamp before scaling. Saturating alone only stops the panic and the
    // negative wrap; it still honors an absurd positive, which is the case that
    // strands the user on a dead token (see [`MAX_EXPIRES_IN_SECS`]).
    let expires_in = expires_in
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
        .min(MAX_EXPIRES_IN_SECS);
    now_ms.saturating_add(expires_in.saturating_mul(1000))
}

/// Whether `value` survives Claude Code's `apiKeyHelper` validation.
///
/// The production definition, deliberately: this rule used to live only in
/// `tests/gateway_cli.rs`, where it asserted things about output nothing
/// enforced. A token that fails it produces an opaque auth failure with no hint
/// the token was the problem, so it is checked before the token is handed out.
pub fn is_helper_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HELPER_OUTPUT
        && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

/// Gate a token on [`is_helper_safe`], naming the gateway that issued it.
fn helper_safe_token(token: String, gateway_url: &str) -> anyhow::Result<String> {
    if is_helper_safe(&token) {
        return Ok(token);
    }
    bail!(
        "{gateway_url} issued an access token that Claude Code's apiKeyHelper will reject: it          must be 1..={MAX_HELPER_OUTPUT} characters of printable ASCII with no whitespace.          Printing it would fail authentication with no diagnostic, so it is refused here instead"
    )
}

/// Whether the token has not *actually* expired yet, ignoring the refresh
/// buffer. The buffer decides when to refresh; this decides whether a token is
/// still usable when a refresh could not be completed.
fn is_unexpired_at(session: &GatewaySession, now: SystemTime) -> bool {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    session.expires_at_ms > now_ms
}

fn is_valid_at(session: &GatewaySession, now: SystemTime) -> bool {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    session.expires_at_ms.saturating_sub(now_ms) > EXPIRY_BUFFER.as_millis() as i64
}

/// Bidirectional overrides (U+202A–U+202E) and isolates (U+2066–U+2069).
///
/// Deliberately these two ranges rather than the whole `Cf` category
/// [`char::is_control`] misses. `Cf` also holds ZWNJ (U+200C), ZWJ (U+200D),
/// and the soft hyphen (U+00AD), which are load-bearing for correct rendering
/// of Arabic, Persian, and Indic text — and an `error_description` can
/// legitimately be prose in those scripts, so replacing the category wholesale
/// would mangle it. The overrides and isolates have no legitimate use in a URL,
/// a device code, or an error message; the joiners do.
fn is_bidi_control(character: char) -> bool {
    matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Strip characters that let a gateway control how its string *renders* before
/// printing it.
///
/// [`truncate_for_error`] caps length but removes nothing, so an escape
/// sequence in a verification URL, a user code, or an `error_description`
/// would be interpreted by the terminal — repainting the screen, or hiding the
/// URL that was actually opened. This is what makes
/// `login::browser_open_refusal`'s "the URL is printed either way" promise
/// worth anything: refusing to auto-open only helps if the printed string is
/// what the user actually reads.
///
/// Two classes, because [`char::is_control`] is general category `Cc` only and
/// stops at U+001B. A bidi override is `Cf`, passes that test, and makes the
/// printed URL *display* as something other than what it is — the Trojan Source
/// class (CVE-2021-42574) — which defeats the printed fallback just as
/// thoroughly as an escape sequence.
pub(crate) fn sanitize_for_terminal(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || is_bidi_control(character) {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

/// Make a gateway-chosen string safe to put in an error message: escape first,
/// then cap.
///
/// The order is defensive, not currently observable: [`sanitize_for_terminal`]
/// replaces one character with one character, so it cannot change the character
/// count [`truncate_for_error`] measures, and either order gives the same
/// result today. It matters the moment the escaping becomes a *lengthening*
/// one — `\x1b` rendered as four literal characters rather than one U+FFFD —
/// because a cap applied first would then be exceeded by the escaped form.
/// Escaping first is the order that stays correct through that change.
///
/// Neither step can split a multi-byte character: [`truncate_for_error`] counts
/// characters and slices on a character boundary, and the replacement is
/// character-to-character.
pub(crate) fn sanitize_for_error(text: &str) -> String {
    truncate_for_error(&sanitize_for_terminal(text)).into_owned()
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
    resolve_token_bounded(path, NETWORK_TIMEOUT).await
}

/// [`resolve_token_at`] with an explicit per-call network bound, so tests can
/// drive the timeout path without waiting [`NETWORK_TIMEOUT`] out.
pub(crate) async fn resolve_token_bounded(
    path: &Path,
    network_timeout: Duration,
) -> anyhow::Result<String> {
    let session = read_session_or_bail(path)?;
    if is_valid_at(&session, SystemTime::now()) {
        return helper_safe_token(session.access_token, &session.gateway_url);
    }

    // Cross-process single flight. Claude Code runs `apiKeyHelper` per session,
    // so the losing racer's replay of a single-use refresh token would revoke
    // the whole family; re-reading under the lock means the waiter usually just
    // picks up the token the winner already persisted and makes no call at all.
    let _lock = store::lock_session(path).await?;
    let session = read_session_or_bail(path)?;
    if is_valid_at(&session, SystemTime::now()) {
        return helper_safe_token(session.access_token, &session.gateway_url);
    }

    match refresh_session(path, &session, network_timeout).await {
        Ok(token) => helper_safe_token(token, &session.gateway_url),
        // Crossing into the expiry buffer makes a refresh *due*, not mandatory:
        // the token is still good for up to EXPIRY_BUFFER, so a two-second
        // network blip four minutes before real expiry must not become a
        // user-visible auth failure. Once the token has genuinely expired this
        // still fails hard — a dead token returned as a success would only move
        // the error somewhere less legible.
        Err(error) if is_unexpired_at(&session, SystemTime::now()) => {
            eprintln!(
                "Warning: could not refresh the gateway token ({error}); serving the cached one, \
                 which is still valid but expires shortly. This will fail once it does."
            );
            helper_safe_token(session.access_token, &session.gateway_url)
        }
        Err(error) => Err(error),
    }
}

/// Whether a stored gateway URL sends its traffic in the clear.
///
/// Mirrors the check `login::normalize_gateway_url` applies at sign-in: plain
/// `http` to anything but loopback.
pub(crate) fn is_plaintext_gateway(gateway_url: &str) -> bool {
    gateway_url.parse::<reqwest::Url>().is_ok_and(|url| {
        url.scheme() == "http"
            && !crate::config::host_is_loopback(url.host_str().unwrap_or_default())
    })
}

/// The refresh critical section: called with the session lock held.
async fn refresh_session(
    path: &Path,
    session: &GatewaySession,
    network_timeout: Duration,
) -> anyhow::Result<String> {
    // The login-time warning promises this exposure continues "on every token
    // refresh for as long as the session lives" — so it has to be said on the
    // refreshes too, or the docs describe a behavior the code does not have.
    //
    // Here rather than in `resolve_token_bounded`: the fast path serves a
    // cached token with no network traffic at all, and warning there would fire
    // on every single helper invocation. Tied to an actual refresh, this is
    // roughly once per token lifetime.
    //
    // stderr, never stdout: stdout is the apiKeyHelper contract.
    if is_plaintext_gateway(&session.gateway_url) {
        eprintln!(
            "Warning: refreshing against {} over plain HTTP; the refresh token and the new \
             access token travel unencrypted.",
            session.gateway_url
        );
    }
    let client = crate::auth::shared::token_refresh_client();
    let discovery = bounded(
        network_timeout,
        discover(&client, &session.gateway_url),
        "discovery",
        &session.gateway_url,
    )
    .await?;
    let tokens = bounded(
        network_timeout,
        refresh(&client, &discovery.token_endpoint, &session.refresh_token),
        "token refresh",
        &discovery.token_endpoint,
    )
    .await?;
    let refreshed = GatewaySession {
        gateway_url: session.gateway_url.clone(),
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

/// Bound one gateway round-trip. A gateway that completes the TCP handshake and
/// then never answers would otherwise hold the session lock forever, and every
/// other `apiKeyHelper` on the machine blocks behind it.
pub(crate) async fn bounded<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    what: &str,
    target: &str,
) -> anyhow::Result<T> {
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => bail!("gateway {what} against {target} did not answer within {timeout:?}"),
    }
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
    async fn a_discovery_document_naming_a_plaintext_endpoint_is_refused() {
        // The exfiltration path this closes: `token_refresh_client()` vets only
        // *redirect* targets, so a document that simply names an off-host
        // plaintext endpoint is never seen by that policy and the refresh token
        // would be POSTed there in the clear on the very first hop.
        for (field, other) in [
            ("token_endpoint", "device_authorization_endpoint"),
            ("device_authorization_endpoint", "token_endpoint"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(DISCOVERY_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    field: "http://collector.example/tok",
                    other: "https://elsewhere.example/ok"
                })))
                .mount(&server)
                .await;

            let error = match discover(&reqwest::Client::new(), &server.uri()).await {
                Ok(discovery) => {
                    panic!("{field} over plaintext must not be usable, got {discovery:?}")
                }
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(
                message.contains(field),
                "the cause must be named: {message}"
            );
            assert!(
                message.contains("http://collector.example/tok"),
                "the offending endpoint must be shown: {message}"
            );
            assert!(
                message.contains("loopback"),
                "the message must say what the floor is: {message}"
            );
        }
    }

    #[tokio::test]
    async fn the_transport_floor_still_allows_loopback_http() {
        // The floor is transport, not origin: a locally hosted deployment is a
        // supported setup, and so is a cross-host endpoint over https (see
        // `discovery_uses_the_documents_endpoints_not_concatenated_paths`).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DISCOVERY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_authorization_endpoint": "http://127.0.0.1:3001/oauth/device_authorization",
                "token_endpoint": "http://localhost:3001/oauth/token"
            })))
            .mount(&server)
            .await;

        let discovery = discover(&reqwest::Client::new(), &server.uri())
            .await
            .expect("plain http to loopback must stay usable");
        assert_eq!(
            discovery.token_endpoint,
            "http://localhost:3001/oauth/token"
        );
    }

    #[tokio::test]
    async fn a_refresh_is_never_sent_to_a_plaintext_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DISCOVERY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_authorization_endpoint": "https://gateway.example/da",
                "token_endpoint": "http://collector.example/tok"
            })))
            .mount(&server)
            .await;

        let dir = temp_dir("plaintext-endpoint");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let error = resolve_token_at(&session_path)
            .await
            .expect_err("the stored refresh token must not leave over plaintext");
        assert!(error.to_string().contains("token_endpoint"), "got: {error}");
        // The session is untouched: nothing was rotated, so a later login is
        // not required just because one discovery document was hostile.
        let stored = store::read_session(&session_path).unwrap().unwrap();
        assert_eq!(stored.refresh_token, "refresh-1");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The invariant the file lock exists for: the gateway's refresh tokens are
    /// single-use, so two concurrent resolvers must produce exactly one refresh
    /// between them. Without the lock the loser replays a spent token, which
    /// revokes the whole rotation family.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resolvers_perform_exactly_one_refresh() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        // The endpoint accepts `refresh-1` once. The delay is load-bearing: it
        // holds the winner inside the critical section long enough that the
        // second caller is guaranteed to arrive while the lock is held, rather
        // than merely usually.
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_json(json!({
                        "access_token": "access-2",
                        "refresh_token": "refresh-2",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Any second presentation of the spent token is rejected, exactly as
        // the real gateway rejects it.
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant"})),
            )
            .mount(&server)
            .await;

        let dir = temp_dir("concurrent-refresh");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let first = tokio::spawn({
            let path = session_path.clone();
            async move { resolve_token_at(&path).await }
        });
        let second = tokio::spawn({
            let path = session_path.clone();
            async move { resolve_token_at(&path).await }
        });
        let first = first.await.unwrap();
        let second = second.await.unwrap();

        let first = first.expect("the winning resolver must succeed");
        let second = second.expect(
            "the losing resolver must pick up the winner's token, not replay the spent one",
        );
        assert_eq!(first, "access-2");
        assert_eq!(second, "access-2");

        let posts = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.method == wiremock::http::Method::POST)
            .count();
        assert_eq!(
            posts, 1,
            "the single-use refresh token must be presented exactly once"
        );

        let _ = std::fs::remove_dir_all(dir);
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
        // Was `assert_eq!(expires_at_ms(Some(i64::MAX), now), i64::MAX)`, which
        // pinned the saturating behavior — and that behavior was the defect: it
        // caches a dead token as valid essentially forever. The clamp is the
        // contract now, so this asserts the clamp rather than whatever the code
        // happens to produce.
        assert_eq!(
            expires_at_ms(Some(i64::MAX), now),
            10_000_000 + MAX_EXPIRES_IN_SECS * 1000
        );
        // The realistic trigger is not `i64::MAX` but a gateway reporting
        // milliseconds in a seconds field.
        assert_eq!(
            expires_at_ms(Some(3_600_000), now),
            10_000_000 + MAX_EXPIRES_IN_SECS * 1000
        );
        // Anything at or under the cap is untouched.
        assert_eq!(
            expires_at_ms(Some(MAX_EXPIRES_IN_SECS), now),
            10_000_000 + MAX_EXPIRES_IN_SECS * 1000
        );
        assert_eq!(expires_at_ms(Some(3600), now), 10_000_000 + 3_600_000);
    }

    #[test]
    fn printed_gateway_strings_cannot_carry_terminal_escapes() {
        // `browser_open_refusal` promises the URL is printed even when it is
        // not opened. That promise is only worth something if what reaches the
        // terminal is what the user reads.
        assert_eq!(
            sanitize_for_terminal("https://gateway.example/device\x1b[2J?x=1"),
            "https://gateway.example/device\u{fffd}[2J?x=1"
        );
        assert_eq!(
            sanitize_for_terminal("BCDF\r\nGHJK"),
            "BCDF\u{fffd}\u{fffd}GHJK"
        );
        // Ordinary values pass through untouched.
        assert_eq!(
            sanitize_for_terminal("https://gateway.example/device?user_code=BCDF-GHJK"),
            "https://gateway.example/device?user_code=BCDF-GHJK"
        );
    }

    #[test]
    fn bidi_overrides_and_isolates_are_stripped_but_joiners_survive() {
        // `char::is_control` is general category Cc only, so every one of these
        // passes it untouched. A bidi override makes the *printed* URL display as
        // something other than what it is (CVE-2021-42574), which defeats
        // `browser_open_refusal`'s printed fallback exactly as an escape sequence
        // would — a test written only against \x1b stays green against these.
        for hostile in [
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}',
        ] {
            assert!(
                !hostile.is_control(),
                "{hostile:?} is Cf, so is_control cannot be what catches it"
            );
            let printed =
                sanitize_for_terminal(&format!("https://gateway.example/{hostile}device"));
            assert!(
                !printed.contains(hostile),
                "{hostile:?} must not reach the terminal: {printed:?}"
            );
        }

        // The rest of Cf is left alone: these are load-bearing for correct
        // rendering of Arabic, Persian, and Indic text, and an `error_description`
        // can legitimately be prose in those scripts.
        for legitimate in ['\u{200c}', '\u{200d}', '\u{00ad}'] {
            let text = format!("خطأ{legitimate}ما");
            assert_eq!(
                sanitize_for_terminal(&text),
                text,
                "{legitimate:?} is legitimate text, not a rendering attack"
            );
        }
    }

    #[test]
    fn gateway_error_strings_are_escaped_before_they_are_capped() {
        // A length cap alone leaves an escape sequence intact, so a test that only
        // checks length would stay green against exactly the input this guards.
        let escaped = sanitize_for_error("boom\u{1b}[2Jcleared");
        assert!(
            !escaped.contains('\u{1b}'),
            "the escape must not survive: {escaped:?}"
        );
        assert!(escaped.contains("boom"), "got: {escaped:?}");

        // A body that is nothing but escapes is still capped. (This does not pin
        // the escape/cap *order*: the replacement is one character for one, so both
        // orders agree today — see `sanitize_for_error`'s note on why escaping
        // still goes first.)
        let flood = "\u{1b}".repeat(5_000);
        let capped = sanitize_for_error(&flood);
        assert!(
            capped.chars().count() <= 201,
            "the escaped form must still be capped, got {} chars",
            capped.chars().count()
        );
        assert!(
            !capped.contains('\u{1b}'),
            "no escape may survive the cap either: {capped:?}"
        );

        // Truncation lands on a character boundary: multi-byte input must not be
        // sliced mid-character (which would panic on the string slice).
        let wide = "\u{1f600}".repeat(5_000);
        let capped = sanitize_for_error(&wide);
        assert!(
            capped.chars().count() <= 201,
            "got {}",
            capped.chars().count()
        );
        assert!(capped.starts_with('\u{1f600}'));
    }

    #[test]
    fn the_lock_timeout_leaves_slack_over_the_worst_case_legitimate_hold() {
        // A holder makes two separately bounded round-trips, so at exactly 2x a
        // holder using its full budget would expire the waiter at the instant it
        // succeeded — reporting contention for a refresh that actually worked.
        assert!(
            crate::auth::gateway::store::LOCK_TIMEOUT > NETWORK_TIMEOUT * 2,
            "the waiter must outlast the worst-case legitimate hold"
        );
    }

    #[test]
    fn helper_safety_matches_the_validator_claude_code_applies() {
        assert!(is_helper_safe("sk-ant-abc123"));
        assert!(is_helper_safe(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJkZXZAZXhhbXBsZSJ9.c2ln-_9"
        ));
        assert!(!is_helper_safe(""));
        assert!(!is_helper_safe("token with space"));
        assert!(!is_helper_safe("token\twith-tab"));
        assert!(!is_helper_safe("token\nwith-newline"));
        assert!(!is_helper_safe("token\0with-nul"));
        assert!(!is_helper_safe("tok\u{e9}n-non-ascii"));
        assert!(is_helper_safe(&"x".repeat(MAX_HELPER_OUTPUT)));
        assert!(!is_helper_safe(&"x".repeat(MAX_HELPER_OUTPUT + 1)));
    }

    #[tokio::test]
    async fn a_token_claude_code_would_reject_is_refused_with_a_diagnostic() {
        let dir = temp_dir("unsafe-token");
        let session_path = dir.join("session.json");
        let mut session = test_session("https://gateway.example", 0);
        // A newline is the realistic case: it turns one line of stdout into two
        // and fails Claude Code's validator with no hint about the cause.
        session.access_token = "access-1\nextra".to_string();
        session.expires_at_ms = now_plus_ms(3600);
        store::write_session(&session_path, &session).unwrap();

        let error = resolve_token_at(&session_path)
            .await
            .expect_err("a token the helper validator rejects must not be printed");
        let message = error.to_string();
        assert!(
            message.contains("https://gateway.example"),
            "the diagnostic must name the gateway that issued it: {message}"
        );
        assert!(
            message.contains("printable ASCII"),
            "the diagnostic must state the rule: {message}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_gateway_that_never_answers_fails_within_the_bound() {
        use tokio::net::TcpListener;

        // Accepts the connection and then says nothing at all — the case a
        // connect timeout does not catch and a bare `LOCK_EX` waits on forever.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });

        let dir = temp_dir("silent-gateway");
        let session_path = dir.join("session.json");
        let mut session = test_session(&format!("http://{address}"), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let started = std::time::Instant::now();
        let error = resolve_token_bounded(&session_path, Duration::from_secs(1))
            .await
            .expect_err("a silent gateway must not hang the helper");
        assert!(error.to_string().contains("did not answer"), "got: {error}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the bound must actually bound: waited {:?}",
            started.elapsed()
        );

        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plaintext_detection_matches_the_rule_applied_at_login() {
        // The login-time warning claims the exposure continues on every
        // refresh, so the refresh path has to apply the same test.
        assert!(is_plaintext_gateway("http://internal.example"));
        assert!(is_plaintext_gateway("http://10.0.0.5:8080/base"));
        assert!(!is_plaintext_gateway("https://gateway.example"));
        assert!(!is_plaintext_gateway("http://127.0.0.1:3001"));
        assert!(!is_plaintext_gateway("http://localhost:3001"));
        assert!(!is_plaintext_gateway("not a url"));
    }

    #[tokio::test]
    async fn a_blip_inside_the_expiry_buffer_serves_the_still_valid_cached_token() {
        // Nothing is mounted, so discovery fails — a stand-in for the two-second
        // network blip that must not become a user-visible auth failure.
        let server = MockServer::start().await;

        let dir = temp_dir("buffer-blip");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        // Inside the refresh buffer, but not actually expired for 60s.
        session.expires_at_ms = now_plus_ms(60);
        store::write_session(&session_path, &session).unwrap();

        let token = resolve_token_at(&session_path)
            .await
            .expect("a failed refresh must not discard a token that is still valid");
        assert_eq!(token, "access-1");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_refresh_on_a_genuinely_expired_token_still_fails() {
        // Same failure, but the cached token is actually dead: returning it
        // would only move the error somewhere less legible.
        let server = MockServer::start().await;

        let dir = temp_dir("buffer-expired");
        let session_path = dir.join("session.json");
        let mut session = test_session(&server.uri(), 0);
        session.expires_at_ms = now_plus_ms(-1);
        store::write_session(&session_path, &session).unwrap();

        let error = resolve_token_at(&session_path)
            .await
            .expect_err("an expired token must not be served as a success");
        assert!(!error.to_string().contains("access-1"), "got: {error}");

        let _ = std::fs::remove_dir_all(dir);
    }
}
