# Tech Stack

> Auto-generated during `/please:setup` from `Cargo.toml`, `AGENTS.md`, and `README.md`.
> Deliberate technology choices. Any change must be documented here before implementation.

## Overview

`shunt` (crate `shunt-gateway`) is a single Rust binary + library: a spec-compliant
[Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol). It is a
transparent Anthropic-Messages proxy that, for mapped model ids, diverts inference to
another provider at the inference layer and passes everything else through to Anthropic
unchanged.

## Language & Runtime

- **Language**: Rust (edition 2021)
- **Async runtime**: `tokio` (full features) — single serving runtime
- **HTTP framework**: `axum` 0.8 (router, endpoint registration in `src/server.rs`)
- **CLI**: `clap` 4 (derive) — subcommands `run`, `check`, `token`, `login`
- **Crate layout**: library `shunt` (`src/lib.rs`) + binary `shunt` (`src/main.rs`)

## HTTP & Streaming

- **Client**: `reqwest` 0.12 (`rustls-tls`, `stream`) — no OpenSSL/native-tls
- **WebSocket**: `tokio-tungstenite` 0.24 (`rustls-tls-webpki-roots`) — Codex Responses
  WebSocket v2 transport (issue #32)
- **TLS**: `rustls` 0.23 with `aws_lc_rs`; process-wide default provider installed on the
  first WS handshake (`ensure_crypto_provider` in `src/adapters/codex_ws.rs`)
- **Streaming semantics**: upstream SSE is **not buffered** unless the client requested
  non-streaming output — preserving streaming is a hard invariant
- **Compression**: `flate2`

## Config

- **Loader**: `figment` 0.10 (`toml`, `yaml`, `env`) — typed config in `src/config.rs`
- **Hot reload**: `notify` 8 (config file watching)
- **State swap**: `arc-swap` for lock-free config/pool snapshots
- **Formats**: TOML (`shunt.toml.example`) and YAML (`shunt.yaml.example`)
- **Principle**: table-driven provider config over hardcoded provider logic

## Serialization & Protocol Translation

- `serde` / `serde_json` — Anthropic Messages ⇄ OpenAI Responses translation (`src/model/`)
- `prost` — protobuf (OTLP export)
- `base64`, `bytes`, `sha2`, `hex` — encoding/hashing
- `tiktoken-rs` 0.12 — token counting / context accounting
- `regex-lite` — lightweight regex (no full `regex` dependency)

## Auth

- Credential lookup & refresh in `src/auth/`
- `rpassword` — interactive credential prompts
- OAuth / subscription reuse: ChatGPT/Codex (`codex login`), xAI/Grok (`shunt login xai`),
  Cursor (`shunt login cursor`), Anthropic passthrough
- Multi-account pooling + load balancing (Anthropic, Codex) — reactive rotation

## Observability

- **Tracing**: `tracing` + `tracing-subscriber` (`env-filter`, `fmt`)
- **OpenTelemetry** (opt-in `[otel]`): `opentelemetry` 0.32 + `opentelemetry-otlp`
  (HTTP/protobuf over reqwest **blocking** client, dedicated SDK threads independent of the
  axum runtime), `tracing-opentelemetry`, `opentelemetry-appender-tracing`
- **Sentry**: `sentry` 0.48 (`anyhow`, `backtrace`, `panic`, `reqwest`, `rustls`, `tracing`)

## Error Handling

- `anyhow` (application errors), `thiserror` 2 (typed errors)
- Gateway-owned errors are always emitted in **Anthropic error shape**

## Testing

- Built-in Rust test harness — `cargo test --all-features --workspace`
- `wiremock` 0.6 — HTTP mocking for adapter/protocol tests
- `tokio` `test-util` — async test utilities
- Integration tests in `tests/` (protocol + translation)

## Utilities

- `rand` 0.9, `uuid` 1 (v4), `futures-util`

## Providers (built-in)

OpenAI · ChatGPT/Codex · xAI · Grok (SuperGrok/X Premium+) · Cursor · Anthropic passthrough.
Any Anthropic-Messages-compatible backend (Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel
AI Gateway, …) is one config table away — no code changes.

## Toolchain

- `cargo build` / `cargo build --release`
- `cargo fmt --all --check` (format), `cargo clippy --all-targets --all-features -- -D warnings` (lint)
- CI runs format, clippy, and tests with `RUSTFLAGS=-D warnings`
- Third-party GitHub Actions pinned to full commit SHAs
