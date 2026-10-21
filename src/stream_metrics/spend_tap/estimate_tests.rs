//! The Responses floor's input estimate (codex-spend-caps, T006): a turn that
//! never reports its input bills an estimate over the retained request body,
//! and a turn that does report it never runs the estimator.
//!
//! These run without a Tokio runtime, so [`StreamSpend::settle`] estimates on
//! the test's own thread and the thread-local [`ESTIMATES`] counter sees it.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{body::Bytes, http::StatusCode};
use futures_util::FutureExt;
use serde_json::json;

use super::{json::JsonSpendBody, SpendTap, StreamSpend, ESTIMATES};
use crate::{
    compression::BodyEncoding,
    config::RateLimitsConfig,
    count_tokens::count_responses_input_tokens,
    gateway::{
        spend::{
            meter::window,
            pricing::{PriceTable, Usage},
            store::Period,
        },
        store::GatewayStores,
    },
    proxy::spend_gate::ServedTarget,
    stream_metrics::Protocol,
};

const PRINCIPAL: &str = "alice";
const UPSTREAM_MODEL: &str = "claude-sonnet-4-6";
const MAX_BYTES: usize = 1 << 20;

fn request_json() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "gpt-5.6-sol",
        "instructions": "You are a careful coding agent. Answer briefly.",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "Summarise the README."}]}],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}],
    }))
    .unwrap()
}

fn tap(body: Vec<u8>, encoding: BodyEncoding) -> SpendTap {
    let tap = SpendTap {
        stores: Arc::new(GatewayStores::new(&RateLimitsConfig::default(), None)),
        prices: Arc::new(PriceTable::from_config(None)),
        principal: Arc::from(PRINCIPAL),
        target: Arc::default(),
        request: None,
    }
    .with_request_body(Bytes::from(body), encoding, MAX_BYTES);
    tap.set_target(&ServedTarget {
        provider: "anthropic".to_string(),
        model: "an-alias".to_string(),
        upstream_model: UPSTREAM_MODEL.to_string(),
        meters: true,
    });
    tap
}

fn spent(tap: &SpendTap) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let meter = tap.stores.spend.meter();
    let current = meter.spent(PRINCIPAL, Period::Daily, now);
    if window(Period::Daily, now).start == window(Period::Daily, now - 120).start {
        current
    } else {
        current + meter.spent(PRINCIPAL, Period::Daily, now - 120)
    }
}

fn priced(tokens: Usage) -> u64 {
    PriceTable::from_config(None)
        .resolve("anthropic", "an-alias", UPSTREAM_MODEL)
        .expect("a built-in model has a list price")
        .cost_femto_usd(&tokens)
}

fn frame(data: serde_json::Value) -> String {
    format!("data: {data}")
}

/// 16 characters of output text: a 4-token floor.
fn text() -> String {
    frame(json!({"type": "response.output_text.delta", "delta": "0123456789abcdef"}))
}

/// Settles a Responses stream of `frames` on `tap`, returning how many input
/// estimates that made.
fn settle(tap: &SpendTap, frames: &[String], status: StatusCode) -> usize {
    let before = ESTIMATES.with(std::cell::Cell::get);
    let mut spend = StreamSpend::for_protocol(tap.clone(), Protocol::Responses);
    for frame in frames {
        spend.observe_frame(frame.as_bytes());
    }
    spend.settle(status);
    ESTIMATES.with(std::cell::Cell::get) - before
}

fn floor(estimate: u64) -> u64 {
    assert!(estimate > 20, "the fixture must estimate a real input");
    priced(Usage {
        input_tokens: estimate,
        output_tokens: 4,
        ..Usage::default()
    })
}

#[test]
fn a_turn_cut_before_its_terminal_bills_the_estimated_input_and_delivered_output() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    assert_eq!(settle(&tap, &[text()], StatusCode::OK), 1);
    let estimate = count_responses_input_tokens(&request_json());
    assert_eq!(spent(&tap), floor(estimate));
}

#[test]
fn a_turn_that_reports_its_usage_never_runs_the_estimator() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    let completed = frame(json!({"type": "response.completed", "response": {
        "usage": {"input_tokens": 7, "output_tokens": 2}}}));
    assert_eq!(settle(&tap, &[text(), completed], StatusCode::OK), 0);
    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 7,
            output_tokens: 2,
            ..Usage::default()
        }),
        "the reported usage is billed as reported"
    );
}

#[test]
fn a_failed_turn_bills_its_usage_or_else_the_floor() {
    let tap_reported = tap(request_json(), BodyEncoding::Identity);
    let failed = frame(json!({"type": "response.failed", "response": {
        "status": "failed", "usage": {"input_tokens": 30, "output_tokens": 9}}}));
    assert_eq!(settle(&tap_reported, &[text(), failed], StatusCode::OK), 0);
    assert_eq!(
        spent(&tap_reported),
        priced(Usage {
            input_tokens: 30,
            output_tokens: 9,
            ..Usage::default()
        })
    );

    let estimate = count_responses_input_tokens(&request_json());
    for terminal in [
        json!({"type": "response.failed", "response": {"status": "failed", "usage": null}}),
        json!({"type": "error", "code": "server_error", "message": "boom"}),
        // Usage that counts only the output still leaves the input unknown.
        json!({"type": "response.completed", "response": {"usage": {"output_tokens": 4}}}),
    ] {
        let tap = tap(request_json(), BodyEncoding::Identity);
        assert_eq!(
            settle(&tap, &[text(), frame(terminal.clone())], StatusCode::OK),
            1,
            "{terminal}"
        );
        assert_eq!(spent(&tap), floor(estimate), "{terminal}");
    }
}

