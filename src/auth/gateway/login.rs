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
    discover, expires_at_ms, parse_token_response, truncate_for_error, CLIENT_ID,
    DEVICE_CODE_GRANT_TYPE,
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
    let gateway_url = normalize_gateway_url(gateway_url)?;
    // Every request below carries a secret this flow cannot afford to leak
    // off-origin: the device-authorization POST returns a one-time
    // `device_code`, and the poll redeems it for the session's tokens. The
    // redirect-hardened client refuses to forward either to an unsafe host.
    let client = crate::auth::shared::token_refresh_client();
    let discovery = discover(&client, &gateway_url).await?;
    let device = request_device_code(&client, &discovery.device_authorization_endpoint)
        .await
        .context("failed to request a device code from the gateway")?;

    let prompt_url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    println!("To authorize this machine against {gateway_url}, open:\n");
    println!("    {prompt_url}\n");
    println!(
        "and confirm the code: {}\n(waiting for approval — this window will update automatically)",
        device.user_code
    );
    if !manual {
        match browser_open_refusal(&prompt_url) {
            Some(reason) => eprintln!(
                "Not opening this URL automatically: {reason}. It was chosen by the gateway, not \
                 by shunt — open it yourself only if it looks like the deployment's own sign-in \
                 page."
            ),
            None => {
                if let Err(error) = crate::auth::shared::open_url_async(&prompt_url).await {
                    eprintln!("Could not open a browser ({error}); open the URL above manually.");
                }
            }
        }
    }

    let tokens = poll_for_tokens(&client, &device, &discovery.token_endpoint)
        .await
        .context("gateway device authorization failed")?;

    let session = GatewaySession {
        gateway_url,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: expires_at_ms(tokens.expires_in, std::time::SystemTime::now()),
    };
    let path = store::session_path();
    store::write_session(&path, &session)?;

    println!("\nLogin successful. Session saved to {}", path.display());
    println!("Use it from Claude Code with: \"apiKeyHelper\": \"shunt gateway token\"");
    Ok(())
}

