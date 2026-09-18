#[path = "weekly_fallback/codex_provenance.rs"]
mod codex_provenance;
mod common;
#[path = "weekly_fallback/failures.rs"]
mod failures;
#[path = "weekly_fallback/protocol.rs"]
mod protocol;
#[path = "weekly_fallback/support.rs"]
mod support;

use serde_json::json;
use shunt::accounts::{UsageSnapshot, UsageWindow};
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

use support::*;

#[tokio::test]
async fn every_pair_keeps_a_healthy_primary_and_switches_on_complete_exhaustion() {
    for (index, (claude, codex)) in PAIRS.iter().enumerate() {
        for from_claude in [true, false] {
            let fixture = Fixture::new(index, from_claude, 2).await;
            fixture.success().await;
            let gateway = fixture.gateway().await;
            let (primary, backend, destination, alternate) = if from_claude {
                ("anthropic", *claude, "codex", *codex)
            } else {
                ("codex", *codex, "anthropic", *claude)
            };
            let response = gateway.post().await;
            assert_eq!(response.status(), 200);
            assert_headers(&response, primary, backend);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["content"][0]["text"],
                "hello"
            );
            assert!(fixture.hits(destination).await.is_empty());

            fixture.quota(&gateway, primary, 0, 1.0);
            let response = gateway.post().await;
            assert_eq!(response.status(), 200);
            assert_headers(&response, primary, backend);
            assert_eq!(
                response.headers()["x-shunt-account"],
                format!("{primary}-1")
            );
            response.bytes().await.unwrap();
            assert!(fixture.hits(destination).await.is_empty());

            fixture.quota(&gateway, primary, 1, 1.0);
            let before = fixture.hits(primary).await.len();
            let response = gateway.post().await;
            assert_eq!(response.status(), 200);
            assert_headers(&response, destination, alternate);
            response.bytes().await.unwrap();
            assert_eq!(fixture.hits(primary).await.len(), before);
            assert_eq!(
                upstream_body(&fixture.hits(destination).await[0])["model"],
                alternate
            );

            fixture.quota(&gateway, primary, 0, 0.1);
            let response = gateway.post().await;
            assert_eq!(response.status(), 200);
            assert_headers(&response, primary, backend);
            response.bytes().await.unwrap();
            assert_eq!(fixture.hits(destination).await.len(), 1);
        }
    }
}

#[tokio::test]
async fn failure_headers_prove_exhaustion_within_the_same_request() {
    for from_claude in [true, false] {
        let fixture = Fixture::new(2, from_claude, 2).await;
        let gateway = fixture.gateway().await;
        let (primary, destination, backend, server, request_path) = if from_claude {
            (
                "anthropic",
                "codex",
                PAIRS[2].1,
                &fixture.claude,
                "/v1/messages",
            )
        } else {
            (
                "codex",
                "anthropic",
                PAIRS[2].0,
                &fixture.codex,
                "/codex/responses",
            )
        };
        Mock::given(method("POST"))
            .and(path(request_path))
            .respond_with(quota_failure(primary, 429))
            .mount(server)
            .await;
        if from_claude {
            Mock::given(method("POST"))
                .respond_with(codex_success())
                .mount(&fixture.codex)
                .await;
        } else {
            Mock::given(method("POST"))
                .respond_with(claude_success())
                .mount(&fixture.claude)
                .await;
        }
        let response = gateway.post().await;
        assert_eq!(response.status(), 200);
        assert_headers(&response, destination, backend);
        response.bytes().await.unwrap();
        assert_eq!(fixture.hits(primary).await.len(), 2);
        assert_eq!(fixture.hits(destination).await.len(), 1);
        assert!(gateway
            .state
            .accounts
            .strict_weekly_exhausted(primary, &fixture.config.providers[primary].accounts));
    }
}

