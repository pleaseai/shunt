---
title: Why shunt
description: What shunt is, how it differs from other Claude Code proxies, and when to use it.
---

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — everything else passes through to Anthropic unchanged (the "shunt").

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped model's inference is diverted to another provider while Claude Code's tools and skills stay intact.

## How it works

Claude Code sends every turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the models you map, diverts their inference to another provider (OpenAI, Codex/ChatGPT, …). Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the session keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast this with handing a subagent off to another runtime (like the Codex CLI), which cuts higher in the stack and drops persona and preloaded skills.

## Per-model, not per-agent — and not a global swap

Most Claude Code proxies route **all** traffic to one alternative provider (a global model swap). `shunt`'s focus is **selective, per-model** diversion driven by the request's `model` id: keep the main session on Claude, and shunt only the models you name onto other providers.

Selectivity is decided in Claude Code itself, which already lets you choose a model per context:

- the `/model` picker for the main session,
- a subagent definition's `model:` frontmatter,
- `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents,
- `ANTHROPIC_CUSTOM_MODEL_OPTION` to add a custom entry to the picker.

shunt just honors the model id it receives — no fragile per-agent system-prompt fingerprinting. That same selectivity reaches down to individual agents without shunt ever inspecting who the caller is.

## What shunt implements

- **`POST /v1/messages`** — inference, routed per the request's `model` id. Unmapped models are forwarded to Anthropic byte-for-byte with the caller's own credential.
- **Anthropic Messages ⇄ OpenAI Responses translation** — for mapped OpenAI-family models, including streaming.
- **ChatGPT subscription reuse** — the `codex` provider reuses (and auto-refreshes) the Codex CLI's `~/.codex/auth.json` login.
- **`GET /v1/models`** — [model discovery](/guides/model-discovery/) for Claude-named aliases.
- **Token counting** — local tiktoken counts for translated providers, exact upstream counts for passthrough.
- **Streaming resilience** — [SSE keepalive pings](/guides/shared-gateway/#sse-keepalive-pings) so proxies like Cloudflare don't kill long reasoning stretches.
- **Optional inbound auth** — [per-client tokens](/guides/shared-gateway/) for shared deployments.

Ready to try it? Head to [Installation](/getting-started/installation/).
