use axum::{
    body::{Body, Bytes},
    http::{header::CONTENT_TYPE, StatusCode},
    response::Response,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use super::{requested_types, synthesize, transform_stream, MAX_FRAME_BYTES, MIN_SYNTHESIS_BYTES};

type Item = Result<Bytes, std::convert::Infallible>;

const LIMIT: usize = 64 * 1024 * 1024;

fn chunk(text: &str) -> Item {
    Ok(Bytes::from(text.to_owned()))
}

fn types() -> Vec<String> {
    vec!["dangerous_tool_use".to_string()]
}

async fn relay(chunks: Vec<Item>) -> String {
    let out: Vec<_> = transform_stream(stream::iter(chunks), types())
        .collect()
        .await;
    out.into_iter()
        .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
        .collect()
}

/// The `data:` payload of the first frame whose event is `message_delta`.
fn message_delta(relayed: &str) -> Value {
    relayed
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .find(|value| value["type"] == "message_delta")
        .expect("the relay carries a message_delta frame")
}

fn sse_response(body: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn json_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn body_string(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

const DELTA: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n";

#[test]
fn requested_types_reads_the_safeguards_array() {
    let request = json!({
        "model": "claude-sonnet-4-6",
        "safeguards": [{"type": "dangerous_tool_use", "classifier_context": {"cwd": "/tmp"}}],
    });
    assert_eq!(requested_types(&request), vec!["dangerous_tool_use"]);
    assert!(requested_types(&json!({"model": "claude-sonnet-4-6"})).is_empty());
}

#[tokio::test]
async fn message_delta_gains_results_for_every_tool_use_seen() {
    let relayed = relay(vec![
        chunk("event: message_start\ndata: {\"type\":\"message_start\"}\n\n"),
        chunk("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"Bash\"}}\n\n"),
        chunk("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"),
        chunk(DELTA),
    ])
    .await;

    let delta = message_delta(&relayed);
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    let results = &delta["delta"]["safeguard_results"];
    assert_eq!(results[0]["type"], "dangerous_tool_use");
    assert_eq!(results[0]["status"]["type"], "available");
    assert_eq!(
        results[0]["status"]["tool_uses"],
        json!({"toolu_a": {"type": "unavailable", "reason": "error"}}),
        "only the tool_use block contributes an id"
    );
}

#[tokio::test]
async fn a_payload_split_over_several_data_lines_is_parsed_as_one_event() {
    // The SSE spec joins an event's `data:` lines with newlines. Parsed one at
    // a time, neither line is valid JSON, so the tool id would go unrecorded
    // and the delta unrewritten — the session-wide fallback this module
    // prevents. The rewritten delta folds back into a single `data:` line.
    let relayed = relay(vec![
        chunk("event: content_block_start\ndata: {\"type\":\"content_block_start\",\ndata: \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_multi\"}}\n\n"),
        chunk("event: message_delta\ndata: {\"type\":\"message_delta\",\ndata: \"delta\":{\"stop_reason\":\"tool_use\"}}\n\n"),
    ])
    .await;

    let delta = message_delta(&relayed);
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(
        delta["delta"]["safeguard_results"][0]["status"]["tool_uses"],
        json!({"toolu_multi": {"type": "unavailable", "reason": "error"}}),
        "the id from the split content_block_start must reach the results"
    );
    // The start event is not rewritten, so it relays byte-for-byte with both of
    // its data lines; only the rewritten delta folds into a single one.
    let (start, delta_event) = relayed.split_once("event: message_delta").unwrap();
    assert_eq!(
        start.matches("data:").count(),
        2,
        "an unrewritten frame keeps its own framing\n{relayed}"
    );
    assert_eq!(
        delta_event.matches("data:").count(),
        1,
        "the re-serialized delta occupies one data line\n{relayed}"
    );
}

#[tokio::test]
async fn frames_split_across_chunks_still_contribute_their_ids() {
    let relayed = relay(vec![
        chunk("event: content_block_start\ndata: {\"type\":\"content_block_st"),
        chunk("art\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_split\"}}\n"),
        chunk("\nevent: message_del"),
        chunk(
            "ta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        ),
    ])
    .await;

    let delta = message_delta(&relayed);
    assert_eq!(
        delta["delta"]["safeguard_results"][0]["status"]["tool_uses"],
        json!({"toolu_split": {"type": "unavailable", "reason": "error"}})
    );
}

#[tokio::test]
async fn a_turn_without_tool_uses_reports_an_empty_map() {
    let relayed = relay(vec![chunk(DELTA)]).await;
    let delta = message_delta(&relayed);
    assert_eq!(
        delta["delta"]["safeguard_results"][0]["status"]["tool_uses"],
        json!({})
    );
}

#[tokio::test]
async fn an_upstream_verdict_is_relayed_byte_for_byte() {
    let answered = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"safeguard_results\":[{\"type\":\"dangerous_tool_use\",\"status\":{\"type\":\"available\",\"tool_uses\":{}}}]}}\n\n";
    assert_eq!(relay(vec![chunk(answered)]).await, answered);
}

#[tokio::test]
async fn every_other_frame_is_relayed_byte_for_byte() {
    let other = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"m\"}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ": keepalive\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    assert_eq!(relay(vec![chunk(other)]).await, other);
}

#[tokio::test]
async fn an_oversized_frame_passes_through_unmodified() {
    // The `message_delta` is preceded, inside one frame, by more bytes than the
    // parse bound allows: it is forwarded rather than buffered and rewritten.
    let padding = "x".repeat(MAX_FRAME_BYTES + 1);
    let frame = format!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"pad\":\"{padding}\"}}}}\n\n");
    assert_eq!(relay(vec![chunk(&frame)]).await, frame);
}

#[tokio::test]
async fn an_oversized_frame_split_across_chunks_passes_through_unmodified() {
    // The head is released before its boundary arrives, so the tail that
    // follows is not a frame of its own and must not be parsed as one — were it
    // re-serialized, the `message_delta` it happens to contain would be
    // rewritten into the middle of another frame.
    let padding = "x".repeat(MAX_FRAME_BYTES + 1);
    let head = format!("event: message_delta\ndata: {{\"pad\":\"{padding}\",");
    let tail = "\"type\":\"message_delta\",\"delta\":{}}

";
    let relayed = relay(vec![chunk(&head), chunk(tail)]).await;
    assert_eq!(relayed, format!("{head}{tail}"));
}

#[tokio::test]
async fn a_request_without_safeguards_leaves_the_stream_verbatim() {
    let body = format!("event: message_start\ndata: {{\"type\":\"message_start\"}}\n\n{DELTA}");
    let response = synthesize(sse_response(&body), &[], LIMIT).await;
    assert_eq!(body_string(response).await, body);
}

#[tokio::test]
async fn a_non_streaming_message_gains_results() {
    let body = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_b","name":"Bash"},{"type":"text","text":"ok"}]}"#;
    let response = synthesize(json_response(StatusCode::OK, body), &types(), LIMIT).await;
    let value: Value = serde_json::from_str(&body_string(response).await).unwrap();

    assert_eq!(value["id"], "msg_1");
    assert_eq!(
        value["safeguard_results"],
        json!([{
            "type": "dangerous_tool_use",
            "status": {
                "type": "available",
                "tool_uses": {"toolu_b": {"type": "unavailable", "reason": "error"}},
            },
        }])
    );
}

#[tokio::test]
async fn a_non_streaming_upstream_verdict_is_relayed_unchanged() {
    let body = r#"{"type":"message","content":[],"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"unavailable","reason":"disabled"}}]}"#;
    let response = synthesize(json_response(StatusCode::OK, body), &types(), LIMIT).await;
    assert_eq!(body_string(response).await, body);
}

#[tokio::test]
async fn a_non_2xx_response_is_relayed_unchanged() {
    let body = r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#;
    let response = synthesize(
        json_response(StatusCode::INTERNAL_SERVER_ERROR, body),
        &types(),
        LIMIT,
    )
    .await;
    assert_eq!(body_string(response).await, body);
}

#[tokio::test]
async fn a_body_past_the_synthesis_budget_is_relayed_unchanged() {
    // Still bounded: past the resolved budget the body is relayed as it
    // arrived rather than buffered whole.
    let padding = "x".repeat(MIN_SYNTHESIS_BYTES);
    let body = format!(r#"{{"type":"message","content":[],"pad":"{padding}"}}"#);
    let response = synthesize(json_response(StatusCode::OK, &body), &types(), 4).await;
    assert_eq!(body_string(response).await, body);
}

#[tokio::test]
async fn a_lowered_inbound_limit_does_not_disable_synthesis() {
    // The budget reaching this module is `server.limits.max_request_bytes`, an
    // inbound upload cap. Before the floor, an operator lowering it relayed
    // every response without `safeguard_results`, which retires the client's
    // server classifier for the whole session (#622 review).
    let body = r#"{"type":"message","content":[{"type":"tool_use","id":"toolu_c"}]}"#;
    let response = synthesize(json_response(StatusCode::OK, body), &types(), 4).await;
    let value: Value = serde_json::from_str(&body_string(response).await).unwrap();

    assert_eq!(
        value["safeguard_results"][0]["status"]["tool_uses"],
        json!({"toolu_c": {"type": "unavailable", "reason": "error"}}),
        "a 4-byte inbound limit must not suppress the response-side answer"
    );
}
