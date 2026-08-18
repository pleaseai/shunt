//! Tests for [`super`]: discovery, refresh, expiry, and token resolution.
//!
//! In a sibling file rather than inline, matching the convention already used
//! by `src/auth/slots.rs`. A pure move — no test was added, removed, or
//! rewritten here.

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
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant"})))
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
    let second = second
        .expect("the losing resolver must pick up the winner's token, not replay the spent one");
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
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant"})))
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
        let printed = sanitize_for_terminal(&format!("https://gateway.example/{hostile}device"));
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
