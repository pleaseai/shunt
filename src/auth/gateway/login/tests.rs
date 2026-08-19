//! Tests for [`super`]: the device-authorization flow and its poll loop.
//!
//! In a sibling file rather than inline, matching the convention already used
//! by `src/auth/slots.rs`.

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

/// The existing call sites all want the production per-attempt bound; only the
/// stalled-gateway test below drives it explicitly.
async fn poll(
    client: &reqwest::Client,
    device: &DeviceCode,
    token_endpoint: &str,
) -> anyhow::Result<super::super::auth::TokenResponse> {
    poll_for_tokens(client, device, token_endpoint, NETWORK_TIMEOUT).await
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
    let tokens = poll(
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
    let tokens = poll(
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
        let failure = poll(
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
    let error = poll(&reqwest::Client::new(), &test_device(1, 1), &token_endpoint)
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
    let error = poll(
        &reqwest::Client::new(),
        &test_device(1, 600),
        "http://127.0.0.1:1/oauth/token",
    )
    .await
    .expect_err("a permanently unreachable gateway must fail fast");
    let message = error.to_string();
    assert!(message.contains("3 times in a row"), "got: {message}");
    assert!(!message.contains("timed out"), "got: {message}");
    // The reported transport error must not carry the endpoint it was talking
    // to: reqwest's `Display` appends it, and this string is both logged and
    // repeated in the timeout diagnostic.
    assert!(
        !message.contains("127.0.0.1:1"),
        "the discovered token endpoint must be stripped from the diagnostic: {message}"
    );
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
    let tokens = poll(&reqwest::Client::new(), &test_device(1, 600), &endpoint)
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

    let error = poll(&reqwest::Client::new(), &test_device(1, 600), &endpoint)
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
fn next_interval_saturates_on_a_hostile_interval() {
    // `interval` is server-supplied and RFC 8628 puts no ceiling on it, so a
    // near-`u64::MAX` value followed by `slow_down` reaches this: a plain `+`
    // panics in debug and wraps to a busy-spin interval in release.
    assert_eq!(next_interval(u64::MAX), MAX_INTERVAL_SECS);
    assert_eq!(
        next_interval(u64::MAX - SLOW_DOWN_INCREMENT_SECS),
        MAX_INTERVAL_SECS
    );
}

/// The stall a per-attempt bound is the only thing that catches: the socket is
/// accepted and then nothing is ever written back. The loop's `deadline` check
/// runs only *between* attempts, so without the bound the very first poll
/// awaits forever — after the verification URL has already been printed, which
/// makes it look like a login merely waiting for approval.
#[tokio::test]
async fn a_stalled_poll_attempt_still_reaches_the_device_code_deadline() {
    let (silent_url, silent) = silent_listener().await;
    let server = MockServer::start().await;
    // Discovery and the device-code request answer normally; only the token
    // endpoint stalls, so the login gets all the way past the printed
    // verification URL before it hits the socket that never replies.
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": format!("{}/oauth/device_authorization", server.uri()),
            "token_endpoint": format!("{silent_url}/oauth/token")
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/device_authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "device-code-1",
            "user_code": "BCDF-GHJK",
            "verification_uri": "https://gateway.example/device",
            // Short, so the deadline — not the consecutive-failure cap — is
            // what this test observes.
            "expires_in": 2,
            "interval": 1
        })))
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    let error = run_bounded(&server.uri(), true, Duration::from_secs(1))
        .await
        .expect_err("a token endpoint that never answers must not hang the login");
    let message = format!("{error:#}");
    assert!(
        message.contains("timed out"),
        "the device-code deadline must be what ends it: {message}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the poll must be bounded per attempt: waited {:?}",
        started.elapsed()
    );

    silent.abort();
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

/// The race a login without the session lock loses: the login stores its new
/// session first, an in-flight refresh's writeback lands after it, and the
/// gateway URL and token pair the user just replaced are back on disk while
/// the command has already printed "Login successful".
///
/// Drives the whole command rather than the store helper, so deleting the lock
/// from `run_bounded`'s write is what turns this red.
///
/// The refresher is released only after the login is observed to have reached
/// its write, in two stages, and the order of the two is the whole point.
///
/// First `await_token_post` waits *causally* for the device flow's last
/// round-trip, with a deadline generous enough to be unreachable in practice.
/// That takes discovery and the two POSTs out of the bounded window entirely —
/// they are waited for, never budgeted for. Then `await_stored_gateway` polls
/// the session file on a short deadline, and by then the only work left is
/// "read the response, build the session, write the file". Collapsing the two
/// into a single short poll would put three network round-trips back inside a
/// budget sized for a microseconds-scale step, and a loaded runner that
/// overran it would flip a defective build green — the exact failure this
/// staging exists to remove.
///
/// The main task therefore never *times* the login's write, it **observes**
/// it: when the lock is missing, the refresher's write is ordered after the
/// login's by construction, and the stale value it restores is what the
/// assertion catches. That holds no matter how slow the runner is.
///
/// The write poll's deadline is the one duration left, and it is deliberately
/// *expected to expire* on a correct build: a login that takes the lock cannot
/// write while the refresher holds it, so the poll must time out. That is the
/// lock doing its job, not a fallback — which is why it cannot be "simplified"
/// back into a sleep. A sleep would make the two builds indistinguishable
/// again, and expiring early would then silently flip a defective build green.
/// Here, expiring early only ends the test sooner.
///
/// Unix only, like the logout counterpart in `store`: off Unix there is no
/// advisory lock to serialize on.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_waits_for_an_in_flight_refresh_before_storing_the_session() {
    let server = MockServer::start().await;
    mount_device_flow(&server, granted()).await;

    let _env = store::TEST_ENV_LOCK.lock().await;
    let dir = store::temp_dir("login-race");
    let path = dir.join("session.json");
    let _session_file =
        crate::auth::shared::EnvVarGuard::set("SHUNT_GATEWAY_SESSION_FILE", path.as_os_str());
    store::write_session(&path, &store::test_session("https://old.example", 0)).unwrap();

    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let refresher_path = path.clone();
    let refresher = tokio::spawn(async move {
        let lock = store::lock_session(&refresher_path)
            .await
            .expect("the refresher takes the lock first");
        held_tx.send(()).expect("signal that the lock is held");
        release_rx.await.expect("the release signal");
        store::write_session(
            &refresher_path,
            &store::test_session("https://old.example", 1),
        )
        .unwrap();
        drop(lock);
    });

    held_rx.await.expect("the refresher must acquire the lock");
    let login = tokio::spawn({
        let gateway_url = server.uri();
        async move { run_bounded(&gateway_url, true, Duration::from_secs(5)).await }
    });

    // Stage 1, causal and effectively unbounded: the login's network work is
    // done. Reached on both paths — `run_bounded` takes the session lock only
    // after the token poll returns — so a timeout here is a real failure.
    await_token_post(&server).await;
    // Stage 2, short and bounded: only the write is left. Without the lock the
    // login has written by now and this observes it; with the lock it cannot
    // have, and this necessarily times out — see the doc comment.
    await_stored_gateway(&path, &server.uri()).await;
    release_tx.send(()).expect("release the refresher");

    login
        .await
        .expect("login task")
        .expect("the login itself must succeed");
    refresher.await.expect("refresher task");

    let stored = store::read_session(&path)
        .unwrap()
        .expect("the login must have stored a session");
    assert_eq!(
        stored.gateway_url,
        server.uri(),
        "the login stored its session before the refresh's writeback, so the refresh put the old \
         gateway back over a session the command already reported as saved"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Block until the gateway has received the device-flow token POST, so a
/// caller can order itself against the login's last network call instead of
/// budgeting for it.
///
/// Stage 1 of the two-stage wait in the race test. The deadline is deliberately
/// far larger than the work: it is not a budget the login races, it is a bound
/// so a login that never posts fails with this message instead of hanging the
/// suite. Both a locked and an unlocked login reach the POST — the session lock
/// is taken only after the poll returns — so reaching it is not evidence about
/// the lock, and a timeout here is a genuine failure worth panicking on.
#[cfg(unix)]
async fn await_token_post(server: &MockServer) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let received = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .any(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path() == "/oauth/token"
            });
        if received {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the login never posted to the token endpoint");
}

/// Block until the session file on disk names `gateway_url`, so a caller can
/// order itself against the login's *write* rather than against the clock.
///
/// Stage 2, and it only ever covers the write, because `await_token_post` has
/// already absorbed the network round-trips. That is what lets the deadline be
/// this short without racing anything.
///
/// Reads through `store::read_session` rather than the raw bytes: the store
/// writes atomically, and going through the same reader means a torn or
/// half-written file can never be mistaken for a completed write.
///
/// Returns quietly when the deadline passes instead of panicking, because the
/// caller's correct path *expects* it to pass — a login blocked on the session
/// lock has not written and must not. The value it was waiting for is asserted
/// by the caller afterwards, so a genuinely missing write still fails there.
///
/// The deadline is the test's floor cost on a correct build. Keep it short.
#[cfg(unix)]
async fn await_stored_gateway(path: &std::path::Path, gateway_url: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(session)) = store::read_session(path) {
            if session.gateway_url == gateway_url {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Mount the three endpoints a whole `run_bounded` login walks — discovery,
/// the device-code POST, and the token poll answered by `token_response`.
///
/// Factored out rather than copied into each caller: the scaffolding is most
/// of the body of every end-to-end login test, and a copy per case is how the
/// device-flow fixture drifts between them.
async fn mount_device_flow(server: &MockServer, token_response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": format!("{}/oauth/device_authorization", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri())
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/device_authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "device-code-1",
            "user_code": "BCDF-GHJK",
            "verification_uri": format!("{}/device", server.uri()),
            "expires_in": 600,
            "interval": 1
        })))
        .mount(server)
        .await;
    // Approved on the first poll: the poll loop is not what these tests drive.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(token_response)
        .mount(server)
        .await;
}

