//! `shunt gateway login <url>` / `logout` — the RFC 8628 device-authorization
//! flow against a self-hosted shunt deployment.
//!
//! There is no scriptable approval path and none is wanted: approval happens in
//! a browser, where the gateway enforces same-origin CSRF on `POST /device` and
//! an OIDC-only deployment renders no password form at all. This command's job
//! ends at handing the user `verification_uri_complete` and polling until they
//! finish.
//!
//! Every request here parses the response body before looking at the HTTP
//! status — see [`super::auth`]'s module doc for why that is load-bearing.

use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::time::{sleep, Instant};

use super::auth::{
    bounded, discover, expires_at_ms, helper_safe_token, parse_token_response, sanitize_for_error,
    sanitize_for_terminal, CLIENT_ID, DEVICE_CODE_GRANT_TYPE, NETWORK_TIMEOUT,
};
use super::store::{self, GatewaySession};

const DEFAULT_INTERVAL_SECS: u64 = 5;
const MIN_INTERVAL_SECS: u64 = 1;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_INTERVAL_SECS: u64 = 30;
/// The gateway's `DEVICE_CODE_TTL`.
const DEFAULT_EXPIRES_SECS: u64 = 600;
/// A transient blip should not abort a login that may legitimately run for ten
/// minutes, but a permanently unreachable endpoint must fail fast rather than
/// retry silently for the whole device-code lifetime.
const MAX_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 3;

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

/// Redacting, deliberately: `device_code` is a one-time secret that redeems the
/// session's tokens, so the derived form must not print it through a panic
/// message.
impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceCode")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// What to do after a non-success poll response (RFC 8628 §3.5).
#[derive(Debug, PartialEq, Eq)]
enum PollOutcome {
    /// Keep polling at the current interval.
    Pending,
    /// Bump the interval and keep polling.
    SlowDown,
    /// Terminal failure with a user-facing reason.
    Failed(String),
}

/// Log in to the shunt gateway at `gateway_url`. With `manual`, the browser is
/// not opened and the user is left to follow the printed URL themselves.
pub async fn run(gateway_url: &str, manual: bool) -> anyhow::Result<()> {
    run_bounded(gateway_url, manual, NETWORK_TIMEOUT).await
}

/// [`run`] with an explicit per-call network bound, so tests can drive the
/// timeout path without waiting [`NETWORK_TIMEOUT`] out.
///
/// The third such variant in this module, alongside
/// [`super::auth::resolve_token_bounded`] and
/// [`super::store::lock_session_for`] — consistency with an established seam
/// rather than a new pattern.
pub(crate) async fn run_bounded(
    gateway_url: &str,
    manual: bool,
    network_timeout: Duration,
) -> anyhow::Result<()> {
    let gateway_url = normalize_gateway_url(gateway_url)?;
    // Every request below carries a secret this flow cannot afford to leak
    // off-origin: the device-authorization POST returns a one-time
    // `device_code`, and the poll redeems it for the session's tokens. The
    // redirect-hardened client refuses to forward either to an unsafe host.
    let client = crate::auth::shared::token_refresh_client();
    // Bounded on the same budget as the refresh path. Not because this one can
    // wedge the lock — it holds none — but because a gateway that accepts the
    // connection and never answers would otherwise leave the terminal waiting
    // indefinitely with nothing to read and only Ctrl-C to end it. The poll
    // loop below is deliberately *not* wrapped: it is supposed to wait, and it
    // has `poll_deadline` plus its own per-attempt bound and transport retry.
    let discovery = bounded(
        network_timeout,
        discover(&client, &gateway_url),
        "discovery",
        &gateway_url,
    )
    .await?;
    let device = bounded(
        network_timeout,
        request_device_code(&client, &discovery.device_authorization_endpoint),
        "device-code request",
        &discovery.device_authorization_endpoint,
    )
    .await
    .context("failed to request a device code from the gateway")?;

    let prompt_url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    println!("To authorize this machine against {gateway_url}, open:\n");
    println!("    {}\n", sanitize_for_terminal(&prompt_url));
    println!(
        "and confirm the code: {}\n(waiting for approval — this window will update automatically)",
        sanitize_for_terminal(&device.user_code)
    );
    if !manual {
        match browser_open_refusal(&prompt_url) {
            Some(reason) => eprintln!(
                "Not opening this URL automatically: {reason}. It was chosen by the gateway, not \
                 by shunt — open it yourself only if it looks like the deployment's own sign-in \
                 page."
            ),
            None => {
                if let Err(error) =
                    crate::auth::shared::open_url_async("gateway login", &prompt_url).await
                {
                    eprintln!("Could not open a browser ({error}); open the URL above manually.");
                }
            }
        }
    }

    let tokens = poll_for_tokens(&client, &device, &discovery.token_endpoint, network_timeout)
        .await
        .context("gateway device authorization failed")?;

    // Vetted before anything is stored, mirroring what `auth::refresh_session`
    // does with a rotated token. A gateway answering with a token the helper
    // contract rejects would otherwise be written to disk under a "Login
    // successful" banner — and since the stored expiry normally sits outside
    // the refresh buffer, every later `shunt gateway token` would take the
    // cached path and fail this same check until the token expired. A session
    // that reports success and can never be used is worse than a failed login.
    let access_token = helper_safe_token(tokens.access_token, &gateway_url).context(
        "the device authorization succeeded, but no session was saved: this deployment cannot be \
         used from Claude Code until it issues a conforming access token",
    )?;

    let session = GatewaySession {
        gateway_url,
        access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: expires_at_ms(tokens.expires_in, std::time::SystemTime::now()),
    };
    let path = store::session_path();
    // Under the same lock logout and the refresh writeback take, and held
    // across the write. A refresh that acquired it before this login finishes
    // its own writeback *after* the login's — resurrecting the signed-out
    // gateway URL and token pair over the session this command is about to
    // report as saved. Same sibling `.lock` inode, so the three serialize.
    let _lock = store::lock_session(&path).await?;
    store::write_session(&path, &session)?;

    println!("\nLogin successful. Session saved to {}", path.display());
    println!("Use it from Claude Code with: \"apiKeyHelper\": \"shunt gateway token\"");
    Ok(())
}

