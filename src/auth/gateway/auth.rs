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
    match parse_discovery(&value, gateway_url) {
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

fn parse_discovery(value: &Value, gateway_url: &str) -> Result<Discovery, DiscoveryProblem> {
    // A plain-http deployment may name its *own* origin, and only its own.
    //
    // `login::normalize_gateway_url` accepts `http://10.0.0.5:8080` with a
    // warning, and `refresh_session` repeats that warning on every refresh —
    // so a plaintext gateway is a supported (if discouraged) setup, and
    // rejecting the endpoints it advertises would make the login fail three
    // steps after promising to proceed. `discover` is shared by login and
    // refresh, and the stored `gateway_url` is that same origin, so the same
    // allowance applies on both paths.
    //
    // Computed from the *operator-supplied* base URL, never from the document.
    // `None` for every other gateway, including an https one: nothing about a
    // TLS deployment justifies plaintext endpoints.
    let plaintext_gateway_origin = is_plaintext_gateway(gateway_url)
        .then(|| gateway_url.parse::<reqwest::Url>().ok())
        .flatten()
        .map(|url| url.origin());
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
        // Same predicate as that policy, not a second copy of it — widened
        // only by the same-origin allowance above.
        //
        // Same-origin is what keeps that allowance narrow: a hostile or MITM'd
        // discovery document must not be able to name a *third-party*
        // plaintext host and have the refresh token POSTed there. Talking to
        // the operator's own plaintext gateway is an exposure they were warned
        // about and chose; shipping the token somewhere else is not. Origins
        // are compared with `Url::origin`, so scheme, host, and port must all
        // match and a non-tuple (opaque) origin never compares equal.
        let accepted = raw.parse::<reqwest::Url>().is_ok_and(|url| {
            crate::auth::shared::is_safe_refresh_url(&url)
                || plaintext_gateway_origin
                    .as_ref()
                    .is_some_and(|origin| url.origin() == *origin)
        });
        if !accepted {
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
            // `tracing`, unlike the security warnings in this module: this one
            // is a compatibility note, so it belongs on the log surface where
            // `RUST_LOG` governs it. The default `shunt=info` filter still puts
            // it on stderr.
            tracing::warn!(
                "this gateway speaks protocol version {version}; shunt was built against version \
                 {SUPPORTED_PROTOCOL_VERSION}. Upgrade shunt if the login misbehaves."
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

/// Marker attached to a refresh the gateway will never accept a retry of: the
/// rotation family is dead, so the only cure is a fresh login.
///
/// A type rather than a string match, because `refresh_session` has to
/// classify this failure and matching on the message would break silently the
/// first time that message is reworded.
#[derive(Debug)]
struct InvalidGrant;

impl std::fmt::Display for InvalidGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid_grant")
    }
}

impl std::error::Error for InvalidGrant {}

/// Marker attached to a token POST that never reached the gateway: the
/// connection itself could not be established, so the refresh token provably
/// never left this process and nothing can have been rotated.
///
/// The only such point on the refresh path. Once `send()` has handed the
/// request bytes off, a failure anywhere after it — the bound expiring, an
/// unreadable body, a body that does not parse — says nothing about whether
/// the gateway received the token and rotated it.
///
/// A type rather than a string match, for the same reason as [`InvalidGrant`].
#[derive(Debug)]
struct ConnectFailed;

impl std::fmt::Display for ConnectFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the connection was never established")
    }
}

impl std::error::Error for ConnectFailed {}

/// Why a refresh failed, from the caller's point of view.
///
/// The distinction decides whether the cached access token may still be served.
/// The gateway rotates the refresh token on every grant and the old one is
/// single-use, so once the token POST has been *sent* the stored refresh token
/// may already be spent: falling back to the cached token from there hides the
/// only instruction that recovers the session, and leaves the spent token on
/// disk for the next helper run to replay — which revokes the whole family.
enum RefreshFailure {
    /// The refresh token was provably never presented: discovery failed (it
    /// never carries the token), or the token POST could not open a connection
    /// at all. Nothing was rotated, so an unexpired cached token is still a
    /// safe answer and a later attempt can still succeed.
    PreRotation(anyhow::Error),
    /// The stored refresh token may be spent, or the family is already dead.
    /// Everything that is not provably pre-send lands here — a timed-out POST,
    /// an unreadable or unparseable body, any error the gateway named, any
    /// status. Serving the cached token would only delay a failure a fresh
    /// login is the sole cure for, so this propagates with its original message
    /// intact.
    Terminal(anyhow::Error),
}