/// `shunt gateway logout` — drop the stored session. Idempotent.
pub fn logout() -> anyhow::Result<()> {
    let path = store::session_path();
    if store::remove_session(&path)? {
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
        bail!("the gateway refused the device-code request ({error})");
    }
    bail!(
        "the gateway's device-code response is missing device_code / user_code / \
         verification_uri (HTTP {status}): {}",
        truncate_for_error(&text)
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
        expires_in: positive_secs(value.get("expires_in"), DEFAULT_EXPIRES_SECS),
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

async fn poll_for_tokens(
    client: &reqwest::Client,
    device: &DeviceCode,
    token_endpoint: &str,
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
        // A connection reset partway through the *body* is the same class of
        // blip as a failed connect, so the two are collapsed into one outcome
        // here rather than only the send being retried. Reading the body inside
        // this match is what puts it on the retry path below.
        let attempt = match request.send().await {
            Ok(response) => {
                let status = response.status();
                response.text().await.map(|text| (status, text))
            }
            Err(error) => Err(error),
        };
        // A single DNS/TLS/connect/read blip must not abort the login: remember
        // the error and fall through to the deadline-bounded sleep, then retry.
        // A permanently unreachable endpoint still fails fast via the
        // consecutive-failure cap rather than spinning until the deadline.
        let (status, text) = match attempt {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("gateway token poll request failed, will retry: {error}");
                last_transport_error = Some(error.to_string());
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
                truncate_for_error(&text)
            ),
        };
        if let Some(tokens) = parse_token_response(&value) {
            return Ok(tokens);
        }
        match classify_poll_error(&value) {
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
    (current + SLOW_DOWN_INCREMENT_SECS).min(MAX_INTERVAL_SECS)
}

fn classify_poll_error(body: &Value) -> PollOutcome {
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
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_device(interval: u64, expires_in: u64) -> DeviceCode {
        DeviceCode {
            device_code: "device-code-1".to_string(),
            user_code: "BCDF-GHJK".to_string(),
            verification_uri: "https://gateway.example/device".to_string(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    fn pending() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"}))
    }

    fn granted() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 3600
        }))
    }

    #[test]
    fn normalize_gateway_url_trims_and_rejects_unusable_urls() {
        assert_eq!(
            normalize_gateway_url("  https://gateway.example/  ").unwrap(),
            "https://gateway.example"
        );
        assert!(normalize_gateway_url("").is_err());
        assert!(normalize_gateway_url("   ").is_err());
        assert!(normalize_gateway_url("gateway.example").is_err());
        // A non-http(s) scheme parses as a URL but can never carry the flow.
        assert!(normalize_gateway_url("ftp://gateway.example").is_err());
        // Plain HTTP is a warning, not a refusal: private deployments are real.
        assert!(normalize_gateway_url("http://gateway.example").is_ok());
    }

    #[tokio::test]
    async fn device_authorization_sends_the_pinned_client_id() {
        let server = MockServer::start().await;
        // The gateway answers any other client_id with a 400, so the match on
        // the body is the point of this test, not incidental.
        Mock::given(method("POST"))
            .and(path("/oauth/device_authorization"))
            .and(body_string_contains("client_id=claude-code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": "device-code-1",
                "user_code": "BCDF-GHJK",
                "verification_uri": "https://gateway.example/device",
                "verification_uri_complete": "https://gateway.example/device?user_code=BCDF-GHJK",
                "expires_in": 600,
                "interval": 5
            })))
            .mount(&server)
            .await;

        let endpoint = format!("{}/oauth/device_authorization", server.uri());
        let device = request_device_code(&reqwest::Client::new(), &endpoint)
            .await
            .expect("the device-code request should be accepted");
        assert_eq!(device.device_code, "device-code-1");
        assert_eq!(device.user_code, "BCDF-GHJK");
        assert_eq!(device.expires_in, 600);
        assert_eq!(device.interval, 5);
        assert_eq!(
            device.verification_uri_complete.as_deref(),
            Some("https://gateway.example/device?user_code=BCDF-GHJK")
        );
    }

    #[tokio::test]
    async fn device_authorization_reports_a_refusal_from_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/device_authorization"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_request"})),
            )
            .mount(&server)
            .await;

        let endpoint = format!("{}/oauth/device_authorization", server.uri());
        let error = request_device_code(&reqwest::Client::new(), &endpoint)
            .await
            .expect_err("a refused device-code request must not yield a DeviceCode");
        assert!(
            error.to_string().contains("invalid_request"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn poll_continues_through_a_400_authorization_pending() {
        // Regression for the load-bearing wire fact: the gateway answers an
        // ordinary pending poll with HTTP 400. An implementation that branches
        // on the status before parsing the body bails here instead of polling.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("device_code=device-code-1"))
            .respond_with(pending())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(granted())
            .mount(&server)
            .await;

        let token_endpoint = format!("{}/oauth/token", server.uri());
        let tokens = poll_for_tokens(
            &reqwest::Client::new(),
            &test_device(1, 60),
            &token_endpoint,
        )
        .await
        .expect("the poll must survive the pending 400 and pick up the grant");
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token, "refresh-1");
    }

    // Real time, deliberately: under `start_paused` the clock auto-advances
    // while the poll waits on the socket, and reqwest's own pool timers then
    // dominate the measurement (hundreds of virtual seconds either way), so a
    // paused-clock version of this assertion passes even with the widening
    // removed. The ~6s cost buys the only check that the loop actually applies
    // `next_interval` rather than merely computing it.
    #[tokio::test]
    async fn slow_down_widens_the_interval_before_the_next_poll() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "slow_down"})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(granted())
            .mount(&server)
            .await;

        let token_endpoint = format!("{}/oauth/token", server.uri());
        let started = std::time::Instant::now();
        let tokens = poll_for_tokens(
            &reqwest::Client::new(),
            &test_device(1, 600),
            &token_endpoint,
        )
        .await
        .expect("slow_down is not terminal");
        assert_eq!(tokens.access_token, "access-1");
        // Starting at a 1s interval, `slow_down` must widen it to 6s before the
        // next poll. Without the widening the whole run takes about 1s.
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "slow_down must add {SLOW_DOWN_INCREMENT_SECS}s to the interval, waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn expired_token_and_access_denied_are_each_fatal_and_distinct() {
        for (error, expected) in [
            ("expired_token", "expired or was already used"),
            ("access_denied", "denied in the browser"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/oauth/token"))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": error})))
                .mount(&server)
                .await;

            let token_endpoint = format!("{}/oauth/token", server.uri());
            let failure = poll_for_tokens(
                &reqwest::Client::new(),
                &test_device(1, 600),
                &token_endpoint,
            )
            .await
            .expect_err("{error} must be terminal");
            let message = failure.to_string();
            assert!(message.contains(expected), "{error} produced: {message}");
            assert!(
                message.contains("shunt gateway login"),
                "{error} must say how to recover: {message}"
            );
        }
    }

    #[tokio::test]
    async fn poll_stops_at_the_device_code_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(pending())
            .mount(&server)
            .await;

        let token_endpoint = format!("{}/oauth/token", server.uri());
        // interval 1s, expires_in 1s: one poll, one sleep past the deadline,
        // then the loop condition ends it rather than polling forever.
        let error = poll_for_tokens(&reqwest::Client::new(), &test_device(1, 1), &token_endpoint)
            .await
            .expect_err("an unapproved device code must eventually give up");
        let message = error.to_string();
        assert!(message.contains("timed out"), "got: {message}");
        // Every poll reached the server, so blaming a transport failure would
        // misreport a plain "the user never approved" timeout.
        assert!(
            !message.contains("the last poll attempt failed"),
            "got: {message}"
        );
    }

    #[tokio::test]
    async fn poll_gives_up_after_consecutive_transport_failures() {
        // Port 1 needs root to bind, so nothing is listening there and every
        // connection attempt fails immediately and deterministically.
        // `expires_in` is long: if the cap regressed to unbounded retry this
        // would run toward the deadline instead, so the deadline is not what
        // ends this test.
        let error = poll_for_tokens(
            &reqwest::Client::new(),
            &test_device(1, 600),
            "http://127.0.0.1:1/oauth/token",
        )
        .await
        .expect_err("a permanently unreachable gateway must fail fast");
        let message = error.to_string();
        assert!(message.contains("3 times in a row"), "got: {message}");
        assert!(!message.contains("timed out"), "got: {message}");
    }

    /// A raw listener that answers the first `truncated` requests with headers
    /// promising more body than it sends and then closes the connection, and
    /// every request after that with a grant. wiremock cannot express a body
    /// that dies mid-flight, and that is exactly the failure under test.
    async fn truncating_token_endpoint(truncated: usize) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let endpoint = format!(
            "http://{}/oauth/token",
            listener.local_addr().expect("local address")
        );
        let handle = tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                if served < truncated {
                    served += 1;
                    // `content-length` promises 64 bytes; 27 are sent and the
                    // connection then closes, so the *body read* fails after
                    // the headers already arrived successfully.
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
                              content-length: 64\r\n\r\n{\"error\":\"authorization_pend",
                        )
                        .await;
                } else {
                    let body = br#"{"access_token":"access-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":3600}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                         connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                }
                let _ = socket.shutdown().await;
            }
        });
        (endpoint, handle)
    }

    #[tokio::test]
    async fn a_body_read_that_dies_after_the_headers_is_retried_not_fatal() {
        let (endpoint, server) = truncating_token_endpoint(2).await;

        // Two truncated bodies stay under MAX_CONSECUTIVE_TRANSPORT_FAILURES,
        // so the login must survive them and succeed on the third poll. Reading
        // the body outside the retry path turns the first one into an immediate
        // "invalid gateway token response" and aborts a ten-minute login over a
        // local read failure.
        let tokens = poll_for_tokens(&reqwest::Client::new(), &test_device(1, 600), &endpoint)
            .await
            .expect("a truncated body is a transport blip, not a malformed response");
        assert_eq!(tokens.access_token, "access-1");

        server.abort();
    }

    #[tokio::test]
    async fn repeated_body_read_failures_still_trip_the_consecutive_cap() {
        // The retry path must not become an unbounded one: a body that never
        // completes has to fail fast on the same counter a failed connect does.
        let (endpoint, server) = truncating_token_endpoint(usize::MAX).await;

        let error = poll_for_tokens(&reqwest::Client::new(), &test_device(1, 600), &endpoint)
            .await
            .expect_err("a body that never completes must eventually give up");
        let message = error.to_string();
        assert!(message.contains("3 times in a row"), "got: {message}");
        assert!(!message.contains("timed out"), "got: {message}");

        server.abort();
    }

    #[test]
    fn only_http_and_https_verification_urls_are_opened_automatically() {
        assert_eq!(browser_open_refusal("https://gateway.example/device"), None);
        assert_eq!(
            browser_open_refusal("http://127.0.0.1:3001/device?user_code=BCDF-GHJK"),
            None
        );

        // The device-authorization response is the one URL in shunt that a
        // remote party chooses, so each of these reaches the OS handler unless
        // the scheme is checked first.
        for hostile in [
            "file:///etc/passwd",
            "smb://attacker.example/share",
            "javascript:alert(1)",
            // Not a URL at all: `open(1)` would read the leading dash as a flag.
            "-n",
            "",
        ] {
            let refusal = browser_open_refusal(hostile)
                .unwrap_or_else(|| panic!("{hostile:?} must not be auto-opened"));
            assert!(
                refusal.contains("verification URL"),
                "the refusal must say what it is refusing: {refusal}"
            );
        }
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
        for terminal in ["access_denied", "expired_token", "unsupported_grant_type"] {
            assert!(
                matches!(
                    classify_poll_error(&json!({ "error": terminal })),
                    PollOutcome::Failed(_)
                ),
                "{terminal} must be terminal"
            );
        }
        match classify_poll_error(&json!({"error": "boom", "error_description": "kaboom"})) {
            PollOutcome::Failed(reason) => assert!(reason.contains("kaboom")),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn next_interval_bumps_by_five_and_caps_at_thirty() {
        let mut interval = 5;
        for expected in [10, 15, 20, 25, 30, 30] {
            interval = next_interval(interval);
            assert_eq!(interval, expected);
        }
    }

    #[test]
    fn parse_device_code_defaults_and_required_fields() {
        let device = parse_device_code(&json!({
            "device_code": "d",
            "user_code": "u",
            "verification_uri": "https://gateway.example/device"
        }))
        .unwrap();
        assert_eq!(device.expires_in, DEFAULT_EXPIRES_SECS);
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);
        assert!(device.verification_uri_complete.is_none());

        // A zero interval would busy-spin the poll loop, so it is treated as
        // absent rather than honored.
        let device = parse_device_code(&json!({
            "device_code": "d",
            "user_code": "u",
            "verification_uri": "https://gateway.example/device",
            "interval": 0
        }))
        .unwrap();
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);

        assert!(parse_device_code(&json!({"user_code": "u"})).is_none());
    }

    #[test]
    fn poll_deadline_saturates_instead_of_panicking() {
        let now = Instant::now();
        assert_eq!(
            poll_deadline(now, u64::MAX),
            now + Duration::from_secs(DEFAULT_EXPIRES_SECS)
        );
        assert_eq!(poll_deadline(now, 300), now + Duration::from_secs(300));
    }
}
