# Architecture

> Bird's-eye view of `shunt` (crate `shunt-gateway`) — a Claude Code LLM gateway.
> Describes structure and intent, not implementation detail. For build/test/style
> rules see [`AGENTS.md`](AGENTS.md); for milestone rationale see [`docs/`](docs/).

## System Overview

**Purpose**: A spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol) — a transparent Anthropic-Messages proxy that, for the model ids you map, diverts inference to another provider (OpenAI, ChatGPT/Codex, xAI, Grok, Cursor, or any Anthropic-compatible backend) while passing everything else through to Anthropic unchanged.

**Primary users**: Claude Code (the HTTP client), driven by developers who want to point it at a different or cheaper model without losing tools, skills, or streaming. Operators run `shunt` as a local or hosted process; the CLI (`run`/`check`/`token`/`login`) is the human surface.

**Core workflow**:

1. Claude Code sends an Anthropic `POST /v1/messages` (or `count_tokens`) request to `shunt`.
2. `proxy::forward` buffers the body, snapshots the live config, and `routing::resolve` picks a `Route` from the request's `model` id (exact route → prefix route → `default_provider`).
3. The route's `AdapterKind` selects an adapter, which injects the provider credential, translates the request/response protocol if needed, and forwards upstream.
4. The upstream response streams back to the client **unbuffered** (unless it asked for non-streaming), with gateway-owned errors reshaped into the Anthropic error envelope.

**Key constraints**: Preserve streaming (never buffer upstream SSE unless non-streaming was requested). Keep gateway-owned errors in Anthropic error shape. Add providers/models by config, not code. Unmapped traffic must pass through to Anthropic unchanged.

## Dependency Layers

Dispatch flows downward. Lower layers never import routing/serving policy from the layers above them.

```
┌──────────────────────────────────────────────────────────────┐
│  Interface       main.rs (CLI), server.rs (axum router,        │  CLI subcommands; HTTP endpoints;
│                  routes.rs, discovery, protocol)               │  AppState per-request snapshot
├──────────────────────────────────────────────────────────────┤
│  Application     proxy.rs (buffer → route → dispatch),         │  request pipeline, inbound-auth gate,
│                  routing.rs, count_tokens.rs                    │  count_tokens short-circuit
├──────────────────────────────────────────────────────────────┤
│  Domain          adapters/ (Adapter trait: anthropic,          │  provider protocol adapters +
│                  responses, cursor), model/ (translation)      │  Anthropic ⇄ OpenAI Responses translation
├──────────────────────────────────────────────────────────────┤
│  Infrastructure  auth/, accounts.rs, config.rs, reload.rs,     │  credentials & pooling, typed config +
│                  telemetry.rs, metrics.rs, error.rs,           │  hot reload, TLS/HTTP client,
│                  keepalive.rs, headers.rs, admin/              │  observability, admin surface
└──────────────────────────────────────────────────────────────┘
```

**Invariant**: An adapter receives only a `Route` and an `AppState` (`adapters/mod.rs`). It never reaches back into routing or endpoint policy — routing decides *which* adapter runs; the adapter only decides *how* to talk to its provider. This keeps "add a provider" a config + one-adapter change.

## Entry Points

For understanding **request proxying** (the hot path):

- `src/proxy.rs` — `post` → `forward`: body buffering, config snapshot, inbound-auth gate, `count_tokens` short-circuit, adapter dispatch, metrics. Start here.
- `src/routing.rs` — `resolve` / `resolve_model`: how a `model` id becomes a `Route` (exact → prefix → default), including the `[1m]` context-window suffix stripping.
- `src/adapters/mod.rs` — the `Adapter` trait and `AdapterError`; the seam every provider implements.

For understanding **server startup & state**:

- `src/main.rs` — CLI parsing (`run`/`check`/`token`/`login`), tracing/OTel init, tokio runtime.
- `src/server.rs` — `build_router` (endpoint table + optional admin router) and `AppState` (the per-request config/auth/pool snapshot via `arc-swap`).

For understanding **protocol translation** (Anthropic ⇄ OpenAI Responses):

- `src/model/responses_request.rs` — Anthropic Messages request → OpenAI Responses request.
- `src/model/responses.rs` — Responses stream → Anthropic Messages SSE; `anthropic_error_type`.
- `src/adapters/responses/mod.rs` — the Responses adapter tying translation to upstream I/O; `codex_ws.rs` is the Codex WebSocket v2 transport.

For understanding **config & credentials**:

- `src/config.rs` — typed config, provider defaults (which provider → which adapter/auth), validation.
- `src/auth/mod.rs` + `src/auth/{claude,codex,cursor,xai}/` — per-provider credential lookup, refresh, and `login` flows.

## Module Reference