/// A login must not report success for a token Claude Code cannot use.
///
/// The refresh path has always vetted the token it persists; login did not, and
/// the gap is not self-correcting: the stored expiry normally sits outside the
/// refresh buffer, so the next `shunt gateway token` takes the *cached* path,
/// fails the same validation, and keeps failing until the token expires. The
/// user is left with a session that reported success and can never be used.
#[tokio::test]
async fn a_login_token_claude_code_would_reject_fails_and_stores_no_session() {
    let server = MockServer::start().await;
    mount_device_flow(
        &server,
        // An embedded newline is the realistic case: Claude Code trims
        // `apiKeyHelper` stdout and then rejects the value outright.
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1\nextra",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 3600
        })),
    )
    .await;

    let _env = store::TEST_ENV_LOCK.lock().await;
    let dir = store::temp_dir("login-unsafe-token");
    let path = dir.join("session.json");
    let _session_file =
        crate::auth::shared::EnvVarGuard::set("SHUNT_GATEWAY_SESSION_FILE", path.as_os_str());

    let error = run_bounded(&server.uri(), true, Duration::from_secs(5))
        .await
        .expect_err("a token Claude Code would reject must not complete the login");
    let message = format!("{error:#}");
    assert!(
        message.contains("printable ASCII"),
        "the diagnostic must say what the contract is: {message}"
    );
    assert!(
        message.contains("no session was saved"),
        "the user must be told the login did not take effect: {message}"
    );
    // The failure mode worth pinning: a half-completed login that leaves an
    // unusable session behind is what the next command would then trip over.
    assert!(
        !path.exists(),
        "the login failed, so it must not have written a session to {}",
        path.display()
    );

    let _ = std::fs::remove_dir_all(dir);
}