/// `shunt gateway logout` — drop the stored session. Idempotent.
///
/// Async because the removal happens under the session lock, so a logout that
/// overlaps an in-flight refresh cannot be undone by that refresh's writeback.
pub async fn logout() -> anyhow::Result<()> {
    let path = store::session_path();
    if store::remove_session(&path).await? {
        println!("Removed the gateway session at {}", path.display());
    } else {
        println!("No gateway session at {}; nothing to do", path.display());
    }
    Ok(())
}

/// Trim and vet the operator-supplied base URL. A plaintext non-loopback URL is
/// a warning rather than a refusal: a self-hosted deployment behind a VPN or on
/// a private network is a legitimate setup, and refusing it would leave the
/// operator with no way to log in at all.
fn normalize_gateway_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("gateway URL must not be empty; pass the deployment's base URL, e.g. https://gateway.example.com");
    }
    let url: reqwest::Url = trimmed
        .parse()
        .with_context(|| format!("{trimmed:?} is not a valid URL"))?;
    match url.scheme() {
        "https" => {}
        "http" => {
            if !crate::config::host_is_loopback(url.host_str().unwrap_or_default()) {
                eprintln!(
                    "Warning: {trimmed} is plain HTTP, so the device code and refresh token \
                     travel unencrypted — not just during this login, but on every token \
                     refresh for as long as the session lives. Use https:// unless this link \
                     is already private."
                );
            }
        }
        scheme => bail!("gateway URL scheme {scheme:?} is not supported; use https or http"),
    }
    Ok(trimmed.to_string())
}

/// Why the verification URL must not be handed to the OS handler, or `None`
/// when it may be.
///
/// This is the one URL in shunt that a **remote party** picks: it arrives
/// verbatim in the device-authorization response, and the OS handler will act
/// on whatever scheme it names. A hostile or MITM'd gateway could otherwise
/// answer with `file:///…`, an `smb://` share the handler authenticates to, or
/// a leading-dash string that `open(1)` reads as a flag. The URL is printed
/// either way — refusing to *auto-open* is not the same as hiding it.
fn browser_open_refusal(raw: &str) -> Option<String> {
    let Ok(url) = raw.parse::<reqwest::Url>() else {
        return Some("the gateway's verification URL is not a valid absolute URL".to_string());
    };
    match url.scheme() {
        "http" | "https" => None,
        scheme => Some(format!(
            "the gateway's verification URL uses the {scheme:?} scheme, and shunt only opens http \
             and https"
        )),
    }
}

async fn request_device_code(
    client: &reqwest::Client,
    device_authorization_endpoint: &str,
) -> anyhow::Result<DeviceCode> {
    let response = client
        .post(device_authorization_endpoint)
        .header("accept", "application/json")
        // Pinned by the server: any other client id is answered with a 400.
        .form(&[("client_id", CLIENT_ID)])
        .send()
        .await
        .with_context(|| format!("failed to reach {device_authorization_endpoint}"))?;
    let status = response.status();
    // Propagate rather than defaulting to "": the same reason as in
    // [`super::auth`]'s two body reads. An empty string here is reported as a
    // response "missing device_code / user_code / verification_uri", which
    // blames the gateway for a body it may well have sent in full.
    let text = response.text().await.with_context(|| {
        format!("failed to read the device-code response from {device_authorization_endpoint}")
    })?;
    // Never gate on `status` before parsing the body (see the module doc).
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(device) = parse_device_code(&value) {
        return Ok(device);
    }
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        bail!(
            "the gateway refused the device-code request ({})",
            sanitize_for_error(error)
        );
    }
    bail!(
        "the gateway's device-code response is missing device_code / user_code / \
         verification_uri (HTTP {status}): {}",
        sanitize_for_error(&text)
    )
}