| Module | Purpose | Key Files | Depends On | Depended By |
| --- | --- | --- | --- | --- |
| `server` | axum router, endpoint registration, `AppState` snapshot | `server.rs` | `proxy`, `routes`, `discovery`, `protocol`, `admin`, `reload`, `accounts`, `auth::inbound` | `main` |
| `proxy` | request pipeline: buffer → route → auth gate → dispatch → metrics | `proxy.rs` | `routing`, `adapters`, `auth`, `count_tokens`, `error`, `metrics` | `server` |
| `routing` | model-id → `Route` (exact/prefix/default), `[1m]` strip | `routing.rs` | `config`, `error` | `proxy`, `routes` |
| `adapters` | provider protocol adapters behind the `Adapter` trait | `adapters/{mod,anthropic,responses,cursor}` | `model`, `auth`, `accounts`, `config`, `error` | `proxy` |
| `model` | Anthropic Messages ⇄ OpenAI Responses translation | `model/{responses,responses_request}.rs` | `config` | `adapters::responses` |
| `auth` | credential lookup/refresh, provider logins, inbound client auth | `auth/mod.rs`, `auth/{claude,codex,cursor,xai}`, `auth/inbound.rs` | `config`, `accounts` | `proxy`, `adapters`, `main`, `admin` |
| `accounts` | multi-account pool + reactive load balancing / rotation | `accounts.rs` | `config` | `server`, `adapters`, `admin` |
| `config` | typed config, provider defaults, TOML/YAML/env load, validation | `config.rs` | — | almost everything |
| `reload` | hot config reload into a hot-swappable `SharedState` | `reload.rs` | `config`, `auth`, `admin` | `server`, `main` |
| `admin` | opt-in browser account provisioning + read-only pool dashboard | `admin/{mod,html,session}.rs` | `accounts`, `auth`, `config` | `server` |
| `discovery` / `protocol` / `routes` | `/v1/models`, `/protocol`, `/routes` metadata endpoints | `discovery.rs`, `protocol.rs`, `routes.rs` | `config`, `routing` | `server` |
| `count_tokens` | local tiktoken token counting for Responses/Cursor routes | `count_tokens.rs` | `model` | `proxy` |
| `telemetry` / `metrics` | tracing, opt-in OTel export, Sentry; request counters | `telemetry.rs`, `metrics.rs` | `config` | `main`, `proxy`, `server` |
| `error` | `ShuntError` / `UpstreamError` → Anthropic error envelope | `error.rs` | — | everywhere errors surface |
| `keepalive` / `headers` | SSE keepalive pings; header hygiene | `keepalive.rs`, `headers.rs` | — | `adapters`, `proxy` |

### Provider → adapter map (default config)

| Provider | Adapter (`AdapterKind`) | Upstream | Auth |
| --- | --- | --- | --- |
| `anthropic` | Anthropic (passthrough) | `api.anthropic.com` | Passthrough / ClaudeOauth |
| `openai` | Responses | `api.openai.com/v1` | `OPENAI_API_KEY` |
| `codex` | Responses (+ WebSocket v2) | `chatgpt.com/backend-api` | ChatGPT OAuth |
| `xai` | Responses | `api.x.ai/v1` | `XAI_API_KEY` |
| `grok` | Responses | `cli-chat-proxy.grok.com/v1` | xAI subscription OAuth |
| `cursor` | Cursor | `api2.cursor.sh` | Cursor OAuth |

`default_provider` is `anthropic`. Any Anthropic-Messages-compatible backend (Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel AI Gateway, …) is reachable via the Anthropic adapter with a custom `base_url` — no code change.

## Architecture Invariants

**Streaming is never buffered.** Upstream SSE is forwarded to the client as it arrives; the body is buffered only when the client explicitly requested non-streaming output. Measured request latency is therefore time-to-headers, not time-to-completion. Violating this breaks Claude Code's incremental rendering and inflates memory.

**Gateway-owned errors use the Anthropic error shape.** Every error the gateway itself emits goes through `ShuntError` / `UpstreamError` in `error.rs`, producing `{"type":"error","error":{"type":…,"message":…}}`. Upstream/network failures flatten to `502 api_error`. Claude Code must never see a non-Anthropic error envelope from `shunt`.

**Providers and models arrive by config, not code.** New providers/models are `[[routes]]`, `[route_prefixes]`, or `[providers.*]` config entries. Do NOT add per-provider branches in routing or a bespoke code path where a config table entry suffices (`AGENTS.md`: "prefer table-driven config additions over hardcoded provider logic").

**Unmapped traffic passes through unchanged.** A request whose model matches no route resolves to `default_provider` (Anthropic by default). Do NOT let a routing/adapter change alter or intercept traffic destined for Anthropic passthrough.

**Config is snapshotted once per request.** `proxy::post` calls `state.refreshed()` at entry, pinning a consistent `config`/`inbound_auth`/`admin_auth` view from the hot-swappable `SharedState` (`arc-swap`) for the whole request. A mid-request reload never changes config underneath an in-flight request; the next request sees the new config. The admin router's *existence* is fixed at startup — a reload re-resolves tokens but cannot add or drop routes.

