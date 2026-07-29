//! Focused measurements for performance issues #252 and #253.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use shunt::{
    accounts::AccountPool,
    adapters::responses::{
        codex_continuation::{self, StoredContinuation},
        codex_ws,
    },
    config::{AccountConfig, Config, ResponsesFlavor, RouteConfig},
    model::responses_request,
    routing::{self, AdapterKind, Route},
};

fn main() {
    divan::main();
}

const BODY_SIZES: [usize; 3] = [300 * 1024, 1024 * 1024, 3 * 1024 * 1024];

fn realistic_body(target_bytes: usize) -> Vec<u8> {
    let message_count = match target_bytes {
        size if size <= 300 * 1024 => 50,
        size if size <= 1024 * 1024 => 100,
        _ => 200,
    };
    let tools = (0..24)
        .map(|index| {
            json!({
                "name": format!("tool_{index}"),
                "description": "Operate on repository files and return structured results for the coding agent.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Absolute repository path"},
                        "query": {"type": "string", "description": "Search or edit expression"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
                        "options": {"type": "object", "additionalProperties": true}
                    },
                    "required": ["path", "query"]
                }
            })
        })
        .collect::<Vec<_>>();
    let messages = (0..message_count)
        .map(|index| {
            if index % 2 == 0 {
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": format!("I inspected module {index}; the relevant implementation has several ownership and concurrency constraints.")},
                        {"type": "tool_use", "id": format!("toolu_{index}"), "name": format!("tool_{}", index % 24), "input": {"path": format!("/repo/src/module_{index}.rs"), "query": "resolve request and preserve streaming behavior", "limit": 200}}
                    ]
                })
            } else {
                json!({
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": format!("toolu_{}", index - 1), "content": [{"type": "text", "text": format!("source result {index}: fn handler() {{ /* representative repository output */ }}")}]},
                        {"type": "text", "text": "Continue the investigation, compare competing explanations, and cite the exact implementation path."}
                    ]
                })
            }
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": "claude-sonnet-4-5-via-codex",
        "max_tokens": 32000,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 10000},
        "metadata": {"user_id": "{\"device_id\":\"bench\",\"account_uuid\":\"bench\",\"session_id\":\"perf-issues\"}"},
        "system": [{"type": "text", "text": "You are a coding subagent. Inspect the repository rigorously and preserve all protocol semantics."}],
        "messages": messages,
        "tools": tools,
        "tool_choice": {"type": "auto"},
        "parallel_tool_calls": true
    });
    let initial = serde_json::to_vec(&body).unwrap().len();
    let padding = target_bytes.saturating_sub(initial + 64);
    body["system"][0]["text"] = Value::String(format!(
        "You are a coding subagent. Follow the complete repository instructions. {}",
        "The long project context contains architecture, tests, prior decisions, and exact source excerpts. ".repeat(padding / 94 + 1)
    ));
    let mut bytes = serde_json::to_vec(&body).unwrap();
    if bytes.len() > target_bytes {
        let excess = bytes.len() - target_bytes;
        let text = body["system"][0]["text"].as_str().unwrap();
        body["system"][0]["text"] = Value::String(text[..text.len() - excess].to_string());
        bytes = serde_json::to_vec(&body).unwrap();
    }
    assert!(
        bytes.len().abs_diff(target_bytes) <= 128,
        "{} vs {target_bytes}",
        bytes.len()
    );
    bytes
}

fn route() -> Route {
    Route {
        provider: "codex".to_string(),
        adapter: AdapterKind::Responses,
        model: "claude-sonnet-4-5-via-codex".to_string(),
        upstream_model: "gpt-5.6-sol".to_string(),
        effort: Some("high".to_string()),
    }
}

fn config() -> Config {
    Config {
        routes: vec![RouteConfig {
            model: "claude-sonnet-4-5-via-codex".to_string(),
            provider: "codex".to_string(),
            upstream_model: Some("gpt-5.6-sol".to_string()),
            effort: Some("high".to_string()),
        }],
        ..Config::default()
    }
}

fn normalize_like_proxy(body: &[u8]) -> Value {
    let mut request: Value = serde_json::from_slice(body).unwrap();
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            let is_empty = |block: &Value| {
                block.get("type").and_then(Value::as_str) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.trim().is_empty())
            };
            if content.iter().all(is_empty) {
                if content.len() > 1 {
                    content.truncate(1);
                }
            } else {
                content.retain(|block| !is_empty(block));
            }
        }
    }
    request
}

fn adapter_flags(body: &[u8]) -> (bool, bool) {
    let request: Value = serde_json::from_slice(body).unwrap();
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let thinking = request.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled");
    (stream, thinking)
}

#[divan::bench(args = BODY_SIZES)]
fn value_parse(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    bencher.bench(|| serde_json::from_slice::<Value>(divan::black_box(&body)).unwrap());
}