fn parse_device_code(value: &Value) -> Option<DeviceCode> {
    Some(DeviceCode {
        device_code: value.get("device_code")?.as_str()?.to_string(),
        user_code: value.get("user_code")?.as_str()?.to_string(),
        verification_uri: value.get("verification_uri")?.as_str()?.to_string(),
        verification_uri_complete: value
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        // Capped, not merely defaulted: any non-overflowing value was honored,
        // so a gateway could hold the terminal polling for a year.
        // DEFAULT_EXPIRES_SECS is the gateway's own DEVICE_CODE_TTL, which is
        // the longest a device code can legitimately stay live.
        expires_in: positive_secs(value.get("expires_in"), DEFAULT_EXPIRES_SECS)
            .min(DEFAULT_EXPIRES_SECS),
        interval: positive_secs(value.get("interval"), DEFAULT_INTERVAL_SECS)
            .max(MIN_INTERVAL_SECS),
    })
}

/// Normalize a server-supplied seconds value, falling back to `default` when it
/// is missing or non-positive (which would otherwise busy-spin the poll loop).
fn positive_secs(value: Option<&Value>, default: u64) -> u64 {
    value
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(default)
}

/// Compute the poll deadline without panicking: `Instant + Duration` panics on
/// overflow and `expires_in` is server-supplied, so a pathological value
/// degrades to the default lifetime instead of aborting the login.
fn poll_deadline(now: Instant, expires_in: u64) -> Instant {
    now.checked_add(Duration::from_secs(expires_in))
        .or_else(|| now.checked_add(Duration::from_secs(DEFAULT_EXPIRES_SECS)))
        .unwrap_or(now)
}

/// One poll attempt, already reduced to what the loop needs: the response, or
/// a printable reason the attempt produced none.
///
/// The reason is a `String` rather than the `reqwest::Error` because a timeout
/// is not one, and because the error is stripped of its URL before it gets
/// here (see [`poll_attempt`]).
type PollAttempt = Result<(reqwest::StatusCode, String), String>;

/// Send one poll and read its body, bounded by `attempt_timeout`.
///
/// The bound is the point: the shared refresh client sets no request timeout,
/// so a gateway that accepts the connection and never finishes the response
/// leaves this awaiting forever — and the loop's `deadline` check only runs
/// *between* attempts, so it would never be reached. A timed-out attempt is
/// reported as a transport failure so it retries on the same path a failed
/// connect does, with the device-code deadline still governing the whole loop.
///
/// A connection reset partway through the *body* is the same class of blip as
/// a failed connect, so the two are collapsed into one outcome here rather than
/// only the send being bounded and retried.
async fn poll_attempt(request: reqwest::RequestBuilder, attempt_timeout: Duration) -> PollAttempt {
    let attempt = async {
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        Ok::<_, reqwest::Error>((status, text))
    };
    match tokio::time::timeout(attempt_timeout, attempt).await {
        Ok(Ok(response)) => Ok(response),
        // `without_url`, deliberately: reqwest's `Display` appends the URL it
        // was talking to, and that is the discovered token endpoint. This
        // string is logged and then reported in the timeout diagnostic, so
        // keeping the URL out of it keeps the endpoint out of both.
        Ok(Err(error)) => Err(error.without_url().to_string()),
        Err(_) => Err(format!("no response within {attempt_timeout:?}")),
    }
}

