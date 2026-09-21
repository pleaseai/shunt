//! End-to-end coverage for the driven lane: a `[models.router]` entry carrying
//! a `[models.router.classifier]` (ADR-0005 §3, issue #594).
//!
//! `tests/stage_router.rs` covers the pure lane, and the unit tests under
//! `src/routing/` pin the gate, the bounds, and the budget in isolation. These
//! pin what a real request experiences once a judge is in the loop: whether the
//! judge is called at all, what it is sent, which credential it runs on, and
//! what a misbehaving one costs.
//!
//! Non-vacuity: drop the `judges()` chain from `routing::envelope` and
//! `an_unauthenticated_turn_is_refused_before_the_judge_is_called` goes red on
//! a `200`; move the consult before `check_inbound_auth` and it goes red on the
//! judge's call count instead. Stop honouring `max_judge_calls` and
//! `the_call_budget_is_spent_once_per_session` goes red. Wrap the upstream in
//! reqwest's `.timeout()` rather than `tokio::time::timeout` and both stall
//! tests hang past their own assertion window.
mod common;
mod judge_harness;

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::config::{
    AuthMode, PassthroughSubagentsConfig, RouterConfig, SubagentsConfig, UpstreamAuth,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use judge_harness::{
    can_bind_loopback, client, decisive_messages, driven_config, driven_config_with, env,
    judge_mock, post, post_to, quiet_messages, stall_mock, start_gateway, tier_mock,
    undecided_messages, upstream_with, verdict, ModelIs, Stall, CAPABLE_UPSTREAM_MODEL,
    CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, JUDGE_KEY, JUDGE_UPSTREAM_MODEL, ROUTER_ID, SESSION,
};

/// Clause 1: the whole point of the lane. A turn the signals leave undecided
/// consults the judge, and the verdict — not the picker's default — decides.
#[tokio::test]
async fn an_undecided_turn_is_decided_by_the_judge() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    judge_mock(0.1, 1).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    let response = post(&gateway, undecided_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-route-source"],
        "llm-classifier",
        "the verdict, not the picker default, is what decided this turn"
    );
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "capable-alias"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["model"], ROUTER_ID,
        "the client is told the id it asked for"
    );

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// Clause 2: a turn the scorer decides on its own is not paid for twice.
#[tokio::test]
async fn a_decisive_turn_never_reaches_the_judge() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    judge_mock(0.9, 0).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    let response = post(&gateway, decisive_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-route-source"], "override");
    capable.verify().await;
    judge.verify().await;
}

/// Clause 3, first half: the envelope's whole reason for existing. A caller
/// with no credential is refused *before* the judge is consulted, so a rejected
/// request spends neither the gateway's judge credential nor the judge pool's
/// quota.
#[tokio::test]
async fn an_unauthenticated_turn_is_refused_before_the_judge_is_called() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_mock(0.1, 0).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    for token in [None, Some("wrong-token")] {
        let refused = post_to(&gateway, "/v1/messages", undecided_messages(), token).await;
        assert_eq!(
            refused.status(),
            StatusCode::UNAUTHORIZED,
            "token {token:?} must not be admitted"
        );
    }

    judge.verify().await;
}

/// Clause 3, third case: the overlay and the envelope on one entry.
///
/// `[models.subagents]` resolves a delegated turn ahead of the stage router
/// (`routing::resolve_chain`), so that turn consults no judge and spends none
/// of the gateway's judge credential. Selecting the envelope from the entry's
/// *static* config ignored that: a delegated child whose overlay target is
/// passthrough end to end was refused for an injecting judge its request could
/// never reach. Non-vacuity: gate the envelope on `driven` alone, without the
/// consultation, and this goes red with a `401`.
#[tokio::test]
async fn a_delegated_turn_is_gated_by_the_chain_the_overlay_resolved() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    // The judge must not be called at all: the overlay answered this turn.
    judge_mock(0.1, 0).mount(&judge).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    let config = driven_config_with(&capable, &efficient, judge.uri(), |config| {
        // `efficient-alias` is the passthrough upstream, so the overlay's
        // chain injects nothing and needs no inbound token.
        config.models[0].subagents =
            Some(SubagentsConfig::Passthrough(PassthroughSubagentsConfig {
                target: "efficient-alias".to_string(),
                by_type: Default::default(),
            }));
    });
    let gateway = start_gateway(config).await;

    let response = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        // A delegated turn: the class is authoritative (ADR-0005 §11).
        .header("x-claude-code-request-class", "subagent")
        .header("x-claude-code-agent-id", "a7a11c2e22e29e67a")
        .body(
            json!({"model": ROUTER_ID, "max_tokens": 16, "messages": undecided_messages()})
                .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a turn the overlay diverted is gated by the chain it resolved, \
         not by a judge it never reaches"
    );
    judge.verify().await;
    efficient.verify().await;
}

