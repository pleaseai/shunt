use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header::CONTENT_TYPE, Response, StatusCode},
};
use futures_util::{stream, StreamExt};
use http_body::Body as _;
use serde_json::json;

use super::{JsonSpendBody, SpendTap, StreamSpend};
use crate::{
    config::RateLimitsConfig,
    gateway::{
        spend::{
            meter::RequestUsage,
            pricing::{PriceTable, Usage},
            store::Period,
        },
        store::GatewayStores,
    },
    stream_metrics::{observe_served, Protocol, UPSTREAM_TRUNCATED_MARKER},
};

const PRINCIPAL: &str = "alice";
const UPSTREAM_MODEL: &str = "claude-sonnet-4-6";

fn tap() -> SpendTap {
    let tap = SpendTap {
        stores: Arc::new(GatewayStores::new(&RateLimitsConfig::default(), None)),
        prices: Arc::new(PriceTable::from_config(None)),
        principal: Arc::from(PRINCIPAL),
        target: Arc::default(),
    };
    tap.set_target("anthropic", "an-alias", UPSTREAM_MODEL);
    tap
}

fn spent(tap: &SpendTap) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    tap.stores
        .spend
        .meter()
        .spent(PRINCIPAL, Period::Daily, now)
}

/// The list price of `tokens` on the upstream model, computed through the
/// pricing table rather than through the tap.
fn priced(tokens: Usage) -> u64 {
    PriceTable::from_config(None)
        .resolve("anthropic", "an-alias", UPSTREAM_MODEL)
        .expect("a built-in model has a list price")
        .cost_femto_usd(&tokens)
}

fn event(name: &str, data: serde_json::Value) -> String {
    format!("event: {name}\ndata: {data}")
}

fn start(input: u64, output: u64) -> String {
    event(
        "message_start",
        json!({"type": "message_start", "message": {"usage": {
            "input_tokens": input, "output_tokens": output,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}),
    )
}

fn text(text: &str) -> String {
    event(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
    )
}

fn delta(output: u64) -> String {
    event(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": output}}),
    )
}

fn stream_spend(frames: &[String]) -> StreamSpend {
    let mut spend = StreamSpend::new(tap());
    for frame in frames {
        spend.observe_frame(frame.as_bytes());
    }
    spend
}

#[test]
fn a_reported_final_count_is_billed_as_reported() {
    let spend = stream_spend(&[start(100, 1), text("0123456789abcdef"), delta(30)]);
    assert_eq!(
        spend.billable().tokens,
        Usage {
            input_tokens: 100,
            output_tokens: 30,
            ..Usage::default()
        }
    );
}

#[test]
fn a_cut_stream_bills_its_delivered_text_at_four_chars_a_token() {
    // 9 + 5 chars = 14, so 4 tokens rounding up — above message_start's 1.
    let spend = stream_spend(&[start(100, 1), text("123456789"), text("héllo")]);
    assert_eq!(spend.billable().tokens.output_tokens, 4);
    assert_eq!(spend.billable().tokens.input_tokens, 100);
}

#[test]
fn every_kind_of_generated_delta_counts_toward_the_floor() {
    let spend = stream_spend(&[
        event(
            "content_block_delta",
            json!({"delta": {"type": "thinking_delta", "thinking": "abcd"}}),
        ),
        event(
            "content_block_delta",
            json!({"delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}}),
        ),
        event(
            "content_block_delta",
            json!({"delta": {"type": "signature_delta", "signature": "ignored-entirely"}}),
        ),
    ]);
    // 4 + 7 chars of output; the signature is not output.
    assert_eq!(spend.billable().tokens.output_tokens, 3);
}

#[test]
fn a_synthesized_end_after_a_cut_still_bills_the_floor() {
    // The Responses adapter's cut: marker, then a message_delta with 0 output.
    let marker = std::str::from_utf8(UPSTREAM_TRUNCATED_MARKER).unwrap();
    let spend = stream_spend(&[start(10, 0), text("abcdefgh"), marker.to_string(), delta(0)]);
    assert_eq!(spend.billable().tokens.output_tokens, 2);
}

#[test]
fn later_usage_replaces_earlier_and_web_searches_are_counted() {
    let spend = stream_spend(&[
        start(100, 1),
        event(
            "message_delta",
            json!({"usage": {"input_tokens": 120, "cache_read_input_tokens": 7,
                             "output_tokens": 9,
                             "server_tool_use": {"web_search_requests": 2}}}),
        ),
    ]);
    assert_eq!(
        spend.billable(),
        RequestUsage {
            tokens: Usage {
                input_tokens: 120,
                output_tokens: 9,
                cache_read_input_tokens: 7,
                cache_creation_input_tokens: 0,
            },
            web_search_requests: 2,
        }
    );
}

#[test]
fn settle_records_once_on_the_principal_and_only_for_success() {
    let spend = stream_spend(&[start(1000, 1), delta(200)]);
    let tap = spend.tap.clone();
    spend.settle(StatusCode::OK);
    let expected = priced(Usage {
        input_tokens: 1000,
        output_tokens: 200,
        ..Usage::default()
    });
    assert!(expected > 0);
    assert_eq!(spent(&tap), expected);

    let failed = StreamSpend {
        tap: tap.clone(),
        ..stream_spend(&[start(1000, 1), delta(200)])
    };
    failed.settle(StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(spent(&tap), expected, "a non-2xx stream is not billed");
}

#[test]
fn nothing_is_billed_without_usage_or_text_or_without_a_target() {
    let tap = tap();
    StreamSpend::new(tap.clone()).settle(StatusCode::OK);
    assert_eq!(spent(&tap), 0, "no usage and no text");

    let untargeted = SpendTap {
        target: Arc::default(),
        ..tap.clone()
    };
    let mut spend = StreamSpend::new(untargeted);
    spend.observe_frame(start(1000, 1).as_bytes());
    spend.settle(StatusCode::OK);
    assert_eq!(spent(&tap), 0, "no winner was ever named");
}

fn sse_response(chunks: Vec<Bytes>) -> Response<Body> {
    let body =
        stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).chain(stream::pending());
    Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

/// A client that hangs up mid-stream: the observer's drop path bills the
/// floor from the text it had already forwarded.
#[tokio::test]
async fn a_client_drop_mid_stream_bills_the_delivered_floor() {
    let tap = tap();
    let response = observe_served(
        sse_response(vec![
            Bytes::from(format!("{}\n\n", start(40, 1))),
            Bytes::from(format!("{}\n\n", text("abcdefghijkl"))),
        ]),
        Protocol::Anthropic,
        Arc::default(),
        Arc::default(),
        Instant::now(),
        Some(tap.clone()),
    );
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    body.next().await.unwrap().unwrap();
    assert_eq!(spent(&tap), 0, "nothing is billed before the stream ends");
    drop(body);

    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 40,
            output_tokens: 3,
            ..Usage::default()
        })
    );
}

fn json_body(bytes: &'static [u8]) -> Body {
    Body::from(bytes)
}

const MESSAGE: &[u8] = br#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":50,"output_tokens":20,"cache_read_input_tokens":5}}"#;

#[tokio::test]
async fn a_json_body_is_forwarded_unchanged_and_billed_from_its_usage() {
    let tap = tap();
    let body = JsonSpendBody::new(json_body(MESSAGE), tap.clone(), StatusCode::OK);
    assert_eq!(body.size_hint().exact(), Some(MESSAGE.len() as u64));
    let forwarded = to_bytes(Body::new(body), usize::MAX).await.unwrap();

    assert_eq!(forwarded.as_ref(), MESSAGE);
    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 50,
            output_tokens: 20,
            cache_read_input_tokens: 5,
            cache_creation_input_tokens: 0,
        })
    );
}

