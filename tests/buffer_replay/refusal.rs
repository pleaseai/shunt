//! What a refused streaming weak chain relays besides its status (issue #655),
//! and the two-route gateway the committed-chain tests run on.

use reqwest::StatusCode;
use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

use super::harness::{
    anthropic_sse, escalation_router, header, judge_text, messages_mock, post, sse_reply, Tiers,
};
use super::judge_harness::{
    can_bind_loopback, env, start_gateway, CAPABLE_UPSTREAM_MODEL, EFFICIENT_UPSTREAM_MODEL,
    JUDGE_KEY_ENV,
};
use super::DECLINE;

/// An escalation gateway whose weak alias `chained-alias` maps two upstreams:
/// an HTTP Responses route on `down` that answers `down_reply`, then the
/// Anthropic route on `efficient`. That is the shape the live path sends
/// through the committed chain stream.
pub(crate) async fn chained_gateway_with(
    strong: &MockServer,
    efficient: &MockServer,
    judge: &MockServer,
    down: &MockServer,
    down_reply: ResponseTemplate,
) -> super::judge_harness::TestGateway {
    Mock::given(method("POST"))
        .respond_with(down_reply)
        .expect(1)
        .mount(down)
        .await;
    // Never called: the chain holds no Responses tier of the harness's own.
    let responses = MockServer::start().await;
    let tiers = Tiers::of(strong, efficient, &responses, judge.uri());
    let mut config =
        super::harness::unvalidated_gated_config(&tiers, &escalation_router("chained-alias"));
    let mut weak_responses = super::judge_harness::api_key("down", down.uri(), JUDGE_KEY_ENV);
    weak_responses.kind = Some(shunt::config::ProviderKind::Responses);
    // Ahead of `efficient`: the chain follows `[[upstreams]]` order.
    let at = config
        .upstreams
        .iter()
        .position(|upstream| upstream.name == "efficient")
        .expect("the harness has an efficient upstream");
    config.upstreams.insert(at, weak_responses);
    let mut chained =
        super::judge_harness::alias("chained-alias", "efficient", EFFICIENT_UPSTREAM_MODEL);
    chained
        .upstream_model
        .as_mut()
        .expect("an alias maps its upstream")
        .insert("down".to_string(), "upstream-down".to_string());
    config.models.push(chained);
    start_gateway(
        config
            .validate()
            .expect("the chained config is well formed"),
    )
    .await
}

fn rate_limited(retry_after: &str) -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("retry-after", retry_after)
        .set_body_json(json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "weak tier is saturated"},
        }))
}

/// A streaming weak chain that runs out on a `429` relays that `429` with the
/// upstream's own `retry-after`, as the ordered loop relays it — whichever
/// adapter's route supplied the failure the chain remembered: the Responses
/// route (ahead of an Anthropic `500`) or the Anthropic route (behind a
/// Responses `503`).
///
/// Non-vacuity: drop the header from the refusal `retain_stream` relays and
/// both cases panic on the missing `retry-after`; stop an adapter's
/// `chain_attempt` from carrying it and that adapter's case does.
#[tokio::test]
async fn a_refused_streaming_weak_chain_relays_the_upstream_retry_after() {
    if !can_bind_loopback() {
        return;
    }
    let _env = env().await;
    // (the Responses route's reply, the Anthropic route's, the expected value)
    let cases = [
        (rate_limited("7"), ResponseTemplate::new(500), "7"),
        (ResponseTemplate::new(503), rate_limited("11"), "11"),
    ];
    for (down_reply, efficient_reply, expected) in cases {
        let (strong, efficient, judge, down) = (
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
        );
        messages_mock(
            sse_reply(anthropic_sse(CAPABLE_UPSTREAM_MODEL, "STRONG")),
            0,
        )
        .mount(&strong)
        .await;
        messages_mock(efficient_reply, 1).mount(&efficient).await;
        messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
        let gateway = chained_gateway_with(&strong, &efficient, &judge, &down, down_reply).await;

        let response = post(&gateway, true).await;
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "{expected}"
        );
        assert_eq!(header(&response, "x-gateway-route-source"), "gated_error");
        let retry_after = response.headers().get("retry-after").unwrap_or_else(|| {
            panic!("no retry-after relayed in the case whose 429 says retry-after: {expected}")
        });
        assert_eq!(
            retry_after, expected,
            "the case whose 429 says retry-after: {expected}"
        );
    }
}
