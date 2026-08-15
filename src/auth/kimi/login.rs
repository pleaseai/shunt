//! `shunt login kimi --name <account-name>` — the RFC 8628 device-authorization
//! flow for a Kimi Code subscription login, storing the result as a named
//! account (see [`super::store`]) rather than a single fixed-path credential
//! file, so the account can be referenced from the Kimi account pool
//! (`[[providers.*.accounts]]`/`account_scope`).
//!
//! No loopback callback server is needed: shunt requests a device code,
//! prints a verification URL and short user code, and long-polls Kimi's token
//! endpoint until the user approves in a browser (on any device).
//!
//! *** Kimi's token endpoint returns HTTP 400 for the ordinary
//! `authorization_pending` poll response (unlike xAI/most RFC 8628 providers,
//! which use 400 too, but Kimi's device-code *request* endpoint is otherwise
//! unmeasured for its failure shape) — every request in this module parses
//! the response body before ever looking at the HTTP status, per
//! [`super::auth`]'s module doc comment.

use std::borrow::Cow;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::Value;
use tokio::time::{sleep, Instant};

use super::auth::{
    expires_at_ms, msh_headers, parse_token_response, CLIENT_ID, DEVICE_CODE_GRANT_TYPE,
    DEVICE_CODE_URL, TOKEN_URL,
};
use super::store;

const DEFAULT_INTERVAL_SECS: u64 = 5;
const MIN_INTERVAL_SECS: u64 = 1;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_INTERVAL_SECS: u64 = 30;
// Measured against the live device-authorization endpoint: `expires_in` = 1800.
const DEFAULT_EXPIRES_SECS: u64 = 1800;
// A transient blip (one dropped connection) should not abort the login, but a
// permanently unreachable or misconfigured endpoint (bad `token_url`, offline
// machine, broken TLS/MITM) must fail fast rather than silently retry for the
// full device-code lifetime — up to 30 minutes at the default interval.
const MAX_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 3;

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
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

/// Run the device-code login for a Kimi Code account named `name`, generating
/// a fresh device id for this account and persisting it alongside the tokens.
pub async fn run(name: &str) -> anyhow::Result<()> {
    store::validate_account_name(name)?;
    let device_id = uuid::Uuid::new_v4().to_string();
    // Both requests below carry secrets the login flow cannot afford to leak
    // off-origin: the device-authorization POST returns a one-time
    // `device_code`, and the token poll redeems it for the account's tokens.
    // Use the redirect-hardened `token_refresh_client()` so a 307/308 from
    // either endpoint cannot forward either secret to an unsafe host.
    let client = crate::auth::shared::token_refresh_client();
    let device = request_device_code(&client, &device_id)
        .await
        .context("failed to request Kimi device code")?;

    let prompt_url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    println!("To authorize shunt with your Kimi Code subscription, open:\n");
    println!("    {prompt_url}\n");
    println!(
        "and confirm the code: {}\n(waiting for approval — this window will update automatically)",
        device.user_code
    );

    let tokens = poll_for_tokens(&client, &device, TOKEN_URL, &device_id)
        .await
        .context("Kimi device authorization failed")?;

    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("Kimi did not return a refresh_token; cannot persist login"))?;
    let expires_at_ms = expires_at_ms(tokens.expires_in, std::time::SystemTime::now());
    let path = store::store_oauth_tokens(
        name,
        &tokens.access_token,
        refresh_token,
        expires_at_ms,
        &device_id,
    )
    .with_context(|| format!("failed to write Kimi credentials for account {name:?}"))?;

    println!(
        "\nLogin successful. Credentials saved to {}",
        path.display()
    );
    if let Some(expiry) =
        std::time::UNIX_EPOCH.checked_add(Duration::from_millis(expires_at_ms.max(0) as u64))
    {
        println!(
            "Access token valid until {} (shunt refreshes it automatically).",
            crate::auth::shared::format_iso8601(expiry)
        );
    }
    Ok(())
}

async fn request_device_code(
    client: &reqwest::Client,
    device_id: &str,
) -> anyhow::Result<DeviceCode> {
    let mut request = client
        .post(DEVICE_CODE_URL)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID)]);
    for (name, value) in msh_headers(device_id) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    // Never gate on `status` before parsing (see the module doc comment) — a
    // failure here may still carry a parseable OAuth error envelope.
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => bail!(
            "Kimi device-code request failed (HTTP {status}): {}",
            truncate_for_error(&text)
        ),
    };
    if let Some(device) = parse_device_code(&value) {
        return Ok(device);
    }
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        let description = value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        if description.is_empty() {
            bail!("Kimi device-code request failed ({error})");
        }
        bail!("Kimi device-code request failed ({error}): {description}");
    }
    bail!("Kimi device-code response missing device_code / user_code / verification_uri (HTTP {status})");
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
        expires_in: positive_secs(value.get("expires_in"), DEFAULT_EXPIRES_SECS),
        interval: positive_secs(value.get("interval"), DEFAULT_INTERVAL_SECS)
            .max(MIN_INTERVAL_SECS),
    })
}