**Credential writeback is guarded.** Auth modules read and refresh credential files (e.g. `~/.claude/.credentials.json`); refresh is cancellation-safe with off-thread store I/O. Do NOT change credential-file writeback behavior, public config keys, or documented provider semantics without asking (`AGENTS.md` boundaries). Never commit secrets or generated local config.

**One rustls default provider.** Feature unification compiles both `aws-lc-rs` and `ring` rustls providers in; the process-wide default is installed on the first WebSocket handshake (`ensure_crypto_provider` in `adapters/responses/codex_ws.rs`). Do NOT assume rustls auto-selects a provider.

## Cross-Cutting Concerns

**Error handling**: Two types in `error.rs` — `ShuntError` (status + Anthropic `error.type` + message, e.g. `invalid_request_error`, `authentication_error`, `not_supported`) and `UpstreamError` (network/reqwest failure → `502 api_error`). Adapters return `AdapterError { message, response }`; `proxy` maps everything into a client `Response`. CLI (`main.rs`) surfaces failures via `anyhow` with non-zero exit.

**Logging & tracing**: `tracing` with `tracing-subscriber` (`env-filter`, `fmt`); each request runs inside a `proxy_request` span (method, path, and — only when the operator opts in — `session_id`). Opt-in `[otel]` exports traces/metrics/logs over OTLP HTTP/protobuf on dedicated SDK threads (independent of the axum runtime); `[sentry]` adds error/trace capture. Both exporters are built once at startup and are *not* rebuilt on config reload, so telemetry emission is pinned at boot.

**Testing**: `cargo test --all-features --workspace`. Unit tests live in-module (`#[cfg(test)]`); integration tests in `tests/` cover protocol translation (`responses_translate.rs`), passthrough, inbound/multi-account auth, Codex WebSocket fallback, and the admin surface. Upstream providers are mocked with `wiremock`. Target >80% coverage for new code (matches the SonarCloud `new_code` gate). CI runs `cargo fmt --check`, `cargo clippy -D warnings`, and tests with `RUSTFLAGS=-D warnings`.

**Configuration**: `figment`-loaded typed config (`config.rs`) from TOML/YAML/env (`shunt.toml.example`, `shunt.yaml.example`). `notify` watches the file and `reload.rs` swaps a fresh `RuntimeState` into the shared `arc-swap` store — no restart to pick up route/credential edits. Validation (`cargo run -- check`) rejects unknown `default_provider`, mismatched provider kinds, and malformed accounts before serving.

## Quality Notes

**Well-tested / safe to refactor**: `routing.rs`, `error.rs`, and the `model/` translation layer have focused unit + integration coverage and stable contracts. `count_tokens.rs` and `proxy.rs`'s routing edges are exercised directly.

**Fragile / handle with care**: Several core files are large and dense, well over the ~500-line target in `AGENTS.md` — `config.rs` (~2200), `adapters/responses/mod.rs` (~1960), `adapters/responses/codex_ws.rs` (~1890), `adapters/anthropic/mod.rs` (~1070), `model/responses_request.rs` (~1010), and the multi-file `adapters/cursor/` subtree. The Codex WebSocket v2 transport (streaming state, mid-stream HTTP fallback, continuation) and the Cursor protobuf/SSE bridge concentrate the most subtle logic; change them with the integration tests as a guardrail.

**Technical debt**: Tracked in [`.please/docs/tracks/tech-debt-tracker.md`](.please/docs/tracks/tech-debt-tracker.md). Known behavioral gaps (e.g. some upstream 5xx flattened to `502 api_error`, provider-specific error bodies) are recorded in the milestone notes and the gateway-protocol conformance doc.

---

_Last updated: 2026-07-14 — initial ARCHITECTURE.md (agent-first template)._

_Decision records — `shunt` uses milestone specs under [`docs/`](docs/) as its ADR equivalent:_

- _[`docs/m1-responses-translation.md`](docs/m1-responses-translation.md) — Anthropic ⇄ OpenAI Responses translation._
- _[`docs/m2-chatgpt-oauth.md`](docs/m2-chatgpt-oauth.md) — ChatGPT/Codex OAuth credential reuse._
- _[`docs/m4-inbound-auth.md`](docs/m4-inbound-auth.md) — `[server.auth]` inbound client-token gating._
- _[`docs/m5-sse-keepalive.md`](docs/m5-sse-keepalive.md) — SSE keepalive under long turns._
- _[`docs/m7-codex-websocket.md`](docs/m7-codex-websocket.md) — Codex Responses WebSocket v2 transport._
- _[`docs/m8-anthropic-multi-account.md`](docs/m8-anthropic-multi-account.md) & [`docs/m10-codex-multi-account.md`](docs/m10-codex-multi-account.md) — multi-account pooling & load balancing._
- _[`docs/m9-admin-surface.md`](docs/m9-admin-surface.md) — opt-in admin web surface._
- _[`docs/gateway-protocol.md`](docs/gateway-protocol.md) — Claude Code LLM gateway protocol conformance._
