---
title: Why shunt
description: What shunt is, how it differs from other Claude Code proxies, and when to use it.
---

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — by default, everything else passes through to Anthropic unchanged (the "shunt"; the fallback is configurable via `server.default_provider`).

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

One model id can opt into deciding for itself: a [stage router](/guides/stage-router/) names a capable and an efficient target and picks between them per turn from the conversation's recent tool-result metadata — `tool_use.name` and `tool_result.is_error`, never prompt text. Any entry can also carry a [`[models.subagents]`](/reference/configuration/#modelssubagents-optional) overlay that sends delegated work to a different target while the parent session keeps its own. Configure neither and nothing changes.

## Routing built on NVIDIA Switchyard

The stage router and the judge-backed routers are not heuristics shunt invented. They build on [`switchyard-libsy`](https://github.com/NVIDIA-NeMo/Switchyard/tree/3ddea9d30174ad835cf505a93617ba251c8eb8dd/crates/libsy), the routing library from NVIDIA's [Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) project, which routes "each LLM call to the cheapest model that can still do the job." That gives you:

- **A scorer with published results.** shunt runs Switchyard's stage scorer unmodified. In upstream's Terminal-Bench 2.1 run, stage routing kept 95.7% of an Opus 4.8 baseline's accuracy at 30.5% lower cost, while each of the four fixed models in the comparison scored below 56%. These are upstream's numbers, not shunt's: shunt has not run the benchmark, and it extracts signals its own way. Read them as evidence that the approach pays, not as a promise for the model pair you configure.
- **More than one algorithm.** Beyond stage scoring, shunt carries Switchyard's other algorithms, each opt-in per model entry. An entry picks one router type — such as `stage_router` (which can add handoff notes), `llm_classifier` (whose `escalation` mode is escalation routing), `composite`, or `advisor` — and sub-agent routing is configured separately, in a `[models.subagents]` table.
- **Fitted to Claude Code sessions.** Switchyard is provider-neutral. shunt adds what a Claude Code gateway needs: it matches Claude Code's own tool names, pins each session's tier with asymmetric hysteresis so a long session does not flip tiers turn by turn and lose its warm prompt cache, and keeps a delegated subagent's routing apart from its parent's.
- **Targets stay ordinary model ids.** A routed target re-enters shunt's normal routing, so it keeps its own failover chain, account pool, and adapter, and the client still sees the model id it asked for.
- **Little to carry.** The dependency added about 0.3% to the release binary, and it is pinned to a reviewed upstream revision, so an upgrade is a reviewed diff. An entry with neither a `[models.router]` nor a `[models.subagents]` table routes exactly as before.
- **No second server.** Switchyard's [server path](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/getting_started.md#server-path) runs `switchyard-server` as a standalone proxy. For clients that speak Anthropic Messages, such as Claude Code, the same route types — `auto`, `stage_router`, and `llm_classifier` — are `[models.router]` types in shunt, so there is no second proxy to run. shunt applies routers only to Anthropic Messages requests (`/v1/messages` and its `/v1/messages/count_tokens` probe), so routing for OpenAI Chat Completions or Responses clients is outside what it covers.

[Switchyard Integration](/guides/switchyard/) covers what shunt takes from Switchyard and what it leaves out, the benchmark caveats, and the algorithm step by step.

## What shunt implements

- **`POST /v1/messages`** — inference, routed per the request's `model` id. Unmapped models are forwarded to Anthropic byte-for-byte with the caller's own credential, except for a [`thinking` signature shunt minted for another provider](/providers/anthropic/) — a value Anthropic would reject.
- **Anthropic Messages ⇄ OpenAI Responses translation** — for mapped OpenAI-family models, including streaming.
- **ChatGPT subscription reuse** — the `codex` provider reuses (and auto-refreshes) the Codex CLI's `~/.codex/auth.json` login.
- **`GET /v1/models`** — [model discovery](/guides/model-discovery/) for Claude-named aliases.
- **Token counting** — local tiktoken counts for translated providers, exact upstream counts for passthrough.
- **Streaming resilience** — [SSE keepalive pings](/guides/shared-gateway/#sse-keepalive-pings) so proxies like Cloudflare don't kill long reasoning stretches.
- **Optional inbound auth** — [per-client tokens](/guides/shared-gateway/) for shared deployments.

Ready to try it? Head to [Installation](/getting-started/installation/).