async fn poll_for_tokens(
    client: &reqwest::Client,
    device: &DeviceCode,
    token_endpoint: &str,
    attempt_timeout: Duration,
) -> anyhow::Result<super::auth::TokenResponse> {
    let deadline = poll_deadline(Instant::now(), device.expires_in);
    let mut interval = device.interval.max(MIN_INTERVAL_SECS);
    let mut last_transport_error: Option<String> = None;
    let mut consecutive_transport_failures: u32 = 0;
    while Instant::now() < deadline {
        let request = client
            .post(token_endpoint)
            .header("accept", "application/json")
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ]);
        // Never wait past the device-code deadline on a single attempt either:
        // a zero remainder times the attempt out immediately and falls through
        // to the loop's own deadline break, rather than adding a second exit.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt = poll_attempt(request, attempt_timeout.min(remaining)).await;
        // A single DNS/TLS/connect/read blip must not abort the login: remember
        // the error and fall through to the deadline-bounded sleep, then retry.
        // A permanently unreachable endpoint still fails fast via the
        // consecutive-failure cap rather than spinning until the deadline.
        let (status, text) = match attempt {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("gateway token poll request failed, will retry: {error}");
                last_transport_error = Some(error.clone());
                consecutive_transport_failures += 1;
                if consecutive_transport_failures >= MAX_CONSECUTIVE_TRANSPORT_FAILURES {
                    bail!(
                        "gateway token poll failed {MAX_CONSECUTIVE_TRANSPORT_FAILURES} times in a row; last error: {error}"
                    );
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                sleep(remaining.min(Duration::from_secs(interval))).await;
                continue;
            }
        };
        // A whole response arrived, so any earlier run of failures is over —
        // only *consecutive* failures trip the cap. Clearing the remembered
        // error keeps the timeout message honest: a stale blip must not make a
        // plain "the user never approved" timeout look like a network problem.
        consecutive_transport_failures = 0;
        last_transport_error = None;
        // *** CRITICAL: the gateway answers an ordinary pending poll with HTTP
        // 400, so the status cannot decide which branch to parse — try the
        // token first regardless of status, then the OAuth error envelope, and
        // only then fall back to a bare-status message.
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => bail!(
                "invalid gateway token response (HTTP {status}): {}",
                sanitize_for_error(&text)
            ),
        };
        if let Some(tokens) = parse_token_response(&value) {
            return Ok(tokens);
        }
        match classify_poll_error(&value, &text) {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = next_interval(interval),
            PollOutcome::Failed(reason) => bail!("{reason}"),
        }
        // Honor the interval strictly. The gateway's first poll always answers
        // `authorization_pending` and schedules the next allowed poll at now+5s;
        // polling before then answers `slow_down` *and* adds 5s to the server's
        // own interval — so an impatient client only inflates its own backoff.
        //
        // Never sleep past the device-code deadline either: `interval` is
        // server-supplied and RFC 8628 puts no ceiling on it, so a large value
        // would stall well beyond `expires_in` before the loop could report the
        // timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_secs(interval))).await;
    }
    match last_transport_error {
        Some(error) => bail!(
            "gateway device authorization timed out; the last poll attempt failed: {error}. Run `shunt gateway login <url>` to try again"
        ),
        None => bail!(
            "gateway device authorization timed out; run `shunt gateway login <url>` to try again"
        ),
    }
}

/// Apply the RFC 8628 `slow_down` backoff: bump the interval by
/// [`SLOW_DOWN_INCREMENT_SECS`], capped at [`MAX_INTERVAL_SECS`].
fn next_interval(current: u64) -> u64 {
    // Saturating, not plain `+`: `current` starts from a gateway-supplied
    // `interval`, so a near-`u64::MAX` value followed by `slow_down` would
    // panic in debug and wrap to a tiny interval in release.
    current
        .saturating_add(SLOW_DOWN_INCREMENT_SECS)
        .min(MAX_INTERVAL_SECS)
}

/// `raw` is the response body the JSON came from: the unrecognized-shape arm
/// reports it, matching the invalid-JSON path one branch up. Without it, a
/// gateway answering with valid JSON in an unexpected shape (`{}`, or a
/// health-check body that matched by accident) ends the login with a message
/// that says nothing about what actually came back.
fn classify_poll_error(body: &Value, raw: &str) -> PollOutcome {
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    match error {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "access_denied" => PollOutcome::Failed(
            "the approval was denied in the browser; run `shunt gateway login <url>` again to \
             retry"
                .to_string(),
        ),
        // The gateway also answers an unknown device_code with `expired_token`,
        // so this covers a code that was already redeemed or swept.
        "expired_token" => PollOutcome::Failed(
            "the device code expired or was already used; run `shunt gateway login <url>` again"
                .to_string(),
        ),
        _ => {
            // Capped like every raw-body site: pulling the string out of a JSON
            // field does not make it any shorter, and the same proxy or WAF can
            // put a whole HTML page in `error_description`.
            match body
                .get("error_description")
                .and_then(Value::as_str)
                .or(Some(error))
                .filter(|value| !value.is_empty())
            {
                Some(description) => PollOutcome::Failed(format!(
                    "device authorization failed: {}",
                    sanitize_for_error(description)
                )),
                // Nothing recognizable in the body: show what did arrive.
                None => PollOutcome::Failed(format!(
                    "device authorization failed; the gateway's response was not a token or a \
                     recognized OAuth error: {}",
                    sanitize_for_error(raw)
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests;