/// Normalize a server-supplied seconds value, falling back to `default` when it
/// is missing or non-positive (defends the poll loop from a garbage interval).
fn positive_secs(value: Option<&Value>, default: u64) -> u64 {
    value
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(default)
}

/// Cap a raw upstream body before it reaches an error message. A proxy or WAF
/// can answer with a full HTML page, which would otherwise be dumped whole into
/// the operator's terminal. Truncates on a char boundary, never mid-codepoint.
fn truncate_for_error(text: &str) -> Cow<'_, str> {
    const LIMIT: usize = 200;
    match text.char_indices().nth(LIMIT) {
        Some((byte_index, _)) => Cow::Owned(format!("{}\u{2026}", &text[..byte_index])),
        None => Cow::Borrowed(text),
    }
}

/// Compute the poll deadline without panicking. `Instant + Duration` panics on
/// overflow, and `expires_in` is a server-supplied value only checked for being
/// positive, so a pathological one must degrade to the default lifetime rather
/// than abort the login. If even that overflows, return `now` so the poll loop
/// exits immediately and reports the ordinary timeout.
fn poll_deadline(now: Instant, expires_in: u64) -> Instant {
    now.checked_add(Duration::from_secs(expires_in))
        .or_else(|| now.checked_add(Duration::from_secs(DEFAULT_EXPIRES_SECS)))
        .unwrap_or(now)
}

async fn poll_for_tokens(
    client: &reqwest::Client,
    device: &DeviceCode,
    token_url: &str,
    device_id: &str,
) -> anyhow::Result<super::auth::TokenResponse> {
    let deadline = poll_deadline(Instant::now(), device.expires_in);
    let mut interval = device.interval.max(MIN_INTERVAL_SECS);
    let mut last_transport_error: Option<String> = None;
    let mut consecutive_transport_failures: u32 = 0;
    while Instant::now() < deadline {
        let mut request = client
            .post(token_url)
            .header("accept", "application/json")
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ]);
        for (name, value) in msh_headers(device_id) {
            request = request.header(name, value);
        }
        // A single DNS/TLS/connect blip during a 15-30 minute poll must not
        // abort the whole login: remember the error and fall through to the
        // deadline-bounded sleep below, then retry. This is safe because the
        // loop is bounded by `deadline` — a permanently unreachable endpoint
        // still fails fast via `MAX_CONSECUTIVE_TRANSPORT_FAILURES` below
        // rather than spinning until `deadline` (up to 30 minutes).
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("Kimi token poll request failed, will retry: {error}");
                last_transport_error = Some(error.to_string());
                consecutive_transport_failures += 1;
                if consecutive_transport_failures >= MAX_CONSECUTIVE_TRANSPORT_FAILURES {
                    bail!(
                        "Kimi token poll failed {MAX_CONSECUTIVE_TRANSPORT_FAILURES} times in a row; last error: {error}"
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
        // The request succeeded, so any earlier run of transport failures is
        // over — only *consecutive* failures should trip the fail-fast cap.
        consecutive_transport_failures = 0;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        // *** CRITICAL: Kimi returns HTTP 400 for the ordinary
        // `authorization_pending` poll response, so the status cannot gate which
        // branch to parse — always attempt to parse a token from the body first,
        // regardless of status, and only fall back to a bare-status message
        // when the body has neither a parseable token nor an OAuth error
        // envelope. See the module doc comment.
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => bail!(
                "invalid Kimi token response (HTTP {status}): {}",
                truncate_for_error(&text)
            ),
        };
        if let Some(tokens) = parse_token_response(&value) {
            return Ok(tokens);
        }
        match classify_poll_error(&value) {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => {
                interval = next_interval(interval);
            }
            PollOutcome::Failed(reason) => bail!("{reason}"),
        }
        // Never sleep past the device-code deadline. The server supplies
        // `interval` and RFC 8628 puts no ceiling on it, so a large value would
        // otherwise stall the login well beyond `expires_in` before the loop
        // condition gets a chance to report the timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_secs(interval))).await;
    }
    match last_transport_error {
        Some(error) => bail!(
            "Kimi device authorization timed out; the last poll attempt failed: {error}. Run shunt login kimi --name <account-name> to try again"
        ),
        None => {
            bail!("Kimi device authorization timed out; run shunt login kimi --name <account-name> to try again")
        }
    }
}

