# shunt

[![Crates.io](https://img.shields.io/crates/v/shunt-gateway.svg)](https://crates.io/crates/shunt-gateway)
[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![Socket Badge](https://socket.dev/api/badge/cargo/package/shunt-gateway)](https://socket.dev/cargo/package/shunt-gateway)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/crates/l/shunt-gateway.svg)](#license)

**English** · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md)

> Shunt Claude Code to any model.

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — everything else passes through to Anthropic unchanged (the "shunt"; the fallback is configurable via `server.default_provider`).

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped model's inference is diverted to another provider while Claude Code's tools and skills stay intact.

It ships with **OpenAI**, **ChatGPT/Codex** (reuse your subscription via `codex login`), and **Anthropic** passthrough built in — and any Anthropic-Messages-compatible backend (Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel AI Gateway, …) is one TOML table away, no code changes.

## Install

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo — the crate is `shunt-gateway`; the binary is still `shunt`
cargo install shunt-gateway
```

Prebuilt binaries (macOS/Linux, arm64/x64) are attached to each [GitHub release](https://github.com/pleaseai/shunt/releases). See [Installation](https://shunt-docs.pages.dev/getting-started/installation/) for prebuilt-binary and from-source instructions.

## Quickstart

```toml
# shunt.toml — route a gpt-* id to your ChatGPT subscription
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"        # reuses `codex login`; use `openai` for OPENAI_API_KEY
```

```bash
codex login                                        # provider credential
shunt run                                           # -> listening on 127.0.0.1:3001

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
claude                                              # /model -> pick gpt-5.6-sol
```

Unmapped models (all your `claude-*` ids) keep working exactly as before — shunt forwards them to Anthropic with your own credential. Full walkthrough: [Quickstart](https://shunt-docs.pages.dev/getting-started/quickstart/).

## Providers

A provider is a `[providers.<name>]` TOML table — two adapter kinds cover everything: `kind = "anthropic"` (the upstream speaks Anthropic Messages; passed through, optionally with a different key) and `kind = "responses"` (the upstream speaks the OpenAI Responses API; shunt translates Anthropic Messages ⇄ Responses, streaming included).

**Built in:**

| Name | Kind | Auth | Backend |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough | `api.anthropic.com` — forwards the caller's own credential |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — reuses `~/.codex/auth.json` (`codex login`) |

OpenAI's Thibault Sottiaux has publicly welcomed running Codex through other coding harnesses:

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([Source](https://x.com/thsottiaux/status/2075830097488249060))

He [followed up](https://x.com/thsottiaux/status/2076119366647894371) by walking through pointing Claude Code ("your orange crab") at GPT-5.6 Sol himself — exactly the inference-layer swap `shunt` performs, no separate app required.

That said, reusing your ChatGPT/Codex subscription (or Kimi, Cursor, or other backends) from an unofficial client is your own call — a public welcome doesn't guarantee future policy or account enforcement. Use at your own risk.

**Any Anthropic-compatible backend** is one table away — no code changes:

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

See [Providers](https://shunt-docs.pages.dev/guides/providers/) for the full list and per-provider notes.

## Documentation

Everything lives at **[shunt-docs.pages.dev](https://shunt-docs.pages.dev)**:

- [Quickstart](https://shunt-docs.pages.dev/getting-started/quickstart/) · [Why shunt?](https://shunt-docs.pages.dev/getting-started/why-shunt/) · [Providers](https://shunt-docs.pages.dev/guides/providers/) · [Configuration](https://shunt-docs.pages.dev/guides/configuration/) · [Troubleshooting](https://shunt-docs.pages.dev/reference/troubleshooting/)
- **For agents:** every page has a Markdown twin (append `.md` to any URL, or use the page's *Copy Markdown* / *Open in AI* buttons), and the site publishes [`/llms.txt`](https://shunt-docs.pages.dev/llms.txt), [`/llms-small.txt`](https://shunt-docs.pages.dev/llms-small.txt), and [`/llms-full.txt`](https://shunt-docs.pages.dev/llms-full.txt) per the [llms.txt spec](https://llmstxt.org/).

Design notes and milestone specs live in [`docs/`](docs/) (start with [`docs/implementation-plan.md`](docs/implementation-plan.md)). To route Claude Code to your ChatGPT/Codex subscription, see the [Codex configuration reference](docs/codex-configuration.md).

## Why

Claude Code sends every turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the models you map, diverts their inference to another provider (OpenAI, Codex/ChatGPT, …). Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the session keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast with the alternative approach (handing a `subagent_type` off to another runtime like Codex CLI), which cuts higher in the stack and drops persona and preloaded skills.

### Per-model, not per-agent — and not a global swap

Selectivity is driven by the **`model` id on each request**, which Claude Code already lets you choose per context: the `/model` picker for the main session, a subagent definition's `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents, or `ANTHROPIC_CUSTOM_MODEL_OPTION` to add a custom entry to the picker. So "divert only this agent / this session" is decided in Claude Code, and shunt just honors the model id it receives — no fragile per-agent system-prompt fingerprinting. Unlike global model-swap proxies, the main session can stay on Claude while only the models you name divert.

## Claude Code integration (official surface)

Claude Code exposes a **first-class gateway contract** behind `ANTHROPIC_BASE_URL` — `shunt` implements this rather than the fragile "hash the subagent's system prompt" heuristic that earlier Claude Code proxies rely on.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — the API contract: endpoints, headers/body fields to forward vs consume, feature pass-through, and attribution. A running gateway serves the machine-readable spec at `GET /protocol`.
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code queries `GET /v1/models?limit=1000` at startup (opt-in via `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and adds returned models to the `/model` picker. **Constraint:** entries whose `id` doesn't begin with `claude`/`anthropic` are ignored — non-Claude models must be aliased or added manually.
  - **System prompt attribution block** — Claude Code prepends a client-version + conversation fingerprint to the system prompt; stable for the conversation lifetime (v2.1.181+). `shunt` forwards it unchanged (never strips it — that's the developer's call via `CLAUDE_CODE_ATTRIBUTION_HEADER=0`).
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` adds a gateway-routed entry to the `/model` picker without replacing built-in aliases; the ID skips validation, so any string the gateway accepts works. **This is the primary way to select a non-Claude model** (e.g. `gpt-5.6-sol`), since discovery ignores ids that don't begin with `claude`/`anthropic`.

**Design principle:** be a spec-compliant Anthropic-Messages gateway (`/v1/messages`, `/v1/models`, correct header/attribution pass-through), route by the request's `model` id, and translate Anthropic Messages ⇄ the OpenAI Responses API for mapped models — no prompt-shape heuristics that break on every Claude Code prompt change.

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

## Contributing

Issues and PRs are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`AGENTS.md`](AGENTS.md) for build/test commands and conventions, and [`SECURITY.md`](SECURITY.md) for reporting vulnerabilities.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
