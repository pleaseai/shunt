//! CodSpeed benchmarks for shunt's CPU-bound request-path hot spots.
//!
//! Three groups, all avoiding network/IO so the CPU-simulation instrument
//! produces stable, hardware-agnostic measurements:
//!
//! - Pure, allocation-light helpers that run on every proxied request: local
//!   token counting (tiktoken), model→route resolution, hop-by-hop header
//!   filtering.
//! - The heavier translation paths that dominate CPU on Responses-routed and
//!   streamed requests: Anthropic Messages → Responses request translation
//!   (per request), Responses SSE parse + Anthropic-SSE state folding (per
//!   event), and Cursor SSE framing (per token delta).
//! - Cursor Connect gzip decompression over representative compressed response
//!   frame sizes, including its output allocation and inflate work.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use std::io::Write;

use shunt::adapters::cursor::connect::decode_gzip_frame as decode_gzip_frame_sync;
use shunt::adapters::cursor::sse::CursorSseFramer;
use shunt::config::{Config, ResponsesFlavor, RouteConfig, RoutePrefixConfig};
use shunt::model::{responses, responses_request};
use shunt::routing::{AdapterKind, Route};
use shunt::{count_tokens, headers, routing};

fn main() {
    divan::main();
}

/// Build compressible protobuf-like data at about a 4:1 decompressed-to-gzip
/// ratio. Each record is a length-delimited UTF-8 field with realistic repeated
/// framing text and deterministic per-chunk content.
fn gzip_fixture(compressed_target: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(compressed_target * 4);
    let mut state = 0x4d59_5df4_d0f3_3173u64;
    let mut chunk = 0usize;
    while payload.len() < compressed_target * 4 {
        let mut suffix = [0u8; 40];
        for byte in &mut suffix {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = b'a' + (state % 26) as u8;
        }
        let text = format!(
            "Cursor agent response chunk {chunk}: reasoning and answer text for coding session {}. ",
            std::str::from_utf8(&suffix).expect("ASCII suffix")
        );
        payload.push(0x0a); // protobuf field 1, wire type 2
        payload.push(text.len() as u8);
        payload.extend_from_slice(text.as_bytes());
        chunk += 1;
    }

    loop {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).expect("gzip fixture writes");
        let compressed = encoder.finish().expect("gzip fixture finishes");
        if compressed.len() <= compressed_target {
            assert!(compressed.len() >= compressed_target * 3 / 4);
            return compressed;
        }
        payload.truncate(payload.len() * 15 / 16);
    }
}

/// Cursor Connect gzip frame decode over representative compressed frame sizes.
/// The fixture is protobuf-shaped UTF-8 data with a realistic compression ratio,
/// rather than an artificially compressible repeated byte.
#[divan::bench(args = [1024, 4096, 16384, 65536])]
fn decode_gzip_frame(bencher: divan::Bencher, compressed_target: usize) {
    let compressed = gzip_fixture(compressed_target);
    bencher.bench(|| decode_gzip_frame_sync(divan::black_box(&compressed)).unwrap());
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

/// A representative streamed Responses SSE turn: a `response.created`, a run of
/// `output_text.delta` frames, and a final `response.completed` carrying usage —
/// the event sequence the relay parses and the Anthropic SSE machine folds on
/// every streamed response.
fn sample_sse_stream() -> String {
    let mut out =
        String::from("event: response.created\ndata: {\"response\":{\"id\":\"resp_bench\"}}\n\n");
    for _ in 0..64 {
        out.push_str(
            "event: response.output_text.delta\ndata: {\"delta\":\"a chunk of streamed text \"}\n\n",
        );
    }
    out.push_str(
        "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":320}}}\n\n",
    );
    out
}

/// Anthropic Messages → OpenAI Responses request translation. Runs once on every
/// request routed to a Responses backend, walking the full message array to
/// build the upstream `input` items.
#[divan::bench]
fn translate_request(bencher: divan::Bencher) {
    let body = sample_request_body();
    let route = Route {
        provider: "codex".to_string(),
        adapter: AdapterKind::Responses,
        model: "claude-sonnet-4-5-via-codex".to_string(),
        upstream_model: "gpt-5.6-sol".to_string(),
        effort: None,
    };
    bencher.bench(|| {
        responses_request::translate_request(
            divan::black_box(&body),
            divan::black_box(&route),
            ResponsesFlavor::Chatgpt,
            false,
        )
    });
}

/// Parse a raw Responses SSE turn into events — the framing + JSON step the HTTP
/// and websocket relays run over every upstream chunk.
#[divan::bench]
fn parse_responses_sse(bencher: divan::Bencher) {
    let sse = sample_sse_stream();
    bencher.bench(|| responses::parse_sse_events(divan::black_box(&sse)));
}

/// Fold a parsed Responses SSE turn into Anthropic SSE frames. `apply` is
/// stateful, so each iteration takes a fresh machine and a fresh copy of the
/// pre-parsed events.
#[divan::bench]
fn apply_responses_sse(bencher: divan::Bencher) {
    let events = responses::parse_sse_events(&sample_sse_stream());
    bencher
        .with_inputs(|| {
            (
                responses::AnthropicSseMachine::new("claude-sonnet-4-5", false, false)
                    .without_content_accumulation(),
                events.clone(),
            )
        })
        .bench_values(|(mut machine, events)| {
            let mut out = Vec::new();
            for event in events {
                out.extend(machine.apply(event));
            }
            out
        });
}

/// Cursor streaming framer: each upstream token delta is serialized directly
/// into the reusable SSE output buffer. Benchmarks repeated flushes of text
/// deltas — the per-token hot path and buffer-capacity reuse. The framer is
/// stateful, so each iteration takes a fresh one.
#[divan::bench]
fn frame_cursor_sse(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| CursorSseFramer::new("msg_bench", "claude-sonnet-4-5"))
        .bench_refs(|framer| {
            framer.ensure_start();
            for _ in 0..8 {
                for _ in 0..8 {
                    framer.emit_text_delta(divan::black_box("a chunk of streamed text "));
                }
                divan::black_box(framer.take_output());
            }
        });
}
