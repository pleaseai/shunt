use serde_json::{json, Value};
use wiremock::ResponseTemplate;

pub fn anthropic_request(model: &str) -> String {
    json!({
        "model": model,
        "max_tokens": 64,
        "stream": false,
        "messages": [{"role": "user", "content": "fixture"}]
    })
    .to_string()
}

pub fn anthropic_streaming_request(model: &str) -> String {
    let mut value: Value = serde_json::from_str(&anthropic_request(model)).unwrap();
    value["stream"] = json!(true);
    value.to_string()
}

pub fn chat_completion_upstream() -> Value {
    json!({
        "id": "chatcmpl-fixture",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-5",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fixture reply"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })
}

pub async fn collect_sse_events(response: reqwest::Response) -> Vec<(String, Value)> {
    use futures_util::StreamExt;

    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(buffer).expect("SSE relay must be UTF-8");
    let mut events = Vec::new();
    for frame in text.split("\n\n") {
        let frame = frame.trim();
        if frame.is_empty() {
            continue;
        }
        let mut event = "message".to_string();
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.to_string();
            }
            if let Some(rest) = line.strip_prefix("data: ") {
                data.push_str(rest);
            }
        }
        if data.is_empty() {
            continue;
        }
        let value = serde_json::from_str(&data).unwrap_or(Value::Null);
        events.push((event, value));
    }
    events
}

pub fn sse_body(frames: &[String]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(
            frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>(),
            "text/event-stream",
        )
}

pub fn chat_delta(delta: Value, finish: Option<&str>) -> String {
    let mut choice = json!({ "index": 0, "delta": delta });
    if let Some(finish) = finish {
        choice["finish_reason"] = json!(finish);
    }
    json!({ "choices": [choice] }).to_string()
}

pub const CHAT_USAGE_CHUNK: &str =
    r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}"#;