#[divan::bench(args = BODY_SIZES)]
fn proxy_normalize_common_case(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    bencher.bench(|| normalize_like_proxy(divan::black_box(&body)));
}

#[divan::bench(args = BODY_SIZES)]
fn route_parse_and_resolve(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    let config = config();
    bencher.bench(|| routing::resolve(divan::black_box(&config), divan::black_box(&body)).unwrap());
}

#[divan::bench(args = BODY_SIZES)]
fn responses_adapter_flags(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    bencher.bench(|| adapter_flags(divan::black_box(&body)));
}

#[divan::bench(args = BODY_SIZES)]
fn responses_translate(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    let route = route();
    bencher.bench(|| {
        responses_request::translate_request(
            divan::black_box(&body),
            divan::black_box(&route),
            ResponsesFlavor::Chatgpt,
            false,
        )
        .unwrap()
    });
}

#[divan::bench(args = BODY_SIZES)]
fn responses_translate_and_serialize_once(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    let route = route();
    bencher.bench(|| {
        let translated = responses_request::translate_request(
            divan::black_box(&body),
            divan::black_box(&route),
            ResponsesFlavor::Chatgpt,
            false,
        )
        .unwrap();
        translated.to_string()
    });
}

fn parse_once_http_front(body: &[u8], config: &Config, route: &Route) -> String {
    let request = normalize_like_proxy(body);
    let model = request.get("model").and_then(Value::as_str).unwrap();
    let resolved = routing::resolve_model(config, model);
    divan::black_box(resolved);
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let thinking = request.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled");
    divan::black_box((stream, thinking));
    responses_request::translate_request_value(&request, route, ResponsesFlavor::Chatgpt, false)
        .to_string()
}

#[divan::bench(args = BODY_SIZES)]
fn current_http_responses_cpu_front(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    let route = route();
    let config = config();
    bencher.bench(|| {
        let normalized = normalize_like_proxy(divan::black_box(&body));
        divan::black_box(normalized);
        let resolved =
            routing::resolve(divan::black_box(&config), divan::black_box(&body)).unwrap();
        divan::black_box(resolved);
        let flags = adapter_flags(divan::black_box(&body));
        divan::black_box(flags);
        let translated = responses_request::translate_request(
            divan::black_box(&body),
            divan::black_box(&route),
            ResponsesFlavor::Chatgpt,
            false,
        )
        .unwrap();
        translated.to_string()
    });
}

#[divan::bench(args = BODY_SIZES)]
fn parse_once_http_responses_cpu_front(bencher: divan::Bencher, size: usize) {
    let body = realistic_body(size);
    let route = route();
    let config = config();
    bencher.bench(|| {
        parse_once_http_front(
            divan::black_box(&body),
            divan::black_box(&config),
            divan::black_box(&route),
        )
    });
}

fn continuation_fixture(size: usize) -> (StoredContinuation, Value) {
    let translated = responses_request::translate_request(
        &realistic_body(size),
        &route(),
        ResponsesFlavor::Chatgpt,
        false,
    )
    .unwrap();
    let input = translated.get("input").and_then(Value::as_array).unwrap();
    let prefix_len = input.len().saturating_sub(1);
    let stored = StoredContinuation {
        response_id: "resp_bench".to_string(),
        signature: codex_continuation::signature(&translated),
        transcript: input[..prefix_len].to_vec(),
        turn_state: Some("turn-state".to_string()),
    };
    (stored, translated)
}

#[divan::bench(args = BODY_SIZES)]
fn codex_continuation_decide_hit(bencher: divan::Bencher, size: usize) {
    let (stored, current) = continuation_fixture(size);
    bencher.bench(|| {
        codex_continuation::decide(divan::black_box(&stored), divan::black_box(&current)).unwrap()
    });
}

#[divan::bench(args = BODY_SIZES)]
fn codex_ws_reused_prepare_and_serialize(bencher: divan::Bencher, size: usize) {
    let (stored, current) = continuation_fixture(size);
    bencher.bench(|| {
        let signature = codex_continuation::signature(divan::black_box(&current));
        let full_input = current
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut frame_body = current.clone();
        if let Some(decision) =
            codex_continuation::decide(divan::black_box(&stored), divan::black_box(&current))
        {
            if let Some(object) = frame_body.as_object_mut() {
                object.insert("input".to_string(), json!(decision.input_delta));
                object.insert(
                    "previous_response_id".to_string(),
                    json!(decision.previous_response_id),
                );
            }
        }
        let request_input = full_input.clone();
        let frame = codex_ws::response_create_frame(frame_body);
        let payload = serde_json::to_string(&frame).unwrap();
        divan::black_box((payload, signature, request_input))
    });
}

