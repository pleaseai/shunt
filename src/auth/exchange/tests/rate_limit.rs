use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    Extension,
};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::*;
use crate::auth::exchange::handler;
use crate::auth::inbound_jwt::tests::{claims, issuer_serving, jwks, token};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// A running gateway state whose only opted-in rule trusts a loopback issuer.
struct Live {
    shared: crate::reload::SharedState,
    state: crate::server::AppState,
    issuer: MockServer,
    env: String,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn config_for(issuer: &str, env: &str, max_failures: u32, exchange: bool) -> Config {
    let mut rule = entry(issuer, exchange);
    rule.audience = StringList::One("shunt-clients".to_string());
    rule.require = [(
        "repository_owner".to_string(),
        StringList::One("pleaseai".into()),
    )]
    .into();
    rule.jwks_url = Some(format!("{issuer}/jwks"));
    let mut config = config_with(env, vec![rule], &[SECRET_A]);
    if !exchange {
        config.server.auth.as_mut().unwrap().exchange = None;
    }
    config.server.rate_limits.exchange_failure.max = max_failures;
    config
}

async fn live(tag: &str, max_failures: u32, issuer: MockServer) -> Live {
    live_with(tag, issuer, |config| {
        config.server.rate_limits.exchange_failure.max = max_failures;
    })
    .await
}

async fn live_with(tag: &str, issuer: MockServer, tune: impl FnOnce(&mut Config)) -> Live {
    let lock = crate::config::test_env_lock();
    let env = format!("SHUNT_TEST_EXCHANGE_RL_{tag}_{}", std::process::id());
    std::env::remove_var(&env);
    let mut config = config_for(&issuer.uri(), &env, 10, true);
    tune(&mut config);
    let stores = crate::gateway::GatewayStores::new(&config.server.rate_limits, None);
    let shared: crate::reload::SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::reload::RuntimeState::from_config(config).unwrap(),
    ));
    let state = crate::server::AppState::from_shared(
        shared.clone(),
        reqwest::Client::new(),
        Arc::new(crate::accounts::AccountPool::new()),
        Arc::new(crate::upstream_status::StatusStore::new()),
        Arc::new(crate::admin::AdminStores::new()),
        Arc::new(stores),
        Arc::new(crate::routing::stage::StageRouterStore::new()),
        Arc::new(JwksCache::new()),
        true,
    );
    Live {
        shared,
        state,
        issuer,
        env,
        _lock: lock,
    }
}

async fn serving(tag: &str, max_failures: u32) -> Live {
    live(tag, max_failures, issuer_serving(jwks()).await).await
}

fn assertion(issuer: &str) -> String {
    assertion_for(issuer, "pleaseai/shunt", "shunt-clients")
}

fn assertion_for(issuer: &str, repository: &str, audience: &str) -> String {
    let mut claims = claims(issuer);
    claims["aud"] = json!(audience);
    claims["repository"] = json!(repository);
    claims["repository_owner"] = json!("pleaseai");
    token(&claims)
}

fn exchange_body(assertion: &str) -> Vec<u8> {
    json!({ "grant_type": GRANT, "assertion": assertion })
        .to_string()
        .into_bytes()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn error(&self) -> &str {
        self.body["error"].as_str().unwrap_or_default()
    }
}

async fn call(
    state: &crate::server::AppState,
    peer: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    call_via(state, peer, None, content_type, body).await
}

async fn call_via(
    state: &crate::server::AppState,
    peer: &str,
    forwarded_for: Option<&str>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).unwrap(),
        );
    }
    if let Some(forwarded_for) = forwarded_for {
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_str(forwarded_for).unwrap(),
        );
    }
    let peer: SocketAddr = format!("{peer}:4000").parse().unwrap();
    let response = handler::post(
        State(state.clone()),
        Some(Extension(ConnectInfo(peer))),
        headers,
        Body::from(body),
    )
    .await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// One failing request (wrong content type) claiming `forwarded_for`.
async fn fail_as(state: &crate::server::AppState, peer: &str, forwarded_for: &str) -> Reply {
    call_via(
        state,
        peer,
        Some(forwarded_for),
        Some("text/plain"),
        Vec::new(),
    )
    .await
}

async fn post_json(state: &crate::server::AppState, peer: &str, body: Vec<u8>) -> Reply {
    call(state, peer, Some("application/json"), body).await
}

