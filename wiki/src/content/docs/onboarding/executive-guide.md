---
title: "Executive Guide"
description: "Capability, risk, and investment overview for engineering leaders."
---

## System Overview

shunt lets a Claude Code user keep the Claude Code workflow while selectively sending chosen model IDs to another model provider. This preserves developer productivity tooling while creating optionality across model vendors [README.md:1-60](https://github.com/chatbot-pf/shunt/blob/main/README.md#L1-L60) [docs/running.md:1-461](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L1-L461).

| Capability | Status | Maturity | Dependency | Source |
|---|---|---|---|---|
| Local gateway for Claude Code | Built | Working implementation | Axum/Rust binary | [src/server.rs:13-25](https://github.com/chatbot-pf/shunt/blob/main/src/server.rs#L13-L25) [src/main.rs:38-76](https://github.com/chatbot-pf/shunt/blob/main/src/main.rs#L38-L76) |
| Anthropic pass-through | Built | Tested | Anthropic-compatible upstream | [src/adapters/anthropic.rs:31-104](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/anthropic.rs#L31-L104) [tests/passthrough.rs:72-247](https://github.com/chatbot-pf/shunt/blob/main/tests/passthrough.rs#L72-L247) |
| OpenAI Responses translation | Built | Tested with fixtures | OpenAI Responses API shape | [src/adapters/responses.rs:34-213](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/responses.rs#L34-L213) [tests/responses_translate.rs:25-287](https://github.com/chatbot-pf/shunt/blob/main/tests/responses_translate.rs#L25-L287) |
| ChatGPT/Codex credential reuse | Built | Sensitive; needs operational care | `~/.codex/auth.json` | [src/auth/codex_auth.rs:34-63](https://github.com/chatbot-pf/shunt/blob/main/src/auth/codex_auth.rs#L34-L63) |
| Model discovery | Built | Limited by Claude Code ID rules | Claude Code gateway discovery | [src/discovery.rs:17-30](https://github.com/chatbot-pf/shunt/blob/main/src/discovery.rs#L17-L30) [docs/running.md:189-393](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L189-L393) |
| Production hardening | Partial | Roadmap item | Observability/timeouts/retries | [docs/implementation-plan.md:6-249](https://github.com/chatbot-pf/shunt/blob/main/docs/implementation-plan.md#L6-L249) |

```mermaid
graph LR
    Dev[Developer] --> Claude[Claude Code]
    Claude --> Shunt[Local shunt gateway]
    Shunt --> Anthropic[Anthropic]
    Shunt --> OpenAI[OpenAI]
    Shunt --> ChatGPT[ChatGPT Codex]
    classDef dark fill:#2d333b,stroke:#6d5dfc,color:#e6edf3;
    class Dev,Claude,Shunt,Anthropic,OpenAI,ChatGPT dark;
    linkStyle default stroke:#8b949e;
```
<!-- Sources: README.md:4, docs/running.md:6, src/server.rs:13, src/adapters/responses.rs:34 -->

## Technology Investment Thesis

| Technology | Purpose | Risk level | Investment view | Source |
|---|---|---|---|---|
| Rust + Tokio | Reliable local network service | Low | Good fit for streaming gateway | [Cargo.toml:1-23](https://github.com/chatbot-pf/shunt/blob/main/Cargo.toml#L1-L23) |
| Axum | HTTP routing and serving | Low | Simple implementation surface | [src/server.rs:13-25](https://github.com/chatbot-pf/shunt/blob/main/src/server.rs#L13-L25) |
| Reqwest streaming | Upstream calls and SSE relay | Medium | Critical for responsiveness | [src/adapters/anthropic.rs:31-104](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/anthropic.rs#L31-L104) $resp_stream |
| Figment TOML/env | Operator configuration | Low | Reduces code churn for providers | [src/config.rs:185-194](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L185-L194) |
| Token-file reuse | Fast setup for Codex/ChatGPT | Medium | Practical but security-sensitive | [src/auth/codex_auth.rs:34-63](https://github.com/chatbot-pf/shunt/blob/main/src/auth/codex_auth.rs#L34-L63) [SECURITY.md:1-38](https://github.com/chatbot-pf/shunt/blob/main/SECURITY.md#L1-L38) |

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation | Owner |
|---|---|---|---|---|
| Provider API shape changes | Medium | High | Keep translation tests and fixtures current | Engineering |
| Credential refresh failure | Medium | Medium | Surface clear `codex login` errors and prefer setup tokens where possible | Engineering/Users |
| Streaming regression | Low | High | Wiremock and SSE state machine tests | Engineering |
| Discovery confusion | Medium | Low | Document custom model option and aliases | Developer Experience |
| Private early status | High | Medium | Keep docs explicit and avoid over-promising | Maintainers |

```mermaid
graph TB
    Shunt[shunt] --> Anthropic[Anthropic-compatible providers]
    Shunt --> OpenAI[OpenAI Platform]
    Shunt --> Codex[ChatGPT Codex backend]
    Shunt --> Files[Local credential files]
    Files --> Risk[Credential handling risk]
    classDef dark fill:#2d333b,stroke:#6d5dfc,color:#e6edf3;
    class Shunt,Anthropic,OpenAI,Codex,Files,Risk dark;
    linkStyle default stroke:#8b949e;
```
<!-- Sources: src/config.rs:142, src/auth/codex_auth.rs:34, src/auth/claude_auth.rs:27, SECURITY.md:1 -->

## Cost and Scaling Model

| Driver | Cost behavior | Current bottleneck | Source |
|---|---|---|---|
| Local CPU/memory | Minimal; proxy and translation only | JSON/SSE transformation in process | [src/model/responses_request.rs:4-280](https://github.com/chatbot-pf/shunt/blob/main/src/model/responses_request.rs#L4-L280) [src/model/responses.rs:45-378](https://github.com/chatbot-pf/shunt/blob/main/src/model/responses.rs#L45-L378) |
| Upstream inference | Scales with selected provider usage | Provider account limits and entitlements | [docs/running.md:189-393](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L189-L393) |
| Developer operations | One local process per user or environment | Credential setup and config correctness | [docs/running.md:1-461](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L1-L461) |

## Roadmap Alignment

```mermaid
flowchart LR
    M0[M0 pass-through] --> M1[M1 Responses translation]
    M1 --> M2[M2 ChatGPT OAuth]
    M2 --> M3[M3 discovery UX]
    M3 --> M4[M4 hardening]
    classDef dark fill:#2d333b,stroke:#6d5dfc,color:#e6edf3;
    class M0,M1,M2,M3,M4 dark;
    linkStyle default stroke:#8b949e;
```
<!-- Sources: docs/implementation-plan.md:238, docs/implementation-plan.md:240, docs/implementation-plan.md:243, docs/implementation-plan.md:244, docs/implementation-plan.md:245, docs/implementation-plan.md:246 -->

## Recommendations

| Priority | Recommendation | Expected impact | Source |
|---|---|---|---|
| 1 | Keep translation tests as release gate | Protects core compatibility | [tests/responses_translate.rs:25-287](https://github.com/chatbot-pf/shunt/blob/main/tests/responses_translate.rs#L25-L287) [.github/workflows/ci.yml:1-42](https://github.com/chatbot-pf/shunt/blob/main/.github/workflows/ci.yml#L1-L42) |
| 2 | Document supported provider/model combinations continuously | Reduces setup failures | [docs/running.md:1-461](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L1-L461) [shunt.toml.example:1-134](https://github.com/chatbot-pf/shunt/blob/main/shunt.toml.example#L1-L134) |
| 3 | Add production-hardening backlog around timeouts/retries/observability | Improves reliability if shared beyond local use | [docs/implementation-plan.md:6-249](https://github.com/chatbot-pf/shunt/blob/main/docs/implementation-plan.md#L6-L249) |
| 4 | Treat credential-file refresh as a security-sensitive area | Reduces incident risk | [SECURITY.md:1-38](https://github.com/chatbot-pf/shunt/blob/main/SECURITY.md#L1-L38) [src/auth/codex_auth.rs:34-63](https://github.com/chatbot-pf/shunt/blob/main/src/auth/codex_auth.rs#L34-L63) |

## Related Pages

| Page | Relationship |
|---|---|
| [Product Manager Guide](./product-manager-guide.md) | Product-facing explanation |
| [Staff Engineer Guide](./staff-engineer-guide.md) | Architectural details |
| [Operations](../01-getting-started/operations.md) | Current operating model |
