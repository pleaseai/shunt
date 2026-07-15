//! CodSpeed benchmarks for shunt's CPU-bound request-path hot spots.
//!
//! These cover pure, allocation-light functions that run on every proxied
//! request: local token counting (tiktoken), model→route resolution, and
//! hop-by-hop header filtering. They avoid network/IO so the CPU-simulation
//! instrument produces stable, hardware-agnostic measurements.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;

use shunt::config::{Config, RouteConfig, RoutePrefixConfig};
use shunt::{count_tokens, headers, routing};

fn main() {
    divan::main();
}

/// A representative Anthropic Messages request body: a system prompt, a handful
/// of conversation turns, and a tool definition — the shape shunt counts tokens
/// for on every `count_tokens` call routed to a Responses backend.
fn sample_request_body() -> Vec<u8> {
    let body = json!({
        "model": "gpt-5.6-sol",
        "system": "You are a helpful coding assistant. Answer concisely and \
                   include runnable examples when relevant.",
        "messages": [
            {"role": "user", "content": "Explain how a Rust iterator adaptor \
                                         differs from a consuming adaptor."},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Adaptors like `map` are lazy and \
                                          return a new iterator; consumers like \
                                          `collect` drive it to completion."}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "Show a small example for each."},
                {"type": "tool_result", "content": "previous run: exit 0"}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "name": "run_code", "input": {
                    "language": "rust",
                    "source": "let doubled: Vec<i32> = (1..=5).map(|n| n * 2).collect();"
                }}
            ]}
        ],
        "tools": [{
            "name": "run_code",
            "description": "Execute a code snippet in a sandbox and return stdout.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "language": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["language", "source"]
            }
        }]
    });
    serde_json::to_vec(&body).expect("sample body serializes")
}

/// A config with explicit and prefix routes, mirroring a realistic multi-model
/// setup so route resolution walks a non-trivial table.
fn sample_config() -> Config {
    Config {
        routes: vec![
            RouteConfig {
                model: "claude-opus-4".to_string(),
                provider: "anthropic".to_string(),
                upstream_model: None,
                effort: None,
            },
            RouteConfig {
                model: "claude-sonnet-4-5-via-codex".to_string(),
                provider: "codex".to_string(),
                upstream_model: Some("gpt-5.6-sol".to_string()),
                effort: Some("high".to_string()),
            },
        ],
        route_prefixes: vec![RoutePrefixConfig {
            prefix: "gpt-".to_string(),
            provider: "openai".to_string(),
        }],
        ..Default::default()
    }
}

#[divan::bench]
fn count_input_tokens(bencher: divan::Bencher) {
    let body = sample_request_body();
    bencher.bench(|| count_tokens::count_input_tokens(divan::black_box(&body)));
}

#[divan::bench]
fn resolve_route(bencher: divan::Bencher) {
    let config = sample_config();
    let body = serde_json::to_vec(&json!({"model": "gpt-5.6-sol[1m]"})).unwrap();
    bencher.bench(|| routing::resolve(divan::black_box(&config), divan::black_box(&body)));
}

#[divan::bench(args = ["claude-opus-4", "gpt-5-codex", "claude-sonnet-4-5-via-codex", "unknown-model"])]
fn resolve_model(bencher: divan::Bencher, model: &str) {
    let config = sample_config();
    bencher.bench(|| routing::resolve_model(divan::black_box(&config), divan::black_box(model)));
}

#[divan::bench]
fn filter_headers(bencher: divan::Bencher) {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("host", "api.anthropic.com"),
        ("connection", "keep-alive"),
        ("content-length", "2048"),
        ("transfer-encoding", "chunked"),
        ("authorization", "Bearer sk-ant-xxxxxxxxxxxxxxxxxxxx"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "messages-2023-12-15"),
        ("content-type", "application/json"),
        ("user-agent", "claude-cli/1.0"),
        ("x-api-key", "sk-ant-yyyyyyyyyyyyyyyyyyyy"),
    ] {
        headers.append(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    bencher.bench(|| headers::filtered(divan::black_box(&headers)));
}
