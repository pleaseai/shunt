//! Tests for [`super`]: the device-authorization flow and its poll loop.
//!
//! In a sibling file rather than inline, matching the convention already used
//! by `src/auth/slots.rs`. A pure move — no test was added, removed, or
//! rewritten here.

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
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_request"})))
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
fn a_device_code_lifetime_is_capped_at_the_gateways_own_ttl() {
    // A year of `expires_in` would otherwise hold the terminal polling for
    // a year: overflow was guarded, but any non-overflowing value honored.
    let device = parse_device_code(&json!({
        "device_code": "d",
        "user_code": "u",
        "verification_uri": "https://gateway.example/device",
        "expires_in": 31_536_000_u64,
        "interval": 5
    }))
    .expect("the response is otherwise well formed");
    assert_eq!(device.expires_in, DEFAULT_EXPIRES_SECS);

    // Anything at or under the ceiling is honored as sent.
    let short = parse_device_code(&json!({
        "device_code": "d",
        "user_code": "u",
        "verification_uri": "https://gateway.example/device",
        "expires_in": 120,
        "interval": 5
    }))
    .unwrap();
    assert_eq!(short.expires_in, 120);
}

#[test]
fn gateway_chosen_error_strings_are_capped_like_raw_bodies() {
    // Pulling the string out of a JSON field does not make it shorter: the
    // same proxy or WAF can put a whole HTML page in `error_description`.
    let flood = "x".repeat(5_000);
    let PollOutcome::Failed(message) =
        classify_poll_error(&json!({"error": "boom", "error_description": flood}), "")
    else {
        panic!("an unknown error is terminal");
    };
    assert!(
        message.chars().count() < 500,
        "the description must be capped, got {} chars",
        message.chars().count()
    );
}

/// A listener that accepts and then says nothing, for the two bounded calls in
/// [`super::run_bounded`].
async fn silent_listener() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");
    let handle = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    (format!("http://{address}"), handle)
}

#[tokio::test]
async fn a_silent_gateway_bounds_the_login_discovery_rather_than_hanging() {
    // The wiring, not the helper: `bounded` is covered on the refresh path, but
    // nothing asserted that `run` actually wraps these two calls, so a refactor
    // could unwrap them with the suite still green.
    let (gateway_url, server) = silent_listener().await;

    let started = std::time::Instant::now();
    let error = run_bounded(&gateway_url, true, Duration::from_secs(1))
        .await
        .expect_err("a silent gateway must not hang the terminal");
    assert!(error.to_string().contains("did not answer"), "got: {error}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the bound must actually bound: waited {:?}",
        started.elapsed()
    );

    server.abort();
}

#[tokio::test]
async fn a_silent_device_code_endpoint_is_bounded_too() {
    // Discovery answers, so this reaches the *second* bounded call — which a
    // test that only exercised discovery would leave unwrapped and unnoticed.
    let (silent_url, silent) = silent_listener().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": format!("{silent_url}/oauth/device_authorization"),
            "token_endpoint": format!("{silent_url}/oauth/token")
        })))
        .mount(&server)
        .await;

    let error = run_bounded(&server.uri(), true, Duration::from_secs(1))
        .await
        .expect_err("a silent device-code endpoint must not hang the terminal");
    let message = format!("{error:#}");
    assert!(
        message.contains("device-code request"),
        "the second call must be the one that timed out: {message}"
    );

    silent.abort();
}

#[test]
fn classifies_poll_errors() {
    assert_eq!(
        classify_poll_error(&json!({"error": "authorization_pending"}), ""),
        PollOutcome::Pending
    );
    assert_eq!(
        classify_poll_error(&json!({"error": "slow_down"}), ""),
        PollOutcome::SlowDown
    );
    for terminal in ["access_denied", "expired_token", "unsupported_grant_type"] {
        assert!(
            matches!(
                classify_poll_error(&json!({ "error": terminal }), ""),
                PollOutcome::Failed(_)
            ),
            "{terminal} must be terminal"
        );
    }
    match classify_poll_error(&json!({"error": "boom", "error_description": "kaboom"}), "") {
        PollOutcome::Failed(reason) => assert!(reason.contains("kaboom")),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn an_unrecognized_poll_body_is_reported_with_what_actually_arrived() {
    // Valid JSON in a shape the client does not know — an empty object, or
    // a health-check body that matched by accident. Reporting only
    // "unknown error" tells the operator nothing about what came back,
    // while the invalid-JSON path one branch up does show the body.
    let PollOutcome::Failed(message) = classify_poll_error(&json!({}), r#"{"status":"ok"}"#) else {
        panic!("an unrecognized body is terminal");
    };
    assert!(
        message.contains(r#"{"status":"ok"}"#),
        "the message must show what arrived: {message}"
    );

    // Still capped: the same proxy or WAF can answer with a whole page.
    let flood = "x".repeat(5_000);
    let PollOutcome::Failed(message) = classify_poll_error(&json!({}), &flood) else {
        panic!("an unrecognized body is terminal");
    };
    assert!(
        message.chars().count() < 500,
        "the raw body must be truncated, got {} chars",
        message.chars().count()
    );
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
