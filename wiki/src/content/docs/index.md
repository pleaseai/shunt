---
title: "shunt Wiki"
description: "Technical documentation for the shunt Claude Code LLM gateway."
---

`shunt` is a Rust Claude Code LLM gateway that exposes the Anthropic Messages gateway surface, routes by request `model`, passes unmapped models through to Anthropic, and translates mapped OpenAI-family models to the OpenAI Responses API. The CLI starts an Axum server, the router chooses a provider from TOML/env configuration, adapters either stream pass-through bytes or translate Responses SSE, and auth helpers resolve OpenAI, ChatGPT/Codex, and Claude gateway credentials [src/main.rs:38-76](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L38-L76) [src/server.rs:13-25](https://github.com/chatbot-pf/shunt/blob/main/src/server.rs#L13-L25) [src/routing.rs:37-89](https://github.com/chatbot-pf/shunt/blob/main/src/routing.rs#L37-L89) [src/adapters/responses.rs:34-213](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/responses.rs#L34-L213).

## Quick Start

| Step | Command | Expected result | Source |
|---|---|---|---|
| Build | `cargo build --release` | `target/release/shunt` is produced | [docs/running.md:26-37](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L26-L37) |
| Create config | `cp shunt.toml.example shunt.toml` | Local editable TOML config | [docs/running.md:40-55](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L40-L55) |
| Validate config | `./target/release/shunt check` | Prints `config ok` or a typed error | [src/main.rs:77-83](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L77-L83) |
| Run gateway | `./target/release/shunt run` | Logs `shunt listening` | [src/main.rs:38-76](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L38-L76) |
| Connect Claude Code | `export ANTHROPIC_BASE_URL=http://127.0.0.1:3001` | Claude Code sends gateway traffic locally | [docs/running.md:189-393](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L189-L393) |

```mermaid
flowchart LR
    CC[Claude Code] -->|ANTHROPIC_BASE_URL| S[shunt Axum gateway]
    S --> R{Route by model}
    R -->|unmapped| A[Anthropic Messages passthrough]
    R -->|mapped responses provider| O[OpenAI Responses translation]
    O --> C[OpenAI or ChatGPT Codex backend]
    A --> API[api.anthropic.com or compatible gateway]
    classDef dark fill:#2d333b,stroke:#6d5dfc,color:#e6edf3;
    class CC,S,R,A,O,C,API dark;
    linkStyle default stroke:#8b949e;
```
<!-- Sources: README.md:4, src/server.rs:13, src/routing.rs:37, src/adapters/anthropic.rs:31, src/adapters/responses.rs:34 -->

## Documentation Map

| Section | Purpose | Start here when... |
|---|---|---|
| [Onboarding](./onboarding/) | Audience-specific guides for contributors, staff engineers, executives, and PMs | You are new to the project |
| [Getting Started](./01-getting-started/overview.md) | Product overview, setup, config, and operations | You want to run or configure shunt |
| [Deep Dive](./02-deep-dive/architecture.md) | Architecture, routing, adapters, auth, and testing | You need to modify internals |

## Key Files

| File | Responsibility | Source |
|---|---|---|
| `src/main.rs` | CLI, tracing, `run`, `check`, and `token` commands | [src/main.rs:38-76](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L38-L76) |
| `src/server.rs` | Axum router and shared `AppState` | [src/server.rs:13-25](https://github.com/chatbot-pf/shunt/blob/main/src/server.rs#L13-L25) |
| `src/proxy.rs` | Request buffering, routing, adapter dispatch, logging | [src/proxy.rs:19-126](https://github.com/chatbot-pf/shunt/blob/main/src/proxy.rs#L19-L126) |
| `src/routing.rs` | Exact route, prefix route, default-provider resolution | [src/routing.rs:37-89](https://github.com/chatbot-pf/shunt/blob/main/src/routing.rs#L37-L89) |
| `src/config.rs` | Typed config, defaults, figment TOML/env loading, validation | [src/config.rs:9-269](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L9-L269) |
| `src/adapters/anthropic.rs` | Pass-through Anthropic-compatible adapter | [src/adapters/anthropic.rs:31-104](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/anthropic.rs#L31-L104) |
| `src/adapters/responses.rs` | OpenAI Responses transport and streaming response conversion | [src/adapters/responses.rs:34-213](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/responses.rs#L34-L213) |
| `src/model/responses_request.rs` | Anthropic request to Responses request translation | [src/model/responses_request.rs:4-280](https://github.com/chatbot-pf/shunt/blob/main/src/model/responses_request.rs#L4-L280) |
| `src/model/responses.rs` | Responses SSE to Anthropic SSE state machine | [src/model/responses.rs:45-378](https://github.com/chatbot-pf/shunt/blob/main/src/model/responses.rs#L45-L378) |
| `src/auth/*` | Provider credential resolution and token refresh helpers | [src/auth/mod.rs:29-99](https://github.com/chatbot-pf/shunt/blob/main/src/auth/mod.rs#L29-L99) |

## Tech Stack Summary

| Layer | Technology | Why it exists | Source |
|---|---|---|---|
| CLI | `clap` | Provides `run`, `check`, and `token` command surface | [src/main.rs:7-35](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L7-L35) |
| HTTP server | Axum | Serves Claude Code gateway endpoints and streams bodies | [src/server.rs:13-25](https://github.com/chatbot-pf/shunt/blob/main/src/server.rs#L13-L25) |
| Async runtime | Tokio | Drives server and HTTP client futures | [Cargo.toml:1-23](https://github.com/chatbot-pf/shunt/blob/main/Cargo.toml#L1-L23) |
| HTTP client | Reqwest with `rustls-tls` and `stream` | Streams upstream responses without OpenSSL | [Cargo.toml:1-23](https://github.com/chatbot-pf/shunt/blob/main/Cargo.toml#L1-L23) |
| Config | Figment + TOML + env | Merges defaults, file config, and `SHUNT_` overrides | [src/config.rs:185-194](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L185-L194) |
| Observability | Tracing | Logs per-request span fields and latency | [src/proxy.rs:26-60](https://github.com/chatbot-pf/shunt/blob/main/src/proxy.rs#L26-L60) |
| Tests | Wiremock + Tokio tests | Exercises gateway endpoints and translation behavior | [tests/passthrough.rs:72-247](https://github.com/chatbot-pf/shunt/blob/main/tests/passthrough.rs#L72-L247) |

## Related Pages

| Page | Relationship |
|---|---|
| [Overview](./01-getting-started/overview.md) | Explains the gateway problem shunt solves |
| [Configuration](./01-getting-started/configuration.md) | Shows provider and route setup |
| [Architecture](./02-deep-dive/architecture.md) | Maps the runtime components and invariants |
| [Adapters and Translation](./02-deep-dive/adapters-and-translation.md) | Details the pass-through and Responses adapters |