#[test]
fn a_zstd_request_is_decoded_for_its_estimate() {
    let compressed = zstd::stream::encode_all(request_json().as_slice(), 3).unwrap();
    let tap = tap(compressed, BodyEncoding::Zstd);
    assert_eq!(settle(&tap, &[text()], StatusCode::OK), 1);
    assert_eq!(
        spent(&tap),
        floor(count_responses_input_tokens(&request_json()))
    );
}

#[test]
fn a_non_2xx_turn_is_neither_estimated_nor_billed() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    assert_eq!(settle(&tap, &[text()], StatusCode::BAD_GATEWAY), 0);
    assert_eq!(spent(&tap), 0);
}

#[test]
fn an_anthropic_stream_never_estimates_a_responses_body() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    let before = ESTIMATES.with(std::cell::Cell::get);
    StreamSpend::new(tap.clone()).settle(StatusCode::OK);
    assert_eq!(ESTIMATES.with(std::cell::Cell::get), before);
    assert_eq!(spent(&tap), 0);
}

/// Settles a non-streamed Responses reply of `body` on `tap`, returning how
/// many input estimates that made.
fn settle_json(tap: &SpendTap, body: &[u8], bound: usize) -> usize {
    let before = ESTIMATES.with(std::cell::Cell::get);
    let reply = JsonSpendBody::with_bound(
        axum::body::Body::from(body.to_vec()),
        tap.clone(),
        StatusCode::OK,
        bound,
    )
    .protocol(Protocol::Responses);
    // An in-memory body is ready at once, so no executor is needed (and no
    // runtime, which keeps the estimate on this thread).
    let collected = http_body_util::BodyExt::collect(reply)
        .now_or_never()
        .expect("an in-memory body is ready");
    assert!(collected.is_ok());
    ESTIMATES.with(std::cell::Cell::get) - before
}

#[test]
fn a_json_reply_without_input_usage_bills_the_estimate_and_output_floor() {
    let estimate = count_responses_input_tokens(&request_json());
    let message = json!({"output": [{"type": "message", "content": [
        {"type": "output_text", "text": "0123456789abcdef"}]}]});
    let output_only_usage = json!({"output": [{"type": "message", "content": [
        {"type": "output_text", "text": "0123456789abcdef"}]}],
        "usage": {"output_tokens": 4}});
    // Output floor: the delivered text when a usage block lacks its output
    // count, the body's own bytes (quarter-rated) when there is no usage.
    let no_usage_bytes = serde_json::to_vec(&message).unwrap();
    let no_usage_output = (no_usage_bytes.len() as u64).div_ceil(4);
    for (body, output) in [
        (no_usage_bytes, no_usage_output),
        (serde_json::to_vec(&output_only_usage).unwrap(), 4),
    ] {
        let tap = tap(request_json(), BodyEncoding::Identity);
        assert_eq!(settle_json(&tap, &body, MAX_BYTES), 1);
        assert_eq!(
            spent(&tap),
            priced(Usage {
                input_tokens: estimate,
                output_tokens: output,
                ..Usage::default()
            })
        );
    }
}

#[test]
fn a_json_reply_past_the_tee_bound_still_bills_the_estimated_input() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    // 16 bytes uncopied: a 4-token byte floor.
    assert_eq!(settle_json(&tap, &[b' '; 16], 8), 1);
    assert_eq!(
        spent(&tap),
        floor(count_responses_input_tokens(&request_json()))
    );
}

#[test]
fn a_json_reply_dropped_before_its_first_poll_stays_unmetered() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    let before = ESTIMATES.with(std::cell::Cell::get);
    let reply = JsonSpendBody::with_bound(
        axum::body::Body::from(b"{}".to_vec()),
        tap.clone(),
        StatusCode::OK,
        MAX_BYTES,
    )
    .protocol(Protocol::Responses);
    drop(reply);
    assert_eq!(ESTIMATES.with(std::cell::Cell::get), before);
    assert_eq!(spent(&tap), 0);
}

#[test]
fn a_json_reply_that_reports_its_input_never_runs_the_estimator() {
    let tap = tap(request_json(), BodyEncoding::Identity);
    let body = serde_json::to_vec(&json!({
        "usage": {"input_tokens": 7, "output_tokens": 2}}))
    .unwrap();
    assert_eq!(settle_json(&tap, &body, MAX_BYTES), 0);
    assert_eq!(
        spent(&tap),
        priced(Usage {
            input_tokens: 7,
            output_tokens: 2,
            ..Usage::default()
        })
    );
}
