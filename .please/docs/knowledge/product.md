# Product Guide

> Auto-generated during `/please:setup` from `README.md` and `AGENTS.md`.
> Product vision, goals, and target users.

## Vision

> Shunt Claude Code to any model.

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol):
a transparent proxy that, for the **models you map**, diverts inference to another LLM
provider at the **inference layer** — while everything else passes through to Anthropic
unchanged. The name is the mechanism: an electrical/railway *shunt* diverts a selected part
of the flow onto a parallel path. A mapped model's inference is diverted to another provider
while Claude Code's tools and skills stay intact.

## Problem

Claude Code speaks the Anthropic Messages API. Users who want to run it against a different
model — a ChatGPT/Codex subscription they already pay for, xAI Grok, Cursor, a local or
third-party Anthropic-compatible endpoint — have no first-class path. Existing proxies
either break streaming, mangle tool use, or require rewriting each provider by hand.

## Solution

A single Rust binary that:

- Routes by the request's `model` id — mapped ids divert, the rest pass through to Anthropic
  (fallback configurable via `server.default_provider`)
- Translates Anthropic Messages ⇄ each provider's protocol (e.g. OpenAI Responses) while
  **preserving streaming semantics** and tool use
- Reuses existing subscriptions via OAuth (Codex, Grok, Cursor) instead of new API spend
- Adds any Anthropic-Messages-compatible backend through **one config table** — no code change
- Emits all gateway-owned errors in Anthropic error shape so Claude Code behaves normally

## Target Users

- **Claude Code users** who want to point it at a different or cheaper model without losing
  tools, skills, or streaming
- **ChatGPT/Codex, SuperGrok, Cursor subscribers** who want to reuse an existing subscription
  as the inference backend
- **Teams** running multiple provider accounts who need pooling + load balancing across them
- **Self-hosters** wiring Claude Code to local or third-party Anthropic-compatible endpoints
  (Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel AI Gateway, …)

## Core Capabilities

- Model-id routing (exact, prefix, default-provider) — `src/routing.rs`
- Provider adapters (OpenAI, ChatGPT/Codex incl. WebSocket v2, xAI, Grok, Cursor, Anthropic
  passthrough) — `src/adapters/`
- Anthropic ⇄ OpenAI Responses translation — `src/model/`
- OAuth login + credential refresh, multi-account pooling & load balancing — `src/auth/`
- `/v1/models` discovery, `/v1/messages` proxy, `count_tokens`
- Opt-in admin web surface for browser account provisioning + read-only pool dashboard
- Observability: tracing, opt-in OpenTelemetry export, Sentry

## Design Constraints

- **Transparency**: unmapped traffic must pass through to Anthropic unchanged
- **Streaming-first**: never buffer upstream SSE unless the client asked for non-streaming
- **Table-driven**: new providers/models arrive via config, not new branches in code
- **Anthropic-shaped errors**: gateway errors always match the Anthropic error contract
- **OSS, English-only**: public repo (`pleaseai/shunt`); docs and code are English-only

## Distribution

- Published to crates.io as `shunt-gateway` (binary + library `shunt`)
- Dual-licensed MIT OR Apache-2.0
- User docs site: Astro Starlight → Cloudflare Pages (`site/`)