/// Clause 3, second half: the same rule when the judge is reached only by
/// fall-through. Both answer tiers are passthrough and the classifier names no
/// `[[models]]` entry at all, so the only credential-injecting route in the
/// envelope is the one `server.default_provider` resolves the judge to. Resolve
/// the judge's chain any way that skips the ladder and this goes green on a
/// `200`.
#[tokio::test]
async fn a_judge_reached_only_by_fall_through_still_gates_the_request() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_mock(0.1, 0).mount(&judge).await;
    let config = driven_config_with(&capable, &efficient, judge.uri(), |config| {
        // Both answer tiers passthrough, so nothing but the judge can gate.
        config.upstreams[0] = upstream_with(
            "capable",
            config.upstreams[0].base_url.clone().unwrap(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        );
        config.server.default_provider = "judge".to_string();
        match config.models[0].router.as_mut().unwrap() {
            RouterConfig::StageRouter(stage) => {
                stage.classifier.as_mut().unwrap().target = "no-such-entry".to_string();
            }
            other => panic!("the fixture is a stage router, got {other:?}"),
        }
    });
    let gateway = start_gateway(config).await;

    let refused = post_to(&gateway, "/v1/messages", undecided_messages(), None).await;

    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    judge.verify().await;
}

/// Clause 4: the managed-model policy is the second gate, and it runs on the
/// same side of the drive as the first. A model the operator's policy forbids
/// is refused with the gateway's own error shape and costs no judge call.
#[tokio::test]
async fn a_policy_denied_model_is_refused_before_the_judge_is_called() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = env().await;
    let secret_env = format!("SHUNT_TEST_JUDGE_GATEWAY_SECRET_{}", std::process::id());
    let users_env = format!("SHUNT_TEST_JUDGE_GATEWAY_USERS_{}", std::process::id());
    vars.set(&secret_env, "0123456789abcdef0123456789abcdef");
    vars.set(&users_env, "dev@example.com:password");

    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_mock(0.1, 0).mount(&judge).await;
    let config = driven_config_with(&capable, &efficient, judge.uri(), |config| {
        config.server.gateway = Some(shunt::config::GatewayConfig {
            public_url: "https://gateway.example".to_string(),
            jwt_secret_env: Some(secret_env.clone()),
            users_env: users_env.clone(),
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: Some(vec![shunt::config::GatewayPolicyConfig {
                matcher: None,
                // The router id is deliberately absent from the allow list.
                cli: toml::toml! { availableModels = ["some-other-model"] }.into(),
            }]),
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
        });
    });
    let gateway = start_gateway(config).await;

    let bearer = shunt::gateway::jwt::mint(
        &shunt::gateway::approval::Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        "https://gateway.example",
        b"0123456789abcdef0123456789abcdef",
        3600,
    );
    let refused = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .header("authorization", format!("Bearer {bearer}"))
        .body(
            json!({"model": ROUTER_ID, "max_tokens": 16, "messages": undecided_messages()})
                .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    judge.verify().await;
}

/// Clause 5: `count_tokens` never enters the driven lane (ADR-0005 §3). The
/// probe is answered by the passthrough answer target's first route, with no
/// token and no judge call — for a quiet history and for the undecided one that
/// a real turn *would* consult on. Both run on an unpinned session, so neither
/// is answered from a pin that had already decided.
#[tokio::test]
async fn a_count_tokens_probe_makes_no_judge_call() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    judge_mock(0.1, 0).mount(&judge).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(2)
        .mount(&efficient)
        .await;
    // Neither tier may serve a turn: a probe is not one.
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    for messages in [quiet_messages(), undecided_messages()] {
        // No token: a probe is gated against the first route only, which is the
        // passthrough efficient tier.
        let probe = post_to(&gateway, "/v1/messages/count_tokens", messages, None).await;
        assert_eq!(probe.status(), StatusCode::OK);
        let body: Value = probe.json().await.unwrap();
        assert_eq!(body["input_tokens"], 7);
    }

    judge.verify().await;
    efficient.verify().await;
    capable.verify().await;
}

