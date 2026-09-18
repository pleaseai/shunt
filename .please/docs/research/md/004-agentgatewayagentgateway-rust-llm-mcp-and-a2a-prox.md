---
id: 004
title: "agentgateway/agentgateway: Rust LLM, MCP and A2A proxy with an ordered translation matrix"
url: "https://github.com/agentgateway/agentgateway/tree/35ab209d559665856228f33ecfdf170b6f46f482"
date: 2026-09-16
summary: "Apache-2.0 Rust proxy whose crates/llm is the closest public prior art to shunt's src/model: an ordered InputFormat x ChatFormat translation table resolved against per-model capability tags from a remote model catalog, plus virtual-model failover, guardrails, and a LiteLLM importer. Credentials are API-key and cloud-IAM shaped — it forwards an Anthropic sk-ant-oat* token but never mints, refreshes, or pools one."
tags: [agentgateway, rust, llm-gateway, mcp, a2a, protocol-translation, model-catalog, failover, prior-art, shunt-comparison]
---

# agentgateway/agentgateway: Rust LLM, MCP and A2A proxy with an ordered translation matrix

## Version examined

`agentgateway/agentgateway` `main` at commit [`35ab209d559665856228f33ecfdf170b6f46f482`](https://github.com/agentgateway/agentgateway/commit/35ab209d559665856228f33ecfdf170b6f46f482) ("llm: re-use cached JSON parse (#3469)", committed 2026-09-14). Latest tagged release at the time of reading: `v1.5.0` (2026-08-27).

Repository metadata (GitHub API, 2026-09-16): Rust, Apache-2.0, 4,865 stars, 846 forks, created 2025-03-18. Topics: `agents`, `ai`, `ai-gateway`, `api-gateway`, `gateway-api`, `kubernetes`, `mcp`, `mcp-gateway`, `reverse-proxy`, `rust`, `service-mesh`.

## Direct answer

Agentgateway is an **agent-native proxy** that bundles three gateways behind one binary: an LLM gateway (OpenAI-compatible front door onto many providers), an **MCP gateway** (tool federation over stdio/HTTP/SSE/Streamable HTTP with OAuth and RBAC), and an **A2A gateway** (agent-to-agent capability discovery). It also does Kubernetes inference routing (Gateway API inference extensions — GPU/KV-cache/LoRA/queue-depth aware endpoint picking).

It ships in two deployment shapes from the same code: **standalone**, driven by a flat YAML config (`llm:` / `binds:` blocks, JSON-schema-published at `https://agentgateway.dev/schema/config`), and **Kubernetes**, driven by a built-in controller plus the Gateway API.

For shunt the interesting part is `crates/llm` (~30.9k LOC of Rust), which solves the same problem as `src/model/` — cross-protocol chat translation — at a wider N x M.

## Workspace layout

`Cargo.toml` declares a 13-member workspace, edition 2024, `rust-version = 1.90`, everything `publish = false`:

| Crate | Role |
|---|---|
| `crates/agentgateway` | Main proxy: `llm/`, `mcp/`, `a2a/`, `proxy/`, `http/`, `telemetry/`, `types/`, `import.rs`, `ui.rs` |
| `crates/llm` | Provider types, conversions, SSE parsing, tokenizer, model-catalog handle |
| `crates/core`, `crates/http`, `crates/hbone`, `crates/pool`, `crates/xds`, `crates/protos` | Transport/infra |
| `crates/celx`, `crates/cel-fork/*` | CEL policy engine (forked `cel`) |

Four `[patch.crates-io]` entries point at maintainer forks (`schemars`, `http-serde`, `wiremock`, `async-openai`) — the same fork-pinning tension shunt hit with its codex-ws deflate fork, but agentgateway does not publish to crates.io, so it is free to do it.

## The translation matrix (the key finding)

Two enums in [`crates/llm/src/lib.rs`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/llm/src/lib.rs) separate **what the client sent** from **what the upstream speaks**:

- `InputFormat` (client-facing): `Completions`, `Messages`, `Responses`, `Embeddings`, `Realtime`, `CountTokens`, `Detect`, `Rerank`, `Gemini`, `GeminiCountTokens`. `is_chat()` is true for the first three plus `Gemini`.
- `ChatFormat` (upstream wire): `OpenAICompletions`, `OpenAIResponses`, `AnthropicMessages`, `BedrockConverse`, `VertexGemini`.

`RouteType` maps an inbound **path** to an `InputFormat`, and the default table lives in [`llm/model_router.rs::default_route_types`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/agentgateway/src/llm/model_router.rs): `/v1/chat/completions`, `/v1/messages`, `/v1/messages/count_tokens`, `:rawPredict`, `:streamRawPredict`, `:generateContent`, `:streamGenerateContent`, `:countTokens`, `/v1/responses`, `/v1/images/{generations,edits,variations}`, `/v1/responses/compact`, `/v1/embeddings`, `/v1/rerank`, `/v2/rerank`, and a `*` catch-all mapped to `Passthrough`.

Dispatch is a **single ordered const table**, not a match tree ([`llm/mod.rs`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/agentgateway/src/llm/mod.rs)):

```rust
/// Ordered chat conversion table.
///
/// For a client `InputFormat`, pick the first entry whose `ChatFormat` is
/// supported by the selected provider/model. Put cheaper or more native
/// translations before broader fallbacks.
const CHAT_TRANSLATIONS: &[ChatTranslation] = ...
```

The 12 entries, in order:

| # | Client `InputFormat` | Upstream `ChatFormat` | Note in source |
|--:|---|---|---|
| 1 | Responses | OpenAIResponses | direct passthrough |
| 2 | Gemini | VertexGemini | direct passthrough |
| 3 | Completions | VertexGemini | **quirk**: preferred over the compat shim because "our conversion does a better job than Google's OpenAI compatible endpoint" |
| 4 | Completions | OpenAICompletions | |
| 5 | Messages | AnthropicMessages | |
| 6 | Completions | AnthropicMessages | |
| 7 | Completions | BedrockConverse | |
| 8 | Messages | OpenAICompletions | |
| 9 | Messages | OpenAIResponses | |
| 10 | Messages | BedrockConverse | |
| 11 | Responses | OpenAICompletions | |
| 12 | Responses | BedrockConverse | |

Two gaps are recorded **in the table itself** as comments rather than left implicit: `// Missing: Bedrock --> Bedrock` and `// Missing: Responses -> Messages`.

Concrete conversion modules under `crates/llm/src/conversion/`: `bedrock.rs` (4,317 lines — by far the largest), `responses.rs`, `vertex_gemini.rs`, `completions.rs`, `messages.rs`, `openai_compat.rs`, `vertex.rs`. Each exposes `from_X::translate`, `translate_response`, and `translate_stream`.

## Capability tags drive selection, and they come from a remote catalog

The table is resolved against **per-model capability tags**, not hardcoded provider knowledge ([`crates/llm/src/model_catalog.rs`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/llm/src/model_catalog.rs)):

```rust
pub const ANTHROPIC_MESSAGES: &str = "anthropic_messages";
pub const BEDROCK_CONVERSE:   &str = "bedrock_converse";
pub const OPENAI_COMPLETIONS: &str = "openai_completions";
pub const OPENAI_RESPONSES:   &str = "openai_responses";
pub const VERTEX_GEMINI:      &str = "vertex_gemini";
// Anthropic thinking capabilities
pub const ADAPTIVE_THINKING:  &str = "adaptive_thinking";
pub const LEGACY_THINKING:    &str = "legacy_thinking";
```

`ChatFormat::tag()` returns the matching string, so "does this model accept this wire format?" is one `BTreeSet<String>::contains`. The catalog is a read-only trait object (`ModelCatalogHandle`) threaded through the request path as `Catalog<'a> = Option<&'a dyn ModelCatalogHandle>` — `agent_llm` therefore never depends on `agentgateway`.

The catalog itself is **fetched at runtime** from `https://agentgateway.dev/model-catalog`, validated, written to a local file, and hot-reloaded ([`llm/catalog/refresh.rs`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/agentgateway/src/llm/catalog/refresh.rs)); the repo also carries `catalog/model-catalog.json` plus `catalog/model-catalog-overrides.yaml`.

GitHub Copilot is the one provider that derives its supported formats from the catalog at request time ([`crates/llm/src/copilot.rs`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/llm/src/copilot.rs)), falling back to `OpenAICompletions` when the model is unknown.

## Providers

`AIProvider` in the published config schema is a tagged union of eight: `openAI`, `gemini`, `vertex`, `anthropic`, `bedrock`, `azure`, `copilot`, `custom`. On top of that, `ProviderPreset` names 13 pre-wired OpenAI-compatible endpoints: `cohere`, `ollama`, `baseten`, `cerebras`, `deepinfra`, `deepseek`, `groq`, `huggingface`, `mistral`, `openrouter`, `togetherai`, `xai`, `fireworks`.

## Virtual models: weighted, failover, conditional

A `ModelRoute` carries a `visibility` (`public` — requestable and listed in `/v1/models`; `internal` — only reachable through a virtual model). `VirtualModelRouting` is a three-way enum:

```rust
pub enum VirtualModelRouting {
	Weighted(Vec<WeightedTarget>),         // weight per target
	Failover { backend: RouteBackendReference },
	Conditional(Vec<ConditionalTarget>),   // `when: Option<Arc<cel::Expression>>`
}
```

The failover test fixture shows the shape ([`types/local_tests/llm_virtual_model_failover_config.yaml`](https://github.com/agentgateway/agentgateway/blob/35ab209d559665856228f33ecfdf170b6f46f482/crates/agentgateway/src/types/local_tests/llm_virtual_model_failover_config.yaml)) — **priority tiers**, with equal priorities forming a load-balanced group:

```yaml
llm:
  models:
  - name: openai/gpt-5-mini
    provider: openAI
    params: { model: gpt-5-mini, apiKey: sk-openai }
  - name: anthropic/*
    visibility: internal
    provider: anthropic
    params: { apiKey: sk-anthropic }
    transformation:
      model: llmRequest.model.stripPrefix("anthropic/")
  virtualModels:
  - name: resilient
    routing:
      failover:
        targets:
        - { model: openai/gpt-5-mini, priority: 0 }
        - { model: anthropic/haiku,   priority: 1 }
        - { model: anthropic/sonnet,  priority: 1 }
```

Note the wildcard model name `anthropic/*` plus a CEL `transformation.model` that strips the prefix before the request goes upstream — a declarative equivalent of shunt's `[[models]]` `upstream_model` map.

## Streaming without buffering

`crates/llm/src/parse/passthrough.rs` defines `PassthroughBody<D, F>`: it wraps the upstream `Body` and runs a `tokio_util::codec::Decoder` over a **separate copy** of the bytes while the original frames (trailers included) are forwarded unchanged and in order:

```rust
// Safe: parsing consumes a separate copy of the data; original frames,
// including trailers, are forwarded unchanged and in order.
body.dangerous_wrap_stream_preserving_content(...)
```

That is the structural answer to "observe SSE for usage/guardrails without buffering it" — the same constraint AGENTS.md states for shunt ("do not buffer upstream SSE responses unless the client requested non-streaming output"). Companion modules: `parse/sse.rs`, `parse/aws_sse.rs` (Bedrock event-stream), `parse/transform.rs`.

## Model-id path-segment safety

`crates/llm/src/lib.rs` carries a `model_path` module whose doc comment is worth quoting, because shunt has been bitten by exactly this class (validator and resolver normalizing a model id differently):

> A model id is interpolated into a single-segment slot of an upstream path (`.../models/{model}:generateContent`), so a `/` in one is either a resource-style name (`models/x`, `tunedModels/x`, a Bedrock inference-profile ARN) or an attempt to choose the upstream path. **Deciding that here keeps extraction and every path builder on the same answer.**

`is_safe_segment` rejects empty, `.`, `..`, and any of ``/ \ % ? # < > " ` { } | ^``, plus control and whitespace characters; `is_safe_resource_name` splits on `/` and requires every segment safe. The unit tests explicitly cover `gemini-2.5-flash?alt=sse` and `gemini-2.5-flash/../../locations/...`.

## Guardrails and policy

`llm/policy/` implements a layered guard system: regex rules and named builtins, a PII recognizer suite (credit card, US SSN, CA SIN, email, phone, URL — Presidio-shaped), OpenAI moderation, AWS Bedrock Guardrails, Google Model Armor, Azure Content Safety, arbitrary webhooks, and `streaming_guardrails.rs` for in-stream enforcement. `ContentScope` lets a guard target `SystemPrompt`, `Messages`, `ToolOutput`, or `ToolInput`, with a documented caveat that rewriting opaque Completions tool arguments can produce invalid JSON.

Failure modes are explicit and named in the schema (`failOpen` / `failClosed`), including on the remote rate-limit service.

## LiteLLM config importer

`crates/agentgateway/src/import.rs` defines a `ConfigImporter` trait normalizing a foreign gateway config into a source-neutral `ImportPlan { providers, models, routes, findings }`, with LiteLLM as the first (and currently only) adapter — `crates/agentgateway/src/tests/import/litellm/` holds snapshot fixtures including `single-model-fallback.snap`. The `findings` vector is the interesting design bit: the importer reports what it could not translate rather than silently dropping it.

## Testing approach

`crates/llm/src/golden_tests.rs` (1,741 lines) drives fixture JSON through each translation and snapshots the result **per provider** — snapshot name is `{fixture_stem}.{provider}` across ~16 provider labels (`anthropic`, `bedrock`, `bedrock-openai`, `bedrock-titan`, `bedrock-cohere`, `bedrock-cohere-v4`, `bedrock-nova`, `vertex`, `vertex-gemini`, `openai`, `gemini`, `gemini-native`, `cohere`, `responses`, `completions`, `vertex-embed-content`). One fixture therefore proves the whole row of the matrix at once.

## Credentials: no subscription OAuth

`BackendAuth` in the config schema covers JWT passthrough, static keys (file references are watched and hot-reloaded), `AwsAuth`, `GcpAuth`, `AzureAuth`, `JwtSignAuth`, `OAuthTokenExchangeAuth`, `CrossAppAccessAuth`, and a pluggable `CredentialProvider` selected by credential-URI authority (e.g. `kubernetes.io`).

`llm/mod.rs` does recognise Anthropic OAuth access tokens — it checks for the `sk-ant-oat*` prefix and, when present, keeps `Authorization: Bearer` and drops `x-api-key`:

```rust
if authz.token().starts_with(anthropic::OAUTH_TOKEN_PREFIX) || explicit_authorization {
    // OAuth tokens ("sk-ant-oat*") keep Authorization: Bearer; drop any x-api-key.
```

That is **forwarding a token the operator supplies**, not minting one. Searching `crates/llm/src` and `crates/agentgateway/src/llm` turned up no device/browser login flow, no refresh-token rotation, and no multi-account credential pool. Subscription-credential pooling remains shunt's differentiator, not a gap agentgateway has already closed.

## What this means for shunt

Ideas worth stealing, each already load-bearing in a shipped Rust gateway:

1. **An ordered translation table with the gaps written down.** shunt's translation choices are spread across `src/adapters/` and `src/model/`; a single const table of (inbound format, upstream format) pairs would make "which pairs exist" greppable and make a missing pair a comment instead of a runtime surprise.
2. **Capability tags instead of provider identity.** Deciding "can this model take Anthropic Messages / adaptive thinking?" from a per-model tag set, rather than from the provider enum, is what lets agentgateway support `custom` providers and Copilot without hardcoding. Compare shunt's `auto_include_builtin_models` problem — the catalog differs per credential.
3. **`is_safe_segment` as a shared oracle.** One predicate that extraction *and* every path builder call is precisely the "validate and resolve must normalize alike" fix, applied preventively.
4. **Snapshot-per-provider golden tests.** One fixture, N snapshots — cheap coverage of a translation matrix, and a new provider is a new snapshot column rather than a new test file.
5. **A config importer that reports findings.** If shunt ever wants a LiteLLM/agentgateway on-ramp, the `ImportPlan { ..., findings }` shape is the right contract.

Where shunt is ahead or simply different: shunt is **Anthropic-Messages-first** (gateway-owned errors keep the Anthropic error shape, except on the inbound Codex endpoint), it owns **subscription OAuth login, refresh rotation, and quota-aware multi-account pooling**, and it targets the Claude Code / Codex CLI clients specifically — including native Codex WebSocket transport. Agentgateway is OpenAI-compatible-first and operator-credential-shaped.

## Related

- [theagentrouter/agent-router](./005-theagentrouteragent-router-the-renamed-envoy-ai-ga.md) — the other gateway surveyed in the same pass.