impl RefreshFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::PreRotation(error) | Self::Terminal(error) => error,
        }
    }
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
        .map_err(|error| {
            // Classified here, where the `reqwest::Error` still exists: this is
            // the only point on this path where a failure proves the refresh
            // token was never handed to the network. `is_connect()` covers the
            // DNS/TCP/TLS setup that precedes the request bytes.
            let never_sent = error.is_connect();
            let reported = anyhow::Error::new(error);
            let reported = if never_sent {
                reported.context(ConnectFailed)
            } else {
                reported
            };
            reported.context(format!("failed to reach {token_endpoint}"))
        })?;
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
        // Carries [`InvalidGrant`] so `refresh_session` can tell this apart
        // from a transient failure without matching on the message text.
        Some("invalid_grant") => Err(anyhow::Error::new(InvalidGrant).context(
            "the gateway rejected the stored refresh token (invalid_grant). It expired, or \
             another session already used it — refresh tokens are single-use. Run \
             `shunt gateway login <url>` to sign in again",
        )),
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
        //
        // Restricted to pre-rotation failures, deliberately. Any failure that
        // is not provably pre-send may have left a *spent* refresh token on
        // disk, so serving the cached token here would swallow the re-login
        // instruction and let the next helper run replay that spent token —
        // the family-wide logout this whole path exists to avoid.
        Err(RefreshFailure::PreRotation(error)) if is_unexpired_at(&session, SystemTime::now()) => {
            eprintln!(
                "Warning: could not refresh the gateway token ({error}); serving the cached one, \
                 which is still valid but expires shortly. This will fail once it does."
            );
            helper_safe_token(session.access_token, &session.gateway_url)
        }
        Err(failure) => Err(failure.into_error()),
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
) -> Result<String, RefreshFailure> {
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
    // Discovery never carries the refresh token, so every failure of this step
    // — including its bound expiring — leaves the stored token untouched. It is
    // the one whole step that is provably pre-rotation; on the POST below only
    // a failure to connect is.
    let discovery = bounded(
        network_timeout,
        discover(&client, &session.gateway_url),
        "discovery",
        &session.gateway_url,
    )
    .await
    .map_err(RefreshFailure::PreRotation)?;
    let tokens = match bounded(
        network_timeout,
        refresh(&client, &discovery.token_endpoint, &session.refresh_token),
        "token refresh",
        &discovery.token_endpoint,
    )
    .await
    {
        Ok(tokens) => tokens,
        // The family is dead; a retry cannot revive it and the cached token
        // would only postpone the same re-login.
        Err(error) if error.downcast_ref::<InvalidGrant>().is_some() => {
            return Err(RefreshFailure::Terminal(error))
        }
        // The connection was never established, so the token cannot have been
        // received: the only failure on this call that is safe to fall back
        // from.
        Err(error) if error.downcast_ref::<ConnectFailed>().is_some() => {
            return Err(RefreshFailure::PreRotation(error))
        }
        // Everything else — the bound expiring (which replaces the inner error
        // outright), a body that could not be read or parsed, an unrecognized
        // `error` field, any status — leaves it unknown whether the gateway
        // received the token and rotated it. Treating "unknown" as
        // pre-rotation is what leaves a spent token on disk for the next
        // helper run to replay.
        Err(error) => return Err(RefreshFailure::Terminal(error)),
    };
    // Validated *before* it is persisted. A gateway answering with a token the
    // helper contract rejects (an embedded newline is the realistic case) would
    // otherwise have it written to disk, and every later call would then take
    // the fast path and fail the same validation — a session bricked until
    // expiry with no way out but a fresh login.
    let access_token = helper_safe_token(tokens.access_token, &session.gateway_url)
        .context(
            "the previous refresh token is now spent, so run `shunt gateway login <url>` to sign \
             in again",
        )
        .map_err(RefreshFailure::Terminal)?;
    let refreshed = GatewaySession {
        gateway_url: session.gateway_url.clone(),
        access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: expires_at_ms(tokens.expires_in, SystemTime::now()),
    };
    // Hard failure, deliberately: the refresh token just presented is already
    // burned, so returning the access token while losing its replacement would
    // strand the user at the next refresh with no way back but a fresh login.
    store::write_session(path, &refreshed)
        .context(
            "the gateway issued a new token pair but it could not be saved; the previous refresh \
             token is now spent, so run `shunt gateway login <url>` to sign in again",
        )
        .map_err(RefreshFailure::Terminal)?;
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
mod tests;