#[divan::bench(args = BODY_SIZES)]
fn parse_once_codex_ws_reused_prepare_and_serialize(bencher: divan::Bencher, size: usize) {
    let (stored, current) = continuation_fixture(size);
    let current = Arc::new(current);
    bencher.bench(|| {
        let signature = codex_continuation::signature(&current);
        let mut frame_body = current.as_ref().clone();
        if let Some(decision) = codex_continuation::decide_with_signature(
            divan::black_box(&stored),
            divan::black_box(&current),
            divan::black_box(&signature),
        ) {
            if let Some(object) = frame_body.as_object_mut() {
                object.insert("input".to_string(), json!(decision.input_delta));
                object.insert(
                    "previous_response_id".to_string(),
                    json!(decision.previous_response_id),
                );
            }
        }
        let record = Arc::clone(&current);
        let frame = codex_ws::response_create_frame(frame_body);
        let payload = serde_json::to_string(&frame).unwrap();
        divan::black_box((payload, record))
    });
}

fn accounts() -> Vec<AccountConfig> {
    (0..8)
        .map(|index| AccountConfig {
            name: format!("account-{index}"),
            uuid: Some(format!("uuid-{index}")),
            ..AccountConfig::default()
        })
        .collect()
}

fn codex_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-used-percent", "24.5"),
        ("x-codex-primary-reset-at", "4102444800"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-used-percent", "31.5"),
        ("x-codex-secondary-reset-at", "4102444800"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
}

const POOL_CYCLES: usize = 16_384;

#[divan::bench(args = [1, 8, 32, 128], sample_count = 8, sample_size = 1)]
fn account_pool_quota_updates(bencher: divan::Bencher, workers: usize) {
    bencher.bench(|| {
        let pool = Arc::new(AccountPool::new());
        let accounts = Arc::new(accounts());
        let headers = Arc::new(codex_headers());
        pool.select_order(
            "codex",
            &accounts,
            Some("warmup"),
            Some("gpt-5.6-sol"),
            None,
        );
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let pool = Arc::clone(&pool);
                let accounts = Arc::clone(&accounts);
                let headers = Arc::clone(&headers);
                let begin = POOL_CYCLES * worker / workers;
                let end = POOL_CYCLES * (worker + 1) / workers;
                scope.spawn(move || {
                    for sequence in begin..end {
                        pool.note_codex_quota(
                            "codex",
                            &accounts[sequence % accounts.len()],
                            &headers,
                        );
                    }
                });
            }
        });
    });
}

#[divan::bench(args = [1, 8, 32, 128], sample_count = 8, sample_size = 1)]
fn account_pool_healthy_updates(bencher: divan::Bencher, workers: usize) {
    bencher.bench(|| {
        let pool = Arc::new(AccountPool::new());
        let accounts = Arc::new(accounts());
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let pool = Arc::clone(&pool);
                let accounts = Arc::clone(&accounts);
                let begin = POOL_CYCLES * worker / workers;
                let end = POOL_CYCLES * (worker + 1) / workers;
                scope.spawn(move || {
                    for sequence in begin..end {
                        pool.mark_healthy("codex", &accounts[sequence % accounts.len()], true);
                    }
                });
            }
        });
    });
}

/// One cycle performs selection, admission + guard drop, quota update (including
/// Sentry/OTel utilization emission under the entries lock), and healthy marking.
#[divan::bench(args = [1, 8, 32, 128], sample_count = 8, sample_size = 1)]
fn account_pool_mixed_cycles(bencher: divan::Bencher, workers: usize) {
    bencher.bench(|| {
        let pool = Arc::new(AccountPool::new());
        let accounts = Arc::new(accounts());
        let headers = Arc::new(codex_headers());
        pool.select_order(
            "codex",
            &accounts,
            Some("warmup"),
            Some("gpt-5.6-sol"),
            None,
        );
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let pool = Arc::clone(&pool);
                let accounts = Arc::clone(&accounts);
                let headers = Arc::clone(&headers);
                let begin = POOL_CYCLES * worker / workers;
                let end = POOL_CYCLES * (worker + 1) / workers;
                scope.spawn(move || {
                    for sequence in begin..end {
                        let account = &accounts[sequence % accounts.len()];
                        let session = format!("session-{sequence}");
                        let order = pool.select_order(
                            "codex",
                            &accounts,
                            Some(&session),
                            Some("gpt-5.6-sol"),
                            None,
                        );
                        divan::black_box(order);
                        let guard = Arc::clone(&pool)
                            .try_admit("codex", account, 1024, true)
                            .unwrap();
                        drop(guard);
                        pool.note_codex_quota("codex", account, &headers);
                        pool.mark_healthy("codex", account, true);
                    }
                });
            }
        });
    });
}