#[tokio::test]
async fn a_json_body_past_the_bound_bills_a_floor_from_its_bytes() {
    let tap = tap();
    let body = JsonSpendBody::with_bound(json_body(MESSAGE), tap.clone(), StatusCode::OK, 16);
    let forwarded = to_bytes(Body::new(body), usize::MAX).await.unwrap();

    assert_eq!(
        forwarded.as_ref(),
        MESSAGE,
        "the tee never shortens the body"
    );
    assert_eq!(
        spent(&tap),
        priced(Usage {
            output_tokens: (MESSAGE.len() as u64).div_ceil(4),
            ..Usage::default()
        })
    );
}

#[tokio::test]
async fn an_unreadable_or_cut_json_body_bills_a_floor_not_zero() {
    let tap = tap();
    let body = JsonSpendBody::new(
        json_body(b"{\"usage\": \"no\"}"),
        tap.clone(),
        StatusCode::OK,
    );
    to_bytes(Body::new(body), usize::MAX).await.unwrap();
    let floor = priced(Usage {
        output_tokens: 4,
        ..Usage::default()
    });
    assert_eq!(spent(&tap), floor, "15 bytes of unreadable usage");

    // Cut: the client is gone after the first of two chunks.
    let chunks = stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
        b"{\"usage\":{\"in",
    ))])
    .chain(stream::pending());
    let mut body = Body::new(JsonSpendBody::new(
        Body::from_stream(chunks),
        tap.clone(),
        StatusCode::OK,
    ))
    .into_data_stream();
    body.next().await.unwrap().unwrap();
    drop(body);
    assert_eq!(
        spent(&tap),
        floor * 2,
        "13 bytes: 4 tokens, the same floor again"
    );
}

#[tokio::test]
async fn an_error_json_body_is_not_billed() {
    let tap = tap();
    let body = JsonSpendBody::new(json_body(MESSAGE), tap.clone(), StatusCode::BAD_REQUEST);
    to_bytes(Body::new(body), usize::MAX).await.unwrap();
    assert_eq!(spent(&tap), 0);
}

/// A gated capture billed after the fact: whole frames only, the partial
/// trailing frame ignored, and the delivered-text floor when the capture was
/// cut before its final usage.
#[test]
fn captured_sse_bytes_bill_like_the_stream_they_were() {
    let tap = tap();
    let cut = format!(
        "{}\r\n\r\n{}\n\n{}",
        start(40, 1),
        text("abcdefghijkl"),
        "event: content_block_delta\ndata: {\"delta\":{\"text\":\"never fin"
    );
    tap.bill_sse(StatusCode::OK, cut.as_bytes());
    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 40,
            output_tokens: 3,
            ..Usage::default()
        })
    );

    tap.bill_sse(StatusCode::BAD_GATEWAY, cut.as_bytes());
    tap.bill_json(StatusCode::BAD_GATEWAY, MESSAGE);
    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 40,
            output_tokens: 3,
            ..Usage::default()
        }),
        "a non-2xx capture or reply is not billed"
    );
}