async fn jwks_fetches(issuer: &MockServer) -> usize {
    issuer.received_requests().await.unwrap_or_default().len()
}

fn assert_slow_down(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.error(), "slow_down");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let seconds: u64 = reply.headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1..=600).contains(&seconds),
        "Retry-After {seconds} outside the window"
    );
}

#[tokio::test]
async fn a_blocked_address_is_refused_before_any_verification() {
    let h = serving("BLOCK", 2).await;
    let valid = assertion(&h.issuer.uri());
    let bad_grant = json!({ "grant_type": "password", "assertion": valid }).to_string();

    for _ in 0..2 {
        let reply = post_json(&h.state, "10.0.0.1", bad_grant.clone().into_bytes()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        jwks_fetches(&h.issuer).await,
        0,
        "failures before the block never verified"
    );

    // The very next request carries a valid assertion and is still refused,
    // and no key set is fetched for it.
    let blocked = post_json(&h.state, "10.0.0.1", exchange_body(&valid)).await;
    assert_slow_down(&blocked);
    assert_eq!(
        jwks_fetches(&h.issuer).await,
        0,
        "a blocked request must not verify"
    );

    // The same assertion from another address succeeds, so the refusal above
    // was the limiter and not a bad assertion.
    let other = post_json(&h.state, "10.0.0.2", exchange_body(&valid)).await;
    assert_eq!(other.status, StatusCode::OK);
    assert_eq!(jwks_fetches(&h.issuer).await, 1);
}

#[tokio::test]
async fn every_kind_of_failed_request_counts_against_the_address() {
    let h = serving("KINDS", 1).await;
    let valid = assertion(&h.issuer.uri());
    let mut wrong_aud = claims(&h.issuer.uri());
    wrong_aud["aud"] = json!("someone-else");
    let unverifiable = token(&wrong_aud);

    let kinds: Vec<(&str, Option<&str>, Vec<u8>, StatusCode)> = vec![
        (
            "wrong content type",
            Some("text/plain"),
            exchange_body(&valid),
            StatusCode::BAD_REQUEST,
        ),
        (
            "missing content type",
            None,
            exchange_body(&valid),
            StatusCode::BAD_REQUEST,
        ),
        (
            "malformed body",
            Some("application/json"),
            b"{not json".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "oversized body",
            Some("application/json"),
            vec![b'a'; 64 * 1024 + 1],
            StatusCode::BAD_REQUEST,
        ),
        (
            "unsupported grant type",
            Some("application/json"),
            json!({ "grant_type": "password", "assertion": valid })
                .to_string()
                .into_bytes(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unverifiable assertion",
            Some("application/json"),
            exchange_body(&unverifiable),
            StatusCode::UNAUTHORIZED,
        ),
    ];
    for (index, (name, content_type, body, expected)) in kinds.into_iter().enumerate() {
        let peer = format!("10.1.0.{}", index + 1);
        let first = call(&h.state, &peer, content_type, body).await;
        assert_eq!(
            first.status, expected,
            "{name}: the original response is kept"
        );
        let second = post_json(&h.state, &peer, exchange_body(&valid)).await;
        assert_slow_down(&second);
    }
}

#[tokio::test]
async fn a_success_is_never_counted() {
    let h = serving("SUCCESS", 1).await;
    let valid = assertion(&h.issuer.uri());
    for _ in 0..4 {
        let reply = post_json(&h.state, "10.2.0.1", exchange_body(&valid)).await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    // Had the successes counted, this first failure would already be a 429.
    let failed = post_json(&h.state, "10.2.0.1", b"{}".to_vec()).await;
    assert_eq!(failed.status, StatusCode::BAD_REQUEST);
    let blocked = post_json(&h.state, "10.2.0.1", exchange_body(&valid)).await;
    assert_slow_down(&blocked);
}

#[tokio::test]
async fn an_unavailable_key_set_is_never_counted() {
    let issuer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&issuer)
        .await;
    let h = live("DOWN", 1, issuer).await;
    let valid = assertion(&h.issuer.uri());
    for _ in 0..4 {
        let reply = post_json(&h.state, "10.3.0.1", exchange_body(&valid)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.error(), "temporarily_unavailable");
    }
    // Had the 503s counted, this first real failure would already be a 429.
    let failed = post_json(&h.state, "10.3.0.1", b"{}".to_vec()).await;
    assert_eq!(failed.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_removed_table_answers_404_even_for_a_blocked_address() {
    let h = serving("REMOVED", 1).await;
    let valid = assertion(&h.issuer.uri());
    post_json(&h.state, "10.4.0.1", b"{}".to_vec()).await;
    assert_slow_down(&post_json(&h.state, "10.4.0.1", exchange_body(&valid)).await);

    let config = config_for(&h.issuer.uri(), &h.env, 1, false);
    h.shared.store(Arc::new(
        crate::reload::RuntimeState::from_config(config).unwrap(),
    ));
    let gone = post_json(&h.state, "10.4.0.1", exchange_body(&valid)).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_full_table_refuses_a_new_failing_address_with_slow_down() {
    let h = serving("FULL", 10).await;
    for index in 0..4096u32 {
        let peer = std::net::Ipv4Addr::from(0x0a10_0000 + index).to_string();
        let reply = call(&h.state, &peer, Some("text/plain"), Vec::new()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "address {index} fits"
        );
    }
    let overflow = call(&h.state, "10.99.0.1", Some("text/plain"), Vec::new()).await;
    assert_slow_down(&overflow);
    // An address already tracked still gets the ordinary answer.
    let tracked = call(&h.state, "10.16.0.0", Some("text/plain"), Vec::new()).await;
    assert_eq!(tracked.status, StatusCode::BAD_REQUEST);
}

fn limit_identity(max: u32, window_seconds: u64) -> impl FnOnce(&mut Config) {
    move |config| {
        config.server.rate_limits.exchange.max = max;
        config.server.rate_limits.exchange.window_seconds = window_seconds;
    }
}

#[tokio::test]
async fn an_identity_at_its_maximum_gets_429_and_no_token_and_no_address_failure() {
    let h = live_with("IDMAX", issuer_serving(jwks()).await, |config| {
        limit_identity(2, 600)(config);
        config.server.rate_limits.exchange_failure.max = 1;
    })
    .await;
    let valid = assertion(&h.issuer.uri());
    for _ in 0..2 {
        let reply = post_json(&h.state, "10.5.0.1", exchange_body(&valid)).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body["access_token"].is_string());
    }
    let refused = post_json(&h.state, "10.5.0.1", exchange_body(&valid)).await;
    assert_slow_down(&refused);
    assert!(
        refused.body.get("access_token").is_none(),
        "no token is minted"
    );
    // Still refused within the window, from any address: the quota is the identity's.
    assert_slow_down(&post_json(&h.state, "10.5.0.2", exchange_body(&valid)).await);
    // The refusals were not address failures: with a failure limit of 1,
    // the first real failure is an ordinary 400, not a 429.
    let failed = post_json(&h.state, "10.5.0.1", b"{}".to_vec()).await;
    assert_eq!(failed.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_identity_is_exchanged_again_once_its_window_elapses() {
    // Chosen: a real 1-second window and a wait just past it. The limiters
    // use real `Instant`s, and the reset itself is also covered at limiter
    // level by `admit_blocks_over_max_until_the_window_elapses`.
    let h = live_with("IDWIN", issuer_serving(jwks()).await, limit_identity(1, 1)).await;
    let valid = assertion(&h.issuer.uri());
    assert_eq!(
        post_json(&h.state, "10.6.0.1", exchange_body(&valid))
            .await
            .status,
        StatusCode::OK
    );
    assert_slow_down(&post_json(&h.state, "10.6.0.1", exchange_body(&valid)).await);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        post_json(&h.state, "10.6.0.1", exchange_body(&valid))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn identities_and_rules_are_counted_separately() {
    let h = live_with("IDSEP", issuer_serving(jwks()).await, |config| {
        limit_identity(1, 600)(config);
        let auth = config.server.auth.as_mut().unwrap();
        let mut second = auth.jwt[0].clone();
        second.audience = StringList::One("other-aud".to_string());
        auth.jwt.push(second);
    })
    .await;
    let issuer = h.issuer.uri();
    let first = assertion_for(&issuer, "pleaseai/shunt", "shunt-clients");
    let other_identity = assertion_for(&issuer, "pleaseai/other", "shunt-clients");
    let other_rule = assertion_for(&issuer, "pleaseai/shunt", "other-aud");

    // One address, three distinct (rule, identity) pairs: each has its own quota.
    for assertion in [&first, &other_identity, &other_rule] {
        let reply = post_json(&h.state, "10.7.0.1", exchange_body(assertion)).await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    for assertion in [&first, &other_identity, &other_rule] {
        assert_slow_down(&post_json(&h.state, "10.7.0.1", exchange_body(assertion)).await);
    }
}

#[tokio::test]
async fn the_two_limits_answer_identically_apart_from_retry_after() {
    let h = live_with("SAME", issuer_serving(jwks()).await, |config| {
        limit_identity(1, 600)(config);
        config.server.rate_limits.exchange_failure.max = 1;
    })
    .await;
    let valid = assertion(&h.issuer.uri());

    // Identity limit.
    post_json(&h.state, "10.8.0.1", exchange_body(&valid)).await;
    let identity_limited = post_json(&h.state, "10.8.0.1", exchange_body(&valid)).await;
    // Failure limit, from an address that never exchanged.
    post_json(&h.state, "10.8.0.2", b"{}".to_vec()).await;
    let failure_limited = post_json(&h.state, "10.8.0.2", exchange_body(&valid)).await;

    assert_slow_down(&identity_limited);
    assert_slow_down(&failure_limited);
    let without_retry_after = |reply: &Reply| {
        let mut headers = reply.headers.clone();
        headers.remove(header::RETRY_AFTER);
        headers
    };
    assert_eq!(identity_limited.status, failure_limited.status);
    assert_eq!(
        without_retry_after(&identity_limited),
        without_retry_after(&failure_limited)
    );
    assert_eq!(identity_limited.body, failure_limited.body);
}

fn trusting_forwarded_for(trust: bool) -> impl FnOnce(&mut Config) {
    move |config| {
        config.server.access_control.trust_forwarded_for = trust;
        config.server.rate_limits.exchange_failure.max = 1;
    }
}

#[tokio::test]
async fn a_trusted_forwarded_address_is_counted_on_its_own() {
    let h = live_with(
        "XFF_ON",
        issuer_serving(jwks()).await,
        trusting_forwarded_for(true),
    )
    .await;
    assert_eq!(
        fail_as(&h.state, "10.9.0.1", "203.0.113.7").await.status,
        StatusCode::BAD_REQUEST
    );
    // The same forwarded address is now blocked, whichever peer relays it.
    assert_slow_down(&fail_as(&h.state, "10.9.0.1", "203.0.113.7").await);
    assert_slow_down(&fail_as(&h.state, "10.9.0.2", "203.0.113.7").await);
    // Another forwarded address behind the same peer is not blocked.
    assert_eq!(
        fail_as(&h.state, "10.9.0.1", "203.0.113.8").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_untrusted_forwarded_header_is_ignored_and_the_peer_is_counted() {
    let h = live_with(
        "XFF_OFF",
        issuer_serving(jwks()).await,
        trusting_forwarded_for(false),
    )
    .await;
    assert_eq!(
        fail_as(&h.state, "10.9.1.1", "203.0.113.7").await.status,
        StatusCode::BAD_REQUEST
    );
    // A different claimed address does not escape the peer's count.
    assert_slow_down(&fail_as(&h.state, "10.9.1.1", "203.0.113.8").await);
    // The claimed address was never recorded: another peer sending the first one is fresh.
    assert_eq!(
        fail_as(&h.state, "10.9.1.2", "203.0.113.7").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_oversized_forwarded_value_is_stored_as_a_bounded_key() {
    let h = live_with(
        "XFF_BIG",
        issuer_serving(jwks()).await,
        trusting_forwarded_for(true),
    )
    .await;
    let long_a = format!("{}a", "9".repeat(8 * 1024));
    let long_b = format!("{}b", "9".repeat(8 * 1024));
    assert_eq!(
        fail_as(&h.state, "10.9.2.1", &long_a).await.status,
        StatusCode::BAD_REQUEST
    );
    // Distinct long values stay distinct keys; the repeated one is blocked.
    assert_eq!(
        fail_as(&h.state, "10.9.2.1", &long_b).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_slow_down(&fail_as(&h.state, "10.9.2.1", &long_a).await);

    let keys = h.state.gateway_stores.exchange_failure_rate.tracked_keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(
        keys.iter().all(|key| key.len() <= 64),
        "stored keys must be bounded, got lengths {:?}",
        keys.iter().map(String::len).collect::<Vec<_>>()
    );
}
