#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

mod common;
#[path = "buffer_replay/harness.rs"]
#[allow(dead_code)] // This binary uses the escalation fixtures, not the advisor fixtures.
mod gated_harness;
mod judge_harness;
mod response_coding_support;

use judge_harness::*;
use response_coding_support::{compressed, decoded};

#[tokio::test]
async fn internal_judge_reads_compressed_json_before_client_encoding() {
    let _env = env().await;
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    let judge = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 0)
        .mount(&efficient)
        .await;
    let verdict = json!({"type":"message", "role":"assistant", "model":JUDGE_UPSTREAM_MODEL,
        "content":[{"type":"text", "text":json!({"crux":"bounded task", "primary_rule":"SUP-1", "capability_boundary":"supported", "p_solve":0.1}).to_string()}],
        "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1}});
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "gzip")
                .set_body_raw(
                    compressed("gzip", verdict.to_string().as_bytes()),
                    "application/json",
                ),
        )
        .expect(1)
        .mount(&judge)
        .await;
    let gateway = start_gateway(driven_config(&capable, &efficient, judge.uri())).await;
    let response = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("x-shunt-token", CLIENT_TOKEN)
        .header("x-claude-code-session-id", SESSION)
        .header("accept-encoding", "gzip")
        .json(&json!({"model":ROUTER_ID,"max_tokens":16,"messages":undecided_messages()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-gateway-route-source"],
        "llm-classifier"
    );
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "capable-alias"
    );
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["model"],
        ROUTER_ID
    );
    capable.verify().await;
    efficient.verify().await;
    judge.verify().await;
}

#[tokio::test]
async fn gated_capture_recognizes_compressed_terminal_frames() {
    use gated_harness::*;
    let _env = env().await;
    for streaming in [true, false] {
        let (strong, efficient, responses, judge) = (
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
        let body = if streaming {
            anthropic_sse(EFFICIENT_UPSTREAM_MODEL, "DECODED-WEAK-ANSWER")
        } else {
            json!({"type":"message", "role":"assistant", "model":EFFICIENT_UPSTREAM_MODEL,
            "content":[{"type":"text","text":"DECODED-WEAK-ANSWER"}], "stop_reason":"end_turn"})
            .to_string()
        };
        messages_mock(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "zstd")
                .set_body_raw(
                    compressed("zstd", body.as_bytes()),
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                ),
            1,
        )
        .mount(&efficient)
        .await;
        messages_mock(
            judge_text(r#"{"escalate":false,"reason":"progressing"}"#),
            1,
        )
        .mount(&judge)
        .await;
        let tiers = Tiers::of(&strong, &efficient, &responses, judge.uri());
        let gateway =
            start_gateway(gated_config(&tiers, &escalation_router("efficient-alias"))).await;
        let response = client().post(format!("{}/v1/messages", gateway.base_url))
        .header("x-shunt-token", CLIENT_TOKEN)
        .header("accept-encoding", "zstd, identity;q=0")
        .json(&json!({"model":ROUTER_ID,"max_tokens":16,"stream":streaming,"messages":[{"role":"user","content":"hello"}]}))
        .send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["x-gateway-route-source"],
            "escalation_weak"
        );
        assert_eq!(response.headers()["content-encoding"], "zstd");
        let bytes = response.bytes().await.unwrap();
        let text = decoded("zstd", &bytes);
        assert!(text.contains("DECODED-WEAK-ANSWER"));
        if streaming {
            assert!(text.contains("event: message_stop"));
        }
        assert!(text.contains(ROUTER_ID));
        strong.verify().await;
        efficient.verify().await;
        judge.verify().await;
    }
}

#[tokio::test]
async fn mixed_chain_winner_relays_decoded_anthropic_sse() {
    use response_coding_support::*;
    let _env = common::env_lock().await;
    let upstream = MockServer::start().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "gzip")
                .set_body_raw(
                    compressed("gzip", format!("{START}{END}").as_bytes()),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let mut cfg = config(upstream.uri(), false);
    cfg.upstreams.push(serde_json::from_value(json!({"name":"fallback","kind":"responses","base_url":fallback.uri(),"retry":{"max_retries":0}})).unwrap());
    cfg.models[0]
        .upstream_model
        .as_mut()
        .unwrap()
        .insert("fallback".into(), "fallback".into());
    let gateway = gateway(cfg).await;
    let response = request(&gateway, "gzip", false, true, true).await;
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("content-encoding"));
    let text = response.text().await.unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: message_stop"));
    assert!(text.contains("safeguard_results"));
    upstream.verify().await;
    assert!(fallback.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn mixed_chain_maps_a_decoded_error_envelope() {
    use response_coding_support::*;
    let _env = common::env_lock().await;
    let upstream = MockServer::start().await;
    let fallback = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400)
        .insert_header("content-encoding", "deflate")
        .set_body_raw(compressed("deflate", br#"{"type":"error","error":{"type":"invalid_request_error","message":"decoded refusal"}}"#), "application/json"))
        .expect(1).mount(&upstream).await;
    let mut cfg = config(upstream.uri(), false);
    cfg.upstreams.push(serde_json::from_value(json!({"name":"fallback","kind":"responses","base_url":fallback.uri(),"retry":{"max_retries":0}})).unwrap());
    cfg.models[0]
        .upstream_model
        .as_mut()
        .unwrap()
        .insert("fallback".into(), "fallback".into());
    let gateway = gateway(cfg).await;
    let response = request(&gateway, "deflate", false, false, true).await;
    let text = response.text().await.unwrap();
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("decoded refusal"), "{text}");
    upstream.verify().await;
}
