# shunt

[![Socket Badge](https://socket.dev/api/badge/cargo/package/shunt-gateway)](https://socket.dev/cargo/package/shunt-gateway)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)

> Shunt Claude Code to any model.

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — everything else passes through to Anthropic unchanged (the "shunt").

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped model's inference is diverted to another provider while Claude Code's tools and skills stay intact.

**Phase 1 target:** OpenAI / Codex / ChatGPT — translate Anthropic Messages ⇄ the OpenAI Responses API, over an OpenAI API key or a reused ChatGPT (`codex login`) subscription.

**Status:** private, early. May be open-sourced later. See [`docs/running.md`](docs/running.md) to build, configure, and connect Claude Code, and [`docs/implementation-plan.md`](docs/implementation-plan.md) for the design and milestones.

## Why

Claude Code sends every turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the models you map, diverts their inference to another provider (OpenAI, Codex/ChatGPT, …). Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the session keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast with the alternative approach (handing a `subagent_type` off to another runtime like Codex CLI), which cuts higher in the stack and drops persona and preloaded skills.

### Per-model, not per-agent — and not a global swap

Selectivity is driven by the **`model` id on each request**, which Claude Code already lets you choose per context: the `/model` picker for the main session, a subagent definition's `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents, or `ANTHROPIC_CUSTOM_MODEL_OPTION` to add a custom entry to the picker. So "divert only this agent / this session" is decided in Claude Code, and shunt just honors the model id it receives — no fragile per-agent system-prompt fingerprinting. Unlike the global model-swap proxies below, the main session can stay on Claude while only the models you name divert.

## Related work / prior art

**Claude Code–specific routers & proxies**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) — the largest in this niche; use Claude Code as a foundation and decide how requests reach different models/providers.
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) — run Claude Code on OpenAI models.
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) — Claude Code → OpenAI API proxy.
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) — captures/visualizes in-flight Claude Code requests, with optional **per-agent** routing to other providers (the direct inspiration for `shunt`'s subagent-routing idea).
- [luohy15/y-router](https://github.com/luohy15/y-router) — a simple proxy enabling Claude Code to work with OpenRouter.
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) — Cloudflare Workers proxy translating Claude API requests to OpenAI format (Gemini, Groq, Ollama).
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) — use any model provider with Claude Code.
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) — cross-runtime router: proxies Anthropic **or** OpenAI API calls to the local **Claude Code or Codex** CLI (routes to the local CLI when the API key is all 9s, else the real cloud API). Note the inverse direction — routing cloud-API calls *to* local CLIs, rather than routing Claude Code agents *out* to cloud providers.
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) — Anthropic-compatible `/v1/messages` proxy that serves Claude Code inference from the **ChatGPT Codex backend** (uses a ChatGPT Plus/Pro subscription instead of an API key). Same inference-layer swap as `shunt`, targeting the Codex/GPT subscription backend while keeping Claude Code's UI and MCP tools.

**General AI gateways (adjacent infrastructure — possible backends)**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) — SDK + proxy/AI gateway calling 100+ LLM APIs in OpenAI format, with cost tracking, guardrails, load balancing.
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) — fast AI gateway routing to 1,600+ LLMs with integrated guardrails.
- [maximhq/bifrost](https://github.com/maximhq/bifrost) — high-performance AI gateway with adaptive load balancing and 1000+ model support.
- [mazori-ai/modelgate](https://github.com/mazori-ai/modelgate) — open-source LLM gateway + MCP server (Go): RBAC/policy enforcement, multi-provider (OpenAI, Anthropic, Gemini, Bedrock, Azure, and local Ollama), an MCP gateway with semantic tool search, and semantic response caching.

### How `shunt` differs

Most Claude Code proxies above route **all** traffic to one alternative provider (a global model swap). `shunt`'s focus is **selective, per-model** diversion driven by the request's `model` id: keep the main session on Claude, and shunt only the models you name onto other providers — the switchboard/patchbay use case. Because Claude Code already lets you bind a model per context (main session, subagent `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL`), that same selectivity reaches down to individual agents without shunt ever inspecting who the caller is.

## Claude Code integration (official surface)

Claude Code exposes a **first-class gateway contract** behind `ANTHROPIC_BASE_URL` — `shunt` should implement this rather than the fragile "hash the subagent's system prompt" heuristic that earlier Claude Code proxies rely on.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — the API contract: endpoints, headers/body fields to forward vs consume, feature pass-through, and attribution. A running gateway serves the machine-readable spec at `GET /protocol`.
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code queries `GET /v1/models?limit=1000` at startup (opt-in via `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and adds returned models to the `/model` picker. **Constraint:** entries whose `id` doesn't begin with `claude`/`anthropic` are ignored — non-Claude models must be aliased or added manually.
  - **System prompt attribution block** — Claude Code prepends a client-version + conversation fingerprint to the system prompt; stable for the conversation lifetime (v2.1.181+). `shunt` forwards it unchanged (never strips it — that's the developer's call via `CLAUDE_CODE_ATTRIBUTION_HEADER=0`).
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` adds a gateway-routed entry to the `/model` picker without replacing built-in aliases; the ID skips validation, so any string the gateway accepts works. **This is the primary way to select a non-Claude model** (e.g. `gpt-5.2-codex`), since discovery ignores ids that don't begin with `claude`/`anthropic`.

**Design implication for `shunt`:** be a spec-compliant Anthropic-Messages gateway (`/v1/messages`, `/v1/models`, correct header/attribution pass-through), route by the request's `model` id, and translate Anthropic Messages ⇄ the OpenAI Responses API for mapped models — no prompt-shape heuristics that break on every Claude Code prompt change. See [`docs/implementation-plan.md`](docs/implementation-plan.md).

## License

TBD (private for now).