/// Clause 6: what the judge is sent. The caller's headers travel — the judge
/// request is made on their turn — but every inbound credential slot is gone,
/// and the credential slot that *is* set holds the judge provider's own key and
/// nothing else. Strip by value rather than by name and the caller's
/// `authorization` survives into a call made to an origin they never addressed.
#[tokio::test]
async fn the_judge_call_carries_no_inbound_credential() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    judge_mock(0.1, 1).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    let response = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .header("x-shunt-token", CLIENT_TOKEN)
        .header("authorization", "Bearer caller-upstream")
        .header("x-api-key", "caller-key")
        .header("cookie", "shunt_admin_session=x")
        .header("anthropic-beta", "context-1m-2025-08-07")
        .body(
            json!({"model": ROUTER_ID, "max_tokens": 16, "messages": undecided_messages()})
                .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let received = judge
        .received_requests()
        .await
        .expect("the judge was called");
    let [request] = received.as_slice() else {
        panic!("expected exactly one judge call, got {}", received.len());
    };
    for absent in ["x-shunt-token", "cookie", "anthropic-beta", "authorization"] {
        assert!(
            !request.headers.contains_key(absent),
            "{absent} must not reach the judge"
        );
    }
    assert_eq!(
        request.headers["x-api-key"], JUDGE_KEY,
        "the slot holds the judge provider's injected key, not the caller's"
    );
}

/// Clause 7: the judge runs on its own provider. The call lands on the judge's
/// base URL with the judge's key, and neither answer tier sees it — a judge
/// dispatched through the answer chain would spend the wrong pool's quota and
/// read the caller's transcript at the wrong origin.
#[tokio::test]
async fn the_judge_runs_on_its_own_provider() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    // The answer call is the *only* thing the capable tier may serve: a judge
    // request carries the judge's upstream model, which this matcher rejects.
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    Mock::given(method("POST"))
        .and(ModelIs(JUDGE_UPSTREAM_MODEL))
        .respond_with(verdict(0.1))
        .expect(0)
        .mount(&capable)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&efficient)
        .await;
    judge_mock(0.1, 1).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    assert_eq!(
        post(&gateway, undecided_messages()).await.status(),
        StatusCode::OK
    );

    let received = judge
        .received_requests()
        .await
        .expect("the judge was called");
    let [request] = received.as_slice() else {
        panic!("expected exactly one judge call, got {}", received.len());
    };
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        body["model"], JUDGE_UPSTREAM_MODEL,
        "the judge's own `[models.upstream_model]` mapping is what travels"
    );
    assert_eq!(request.headers["x-api-key"], JUDGE_KEY);

    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

/// Clause 10: `max_judge_calls` is a per-session ceiling on calls *made*. The
/// first verdict is the picker's own default, so the session's pin does not
/// move and the next undecided turn asks again — and is refused by the budget,
/// falling open rather than consulting a second time.
#[tokio::test]
async fn the_call_budget_is_spent_once_per_session() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 2)
        .mount(&efficient)
        .await;
    // `max_judge_calls = 1`, so a second consultation would show up here.
    judge_mock(0.9, 1).mount(&judge).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;

    let first = post(&gateway, undecided_messages()).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.headers()["x-gateway-route-source"],
        "llm-classifier",
        "the first turn is the one that spends the budget"
    );

    let second = post(&gateway, undecided_messages()).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.headers()["x-gateway-route-source"],
        "fall_open",
        "the budget is spent, so the picker's default stands"
    );
    assert_eq!(
        second.headers()["x-gateway-routed-model"],
        "efficient-alias"
    );

    judge.verify().await;
    efficient.verify().await;
}

/// Clause 8: `200`, then silence. Headers are committed, so a `.send()`
/// deadline has already been satisfied when the hang begins.
#[tokio::test]
async fn a_judge_that_stalls_after_its_headers_fails_open() {
    a_stalled_judge_fails_open_inside_its_deadline(Stall::AfterHeaders).await;
}

/// Clause 9: the endless keep-alive. Chunks keep arriving, so a deadline reset
/// by any byte would never fire.
#[tokio::test]
async fn a_judge_that_only_sends_pings_fails_open() {
    a_stalled_judge_fails_open_inside_its_deadline(Stall::EndlessPing).await;
}

/// A judge that commits headers and then stops must fail open inside the
/// configured deadline, be served by the picker's default, and have its
/// upstream request actually **cancelled** — a deadline that only stopped
/// waiting would leave the connection held for as long as the mock cared to
/// hold it.
async fn a_stalled_judge_fails_open_inside_its_deadline(stall: Stall) {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    let (judge_url, closed) = stall_mock(stall).await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge_url)).await;

    // `judge_timeout_ms` is 500, so three seconds is a bound this can only meet
    // by failing open rather than by waiting the mock out.
    let response =
        tokio::time::timeout(Duration::from_secs(3), post(&gateway, undecided_messages()))
            .await
            .expect("the turn must not wait for a stalled judge");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-route-source"],
        "fall_open",
        "a judge that never answered decides nothing"
    );
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "efficient-alias"
    );

    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .expect("the elapsed deadline must close the judge connection")
        .expect("the mock reports the close");

    capable.verify().await;
    efficient.verify().await;
}