#[tokio::test]
async fn exhausted_pools_return_anthropic_429_without_dispatch() {
    for from_claude in [true, false] {
        let fixture = Fixture::new(0, from_claude, 2).await;
        let gateway = fixture.gateway().await;
        fixture.exhaust(&gateway, "anthropic");
        fixture.exhaust(&gateway, "codex");
        let response = gateway.post().await;
        assert_eq!(response.status(), 429);
        assert!(response.headers().get("retry-after").is_none());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert!(fixture.hits("anthropic").await.is_empty());
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn destination_exhaustion_during_dispatch_returns_429_without_a_return_switch() {
    let fixture = Fixture::new(2, true, 1).await;
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "anthropic");
    Mock::given(method("POST"))
        .respond_with(quota_failure("codex", 429))
        .mount(&fixture.codex)
        .await;
    let response = gateway.post().await;
    assert_eq!(response.status(), 429);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["type"],
        "rate_limit_error"
    );
    assert!(fixture.hits("anthropic").await.is_empty());
    assert_eq!(fixture.hits("codex").await.len(), 1);
}

#[tokio::test]
async fn incomplete_and_unrelated_quota_signals_do_not_switch_providers() {
    for scenario in [
        "five_hour",
        "fable_only",
        "resetless",
        "past_reset",
        "soft",
        "cooldown",
        "malformed",
        "contradictory",
        "aggregate",
    ] {
        let fixture = Fixture::new(2, true, 1).await;
        fixture.success().await;
        let gateway = fixture.gateway().await;
        let pool = &gateway.state.accounts;
        let account = fixture.account("anthropic", 0);
        let window = UsageWindow {
            utilization: 1.0,
            resets_at: Some(reset_time()),
        };
        let mut usage = UsageSnapshot::default();
        match scenario {
            "five_hour" => usage.five_hour = Some(window),
            "fable_only" => usage.seven_day_oi = Some(window),
            "resetless" => {
                usage.seven_day = Some(UsageWindow {
                    resets_at: None,
                    ..window
                })
            }
            "past_reset" => {
                usage.seven_day = Some(UsageWindow {
                    resets_at: Some(1),
                    ..window
                })
            }
            "soft" => {
                usage.seven_day = Some(UsageWindow {
                    utilization: 0.99,
                    ..window
                })
            }
            "cooldown" => pool.cooldown(
                "anthropic",
                account,
                std::time::Duration::from_secs(60),
                "quota",
            ),
            _ => {
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    "anthropic-ratelimit-unified-7d-reset",
                    reset_time().to_string().parse().unwrap(),
                );
                if scenario == "aggregate" {
                    headers.insert(
                        "anthropic-ratelimit-unified-status",
                        "rejected".parse().unwrap(),
                    );
                } else {
                    headers.insert(
                        "anthropic-ratelimit-unified-7d-utilization",
                        if scenario == "malformed" {
                            "NaN"
                        } else {
                            "0.1"
                        }
                        .parse()
                        .unwrap(),
                    );
                    headers.insert(
                        "anthropic-ratelimit-unified-7d-status",
                        "rejected".parse().unwrap(),
                    );
                }
                pool.note_quota("anthropic", account, &headers);
            }
        }
        pool.note_usage("anthropic", account, &usage);
        let response = gateway.post().await;
        assert_eq!(response.status(), 200, "scenario: {scenario}");
        assert_headers(&response, "anthropic", PAIRS[2].0);
        response.bytes().await.unwrap();
        assert!(
            fixture.hits("codex").await.is_empty(),
            "scenario: {scenario}"
        );
    }
}

#[tokio::test]
async fn count_tokens_and_unmapped_routes_preserve_the_primary() {
    let mut fixture = Fixture::new(0, true, 1).await;
    fixture.config.routes.push(shunt::config::RouteConfig {
        model: "unmapped".into(),
        upstream_model: Some("unmapped-backend".into()),
        ..route("anthropic", "unused")
    });
    fixture.success().await;
    Mock::given(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .mount(&fixture.claude)
        .await;
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "anthropic");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens", gateway.base_url))
        .header("x-shunt-token", CLIENT_TOKEN)
        .json(&request_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["input_tokens"],
        7
    );
    let mut body = request_body(false);
    body["model"] = json!("unmapped");
    let response = gateway.post_body(body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        "unmapped-backend"
    );
    response.bytes().await.unwrap();
    assert!(fixture.hits("codex").await.is_empty());
}