/// Apply the RFC 8628 `slow_down` backoff: bump the poll interval by
/// [`SLOW_DOWN_INCREMENT_SECS`], capped at [`MAX_INTERVAL_SECS`].
fn next_interval(current: u64) -> u64 {
    (current + SLOW_DOWN_INCREMENT_SECS).min(MAX_INTERVAL_SECS)
}

fn classify_poll_error(body: &Value) -> PollOutcome {
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    match error {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "access_denied" | "authorization_denied" => {
            PollOutcome::Failed("authorization was denied".to_string())
        }
        "expired_token" => PollOutcome::Failed(
            "device code expired; run shunt login kimi --name <account-name> again".to_string(),
        ),
        // Measured: a bad/reused device_code returns `invalid_grant`, not
        // `expired_token` or `access_denied` — terminal, pointing back at a
        // fresh login rather than reporting a bare status code.
        "invalid_grant" => PollOutcome::Failed(
            "device code is invalid or already used; run shunt login kimi --name <account-name> again"
                .to_string(),
        ),
        _ => {
            let description = body
                .get("error_description")
                .and_then(Value::as_str)
                .or(Some(error))
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown error");
            PollOutcome::Failed(format!("device authorization failed: {description}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_device_code_with_defaults() {
        let device = parse_device_code(&json!({
            "device_code": "Q5KS-GNISQ5KSGNIS",
            "user_code": "Q5KS-GNIS",
            "verification_uri": "https://www.kimi.com/code/authorize_device",
            "verification_uri_complete": "https://www.kimi.com/code/authorize_device?user_code=Q5KS-GNIS",
            "expires_in": 1800,
            "interval": 5
        }))
        .unwrap();
        assert_eq!(device.device_code, "Q5KS-GNISQ5KSGNIS");
        assert_eq!(device.expires_in, 1800);
        assert_eq!(device.interval, 5);
        assert_eq!(
            device.verification_uri_complete.as_deref(),
            Some("https://www.kimi.com/code/authorize_device?user_code=Q5KS-GNIS")
        );

        // Missing/zero interval floors to the minimum; missing expiry defaults
        // to the measured 1800s, and verification_uri_complete is optional.
        let device = parse_device_code(&json!({
            "device_code": "d",
            "user_code": "u",
            "verification_uri": "https://www.kimi.com/code/authorize_device"
        }))
        .unwrap();
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);
        assert_eq!(device.expires_in, DEFAULT_EXPIRES_SECS);
        assert!(device.verification_uri_complete.is_none());

        // A zero/garbage interval is treated as absent and falls back to the
        // default (guarding the poll loop from a 0-second busy spin).
        let device = parse_device_code(&json!({
            "device_code": "d",
            "user_code": "u",
            "verification_uri": "https://www.kimi.com/code/authorize_device",
            "interval": 0
        }))
        .unwrap();
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn parse_device_code_requires_core_fields() {
        // Missing any of device_code / user_code / verification_uri yields None
        // rather than a half-built DeviceCode.
        assert!(parse_device_code(&json!({
            "user_code": "u",
            "verification_uri": "https://www.kimi.com/code/authorize_device"
        }))
        .is_none());
    }

    #[test]
    fn classifies_poll_errors() {
        assert_eq!(
            classify_poll_error(&json!({"error": "authorization_pending"})),
            PollOutcome::Pending
        );
        assert_eq!(
            classify_poll_error(&json!({"error": "slow_down"})),
            PollOutcome::SlowDown
        );
        assert!(matches!(
            classify_poll_error(&json!({"error": "access_denied"})),
            PollOutcome::Failed(_)
        ));
        assert!(matches!(
            classify_poll_error(&json!({"error": "expired_token"})),
            PollOutcome::Failed(_)
        ));
        // The measured bad-device_code response: invalid_grant, terminal, and
        // its message must point back at re-running login by name.
        match classify_poll_error(
            &json!({"error": "invalid_grant", "error_description": "The provided authorization grant is invalid"}),
        ) {
            PollOutcome::Failed(reason) => {
                assert!(reason.contains("shunt login kimi --name"), "got: {reason}");
            }
            other => panic!("expected failure, got {other:?}"),
        }
        match classify_poll_error(&json!({"error": "boom", "error_description": "kaboom"})) {
            PollOutcome::Failed(reason) => assert!(reason.contains("kaboom")),
            other => panic!("expected failure, got {other:?}"),
        }
        // An unknown error with no description still fails, using the raw code.
        match classify_poll_error(&json!({"error": "boom"})) {
            PollOutcome::Failed(reason) => assert!(reason.contains("boom")),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn next_interval_bumps_and_caps() {
        assert_eq!(next_interval(1), 6);
        assert_eq!(next_interval(27), 30);
        assert_eq!(next_interval(30), 30);
    }

    #[test]
    fn poll_deadline_saturates_instead_of_panicking() {
        let now = Instant::now();
        // A pathological server-supplied expires_in must not panic `Instant +
        // Duration` — it degrades to the default lifetime instead.
        assert_eq!(
            poll_deadline(now, u64::MAX),
            now + Duration::from_secs(DEFAULT_EXPIRES_SECS)
        );
        // A normal value is used as-is.
        assert_eq!(poll_deadline(now, 900), now + Duration::from_secs(900));
    }

    #[test]
    fn truncate_for_error_caps_long_bodies_on_a_char_boundary() {
        let short = "short body";
        assert_eq!(truncate_for_error(short), short);
        assert!(matches!(truncate_for_error(short), Cow::Borrowed(_)));

        // 500 multi-byte characters: a naive byte-slice `&text[..200]` would
        // panic here since 200 bytes lands mid-codepoint.
        let long: String = "한".repeat(500);
        let truncated = truncate_for_error(&long);
        let chars: Vec<char> = truncated.chars().collect();
        assert_eq!(chars.len(), 201, "200 chars + the ellipsis");
        assert_eq!(chars.last(), Some(&'\u{2026}'));
        assert!(truncated.starts_with(&long[..long.char_indices().nth(200).unwrap().0]));
    }

    fn test_device(interval: u64, expires_in: u64) -> DeviceCode {
        DeviceCode {
            device_code: "dev".to_string(),
            user_code: "Q5KS-GNIS".to_string(),
            verification_uri: "https://www.kimi.com/code/authorize_device".to_string(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    #[tokio::test]
    async fn poll_for_tokens_continues_past_measured_400_pending_then_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Regression for the critical fact: Kimi's measured pending-poll
        // response is HTTP 400 with `{"error":"authorization_pending", ...}`,
        // not HTTP 200 (a reference implementation's comment claiming 200 is
        // wrong) — the loop must keep polling through it, not bail.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "authorization_pending",
                "error_description": "Authorization is pending"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let device = test_device(1, 30);
        let token_url = format!("{}/token", server.uri());
        let tokens = poll_for_tokens(&client, &device, &token_url, "device-1")
            .await
            .expect("second poll should succeed after the pending 400");
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn poll_for_tokens_reports_invalid_grant_as_terminal_not_a_bare_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "The provided authorization grant is invalid"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let device = test_device(1, 30);
        let token_url = format!("{}/token", server.uri());
        let error = poll_for_tokens(&client, &device, &token_url, "device-1")
            .await
            .expect_err("a bad device_code must be terminal");
        assert!(
            error.to_string().contains("shunt login kimi --name"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn poll_for_tokens_times_out_at_the_expiry_deadline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"})),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        // interval 1s, expires 1s: the loop polls once, sleeps past the
        // deadline, then bails out on the next deadline check.
        let device = test_device(1, 1);
        let token_url = format!("{}/token", server.uri());
        let error = poll_for_tokens(&client, &device, &token_url, "device-1")
            .await
            .expect_err("poll should time out");
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn poll_for_tokens_gives_up_after_consecutive_transport_failures() {
        // Port 1 needs root to bind, so nothing is listening on it here —
        // every connection attempt fails deterministically and immediately
        // with connection-refused, unlike a dropped MockServer whose port
        // could in principle be reclaimed before the request lands.
        let client = reqwest::Client::new();
        // A long expiry with a short interval: if the consecutive-failure
        // cap regressed back to unbounded retry, this test would run toward
        // the full deadline instead of failing fast, so the deadline itself
        // must not be what ends this test.
        let device = test_device(1, 1800);
        let error = poll_for_tokens(&client, &device, "http://127.0.0.1:1/token", "device-1")
            .await
            .expect_err("a permanently unreachable endpoint must fail fast");
        let message = error.to_string();
        assert!(message.contains("3 times in a row"), "got: {message}");
        assert!(
            !message.contains("timed out"),
            "must exit via the consecutive-failure cap, not the deadline: {message}"
        );
    }
}
