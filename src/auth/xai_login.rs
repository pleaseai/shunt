//! `shunt login xai` — the RFC 8628 device-authorization flow for xAI
//! subscription OAuth (SuperGrok / X Premium+).
//!
//! No loopback callback server is needed: shunt requests a device code, prints
//! a verification URL and short user code, and long-polls xAI's token endpoint
//! until the user approves in a browser (on any device). The resulting tokens
//! are written to the shunt-owned credential file (see [`super::xai_auth`]).
//!
//! Logs go to stderr (the crate convention); the URL and user code are printed
//! to stdout with plain `println!` so they are always visible to the operator.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::Value;
use tokio::time::{sleep, Instant};

use crate::auth::default_xai_auth_path;
use crate::auth::shared::jwt_exp;
use crate::auth::xai_auth::{
    parse_token_response, write_tokens, CLIENT_ID, DEVICE_CODE_GRANT_TYPE, DEVICE_CODE_URL, SCOPE,
    TOKEN_URL,
};

const DEFAULT_INTERVAL_SECS: u64 = 5;
const MIN_INTERVAL_SECS: u64 = 1;
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const MAX_INTERVAL_SECS: u64 = 30;
const DEFAULT_EXPIRES_SECS: u64 = 5 * 60;

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

/// Run the device-code login for `provider`. Only `xai` is supported today.
pub async fn run(provider: &str) -> anyhow::Result<()> {
    if provider != "xai" {
        bail!("unknown login provider {provider:?}; supported: xai");
    }
    let client = reqwest::Client::new();
    let device = request_device_code(&client)
        .await
        .context("failed to request xAI device code")?;

    let prompt_url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    println!("To authorize shunt with your xAI subscription, open:\n");
    println!("    {prompt_url}\n");
    println!(
        "and confirm the code: {}\n(waiting for approval — this window will update automatically)",
        device.user_code
    );

    let tokens = poll_for_tokens(&client, &device, TOKEN_URL)
        .await
        .context("xAI device authorization failed")?;

    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("xAI did not return a refresh_token; cannot persist login"))?;
    let path = default_xai_auth_path();
    write_tokens(
        &path,
        &tokens.access_token,
        refresh_token,
        tokens.id_token.as_deref(),
    )
    .with_context(|| format!("failed to write xAI credentials to {}", path.display()))?;

    println!(
        "\nLogin successful. Credentials saved to {}",
        path.display()
    );
    if let Some(exp) = jwt_exp(&tokens.access_token) {
        println!(
            "Access token valid until {} (shunt refreshes it automatically).",
            crate::auth::shared::format_iso8601(exp)
        );
    }
    Ok(())
}

async fn request_device_code(client: &reqwest::Client) -> anyhow::Result<DeviceCode> {
    let response = client
        .post(DEVICE_CODE_URL)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("xAI device-code request failed (HTTP {status}): {text}");
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid device-code response: {text}"))?;
    parse_device_code(&value).ok_or_else(|| {
        anyhow!("device-code response missing device_code / user_code / verification_uri")
    })
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

async fn poll_for_tokens(
    client: &reqwest::Client,
    device: &DeviceCode,
    token_url: &str,
) -> anyhow::Result<crate::auth::xai_auth::TokenResponse> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(MIN_INTERVAL_SECS);
    while Instant::now() < deadline {
        let response = client
            .post(token_url)
            .header("accept", "application/json")
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ])
            .send()
            .await?;
        let success = response.status().is_success();
        let text = response.text().await.unwrap_or_default();
        if success {
            let value: Value = serde_json::from_str(&text).context("invalid token response")?;
            let tokens = parse_token_response(&value)
                .ok_or_else(|| anyhow!("token response missing access_token"))?;
            if tokens.refresh_token.is_none() {
                bail!("xAI token response did not include a refresh_token");
            }
            return Ok(tokens);
        }
        let error_body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match classify_poll_error(&error_body) {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => {
                interval = next_interval(interval);
            }
            PollOutcome::Failed(reason) => bail!("{reason}"),
        }
        sleep(Duration::from_secs(interval)).await;
    }
    bail!("xAI device authorization timed out; run shunt login xai to try again")
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
        "expired_token" => {
            PollOutcome::Failed("device code expired; run shunt login xai again".to_string())
        }
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
            "device_code": "dev",
            "user_code": "ABCD-1234",
            "verification_uri": "https://x.ai/device",
            "verification_uri_complete": "https://x.ai/device?code=ABCD-1234",
            "expires_in": 600,
            "interval": 5
        }))
        .unwrap();
        assert_eq!(device.device_code, "dev");
        assert_eq!(device.expires_in, 600);
        assert_eq!(device.interval, 5);
        assert_eq!(
            device.verification_uri_complete.as_deref(),
            Some("https://x.ai/device?code=ABCD-1234")
        );

        // Missing/zero interval floors to the minimum; missing expiry defaults.
        let device = parse_device_code(&json!({
            "device_code": "d",
            "user_code": "u",
            "verification_uri": "https://x.ai/device"
        }))
        .unwrap();
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);
        assert_eq!(device.expires_in, DEFAULT_EXPIRES_SECS);
        assert!(device.verification_uri_complete.is_none());
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
        match classify_poll_error(&json!({"error": "boom", "error_description": "kaboom"})) {
            PollOutcome::Failed(reason) => assert!(reason.contains("kaboom")),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn next_interval_bumps_and_caps() {
        assert_eq!(next_interval(1), 6);
        assert_eq!(next_interval(27), 30);
        assert_eq!(next_interval(30), 30);
    }

    fn test_device(interval: u64, expires_in: u64) -> DeviceCode {
        DeviceCode {
            device_code: "dev".to_string(),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://x.ai/device".to_string(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    #[tokio::test]
    async fn poll_for_tokens_continues_past_pending_then_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let device = test_device(1, 30);
        let token_url = format!("{}/token", server.uri());
        let tokens = poll_for_tokens(&client, &device, &token_url)
            .await
            .expect("second poll should succeed");
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    }

    // The slow_down clamp arithmetic (+5s, capped at 30s) is covered by
    // `next_interval_bumps_and_caps` above without sleeping. A full
    // integration test would need a real ~6s sleep to observe the bumped
    // interval before the next poll — too slow for this suite, so the pure
    // helper is the coverage for that behavior (see the module doc comment
    // on `PollOutcome::SlowDown`'s handling in `poll_for_tokens`).

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
        let error = poll_for_tokens(&client, &device, &token_url)
            .await
            .expect_err("poll should time out");
        assert!(error.to_string().contains("timed out"));
    }
}
