# shunt

Offline credential import: `shunt import opencodex --dry-run` previews compatible API keys and Cursor/Command Code access tokens. Exports create new private snapshots without changing existing settings or copying refresh tokens. See [credential import](docs/credential-import.md).

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

**English** · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md)

> Shunt Claude Code to any model.

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — everything else passes through to Anthropic unchanged (the "shunt"; the fallback is configurable via `server.default_provider`).

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped model's inference is diverted to another provider while Claude Code's tools and skills stay intact.

Providers for OpenAI, ChatGPT/Codex, xAI, Grok, Cursor, Kimi Code, Zhipu, MiniMax China, Gemini, Antigravity, and Anthropic passthrough ship built in — several of them reusing a subscription you already pay for. Any Anthropic-Messages-compatible backend is one config table away, with no code changes. See [Providers](#providers).

> [!NOTE]
> `shunt` is pre-1.0 software under active development. Per [SemVer](https://semver.org/#spec), `0.x` releases may include breaking changes to configuration keys, the CLI, and behavior — check the [release notes](https://github.com/pleaseai/shunt/releases) before upgrading.

## Install

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo, directly from the source repository
cargo install --git https://github.com/pleaseai/shunt
```

New versions are distributed through Homebrew and prebuilt binaries (macOS/Linux, arm64/x64) attached to each [GitHub release](https://github.com/pleaseai/shunt/releases); the crates.io package stops at the last version published there. See [Installation](https://shunt.dev/getting-started/installation/) for prebuilt-binary and from-source instructions.

### Run as a service (macOS/Homebrew)

```bash
brew services start shunt
```

Logs go to `$(brew --prefix)/var/log/shunt.log`. `brew services stop` sends `SIGTERM`, and shunt
drains in-flight requests before exiting; on Unix, Antigravity agent turns are terminated when
shutdown starts so their isolated process groups cannot hold the drain open. Editing the config file
afterwards doesn't need a restart — it [hot-reloads](docs/config-reload.md) automatically. Details:
[Running as a service](docs/running.md#run-as-a-background-service-homebrew).

## Quickstart

```toml
# shunt.toml — route a gpt-* id to your ChatGPT subscription
# [[routes]] is legacy for exact ids; prefer [models.upstream_model].
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

Unmapped models (all your `claude-*` ids) keep working exactly as before — shunt forwards them to Anthropic with your own credential. Full walkthrough: [Quickstart](https://shunt.dev/getting-started/quickstart/).

### Starter configuration

`shunt init` creates a commented `shunt.toml` in an existing directory. Keep the default passthrough starter, or scaffold ordered upstream presets without changing the fallback for unmapped models:

```bash
shunt init
shunt init --upstream codex --upstream kimi
```

### Agent-native setup blueprints

`shunt add` retrieves embedded Markdown implementation guides for coding agents. List the available upstream blueprints with `shunt add upstream`, or pipe one directly into an agent:

```bash
shunt add upstream kimi --print | claude
shunt add upstream https://provider.example/docs --print | claude
```

The command is offline and read-only: it prints guidance but never edits files, installs anything, or accesses the network. Use `shunt add provider <absolute-url>` when contributing support for a genuinely new provider protocol.

## Providers

A provider is either an ordered `[[upstreams]]` entry or a legacy `[providers.<name>]` TOML table (under YAML, an entry in the corresponding sequence or mapping). Two adapter kinds cover most upstreams: `kind = "anthropic"` (the upstream speaks Anthropic Messages; passed through, optionally with a different key) and `kind = "responses"` (the upstream speaks the OpenAI Responses API; shunt translates Anthropic Messages ⇄ Responses, streaming included). A third native kind, `kind = "cursor"`, bridges Cursor's ConnectRPC/protobuf AgentService so a Cursor subscription is reachable through the same Anthropic-Messages interface.

Ordered upstreams enable cross-provider failover. Declaration order is the attempt order; a model's `upstream_model` map selects the participating entries and maps its public id to each backend's id:

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic" # preset: kind, base_url, and default auth
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "codex-fallback"
provider = "codex" # defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
codex-fallback = "gpt-5.6-sol"
```

This chain tries `anthropic-primary` and then `codex-fallback`. `auth` accepts either a mode string or a map; `claude_oauth` and `chatgpt_oauth` maps can narrow credentials with `account = "name"` or `accounts = [...]`. Legacy `[providers.<name>]` remains supported and becomes implicit name-sorted upstreams. Do not declare both forms: mixing `[[upstreams]]` with `[providers.*]` is a configuration error. See the [configuration reference](https://shunt.dev/reference/configuration/) for presets, failure classes, and migration details.

### Built in

These providers are seeded by default, so `provider = "<name>"` routes to them with no `[providers.*]` table of your own — **but only while you declare no `[[upstreams]]`**. An ordered `[[upstreams]]` list replaces the provider map entirely, so under that form every provider you route to must be declared there, presets included:

| Name | Kind | Auth | Backend |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough or Claude OAuth account pool | `api.anthropic.com` — forwards the caller's credential by default; `auth = "claude_oauth"` enables pooled subscription credentials |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — reuses `~/.codex/auth.json` (`codex login`) |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — the developer API, billed per token |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — the Grok CLI proxy; reuses `~/.shunt/xai-auth.json` (`shunt login xai` with a SuperGrok / X Premium+ subscription) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`) |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` — Google Code Assist backend; reuses `~/.gemini/oauth_creds.json` |
| `antigravity` | `antigravity` | Antigravity OAuth | `daily-cloudcode-pa.googleapis.com` — Google Antigravity backend over HTTP; uses `~/.shunt/antigravity-auth.json` (`shunt login antigravity`) |
| `antigravity-cli` | `antigravity_cli` | None (local CLI) | **Deprecated.** Local `agy` binary — same backend via subprocess; superseded by `antigravity` above |

Ordered `[[upstreams]]` entries additionally accept the presets `kimi`, `kimi-code`, `zhipu`, and `minimax-cn`, which fill in `kind`, `base_url`, and the default auth for those backends.

Per-provider setup, model ids, and caveats live under [Providers](https://shunt.dev/guides/providers/) — including xAI's OAuth tier gate ([xAI / Grok](https://shunt.dev/guides/xai/)), Cursor's agent-mode prefixes ([Cursor](https://shunt.dev/providers/cursor/)), and Antigravity's two transports and the `kind = "antigravity"` migration ([Antigravity](https://shunt.dev/providers/antigravity/)).

> [!WARNING]
> `antigravity-cli` is deprecated and is **arbitrary code execution**: it runs the local `agy` binary agentically with `--dangerously-skip-permissions`, as the user running shunt. Keep its `sandbox` setting on, keep the bind on loopback, and prefer the `antigravity` provider, which needs none of this. See [the deprecated transport](https://shunt.dev/guides/providers/#the-deprecated-antigravity-cli-transport).

### Any Anthropic-compatible backend

One table, no code changes:

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| Kimi Code (subscription, OAuth) | `https://api.kimi.com/coding` | use the ids your subscription exposes |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| Zhipu (GLM China) | `https://open.bigmodel.cn/api/anthropic` | `glm-5.3`, `glm-5.3-flash` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| MiniMax China | `https://api.minimax.cn/anthropic` | `MiniMax-M3` |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"
```

Every row above but one takes `auth = "api_key"`. **Kimi Code** is the exception: a separate, subscription-billed service from the metered Moonshot API — different host, OAuth instead of an API key, and a built-in `kimi-code` preset. That preset resolves only inside an ordered `[[upstreams]]` entry, so declare it there (it is not in the seeded provider map) and log in. See [Kimi Code](https://shunt.dev/providers/kimi/#kimi-code-oauth-subscription).

### Reusing a subscription

OpenAI's Thibault Sottiaux has publicly welcomed running Codex through other coding harnesses:

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([Source](https://x.com/thsottiaux/status/2075830097488249060))

He [followed up](https://x.com/thsottiaux/status/2076119366647894371) by walking through pointing Claude Code ("your orange crab") at GPT-5.6 Sol himself — exactly the inference-layer swap `shunt` performs, no separate app required.

That said, reusing your ChatGPT/Codex or SuperGrok subscription (or Kimi, Cursor, or other backends) from an unofficial client is your own call — a public welcome doesn't guarantee future policy or account enforcement. Use at your own risk.

**Antigravity is the exception where the terms are explicit.** Google's [Antigravity terms](https://antigravity.google/terms) state that "using third party software, tools, or services to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach of this Agreement" and that such a breach "may be grounds for suspension or termination of your Antigravity and/or Gemini CLI accounts". shunt's `antigravity` provider is exactly that — third-party software using Antigravity OAuth — so routing through it falls squarely under that clause. Decide with that in mind before running `shunt login antigravity`.

## Optional server features

Unless a row says otherwise, these are **off by default** — absent its config table, the feature registers no routes and starts no background work.

| Feature | Enable with | Documentation |
| :-- | :-- | :-- |
| Anthropic multi-account pooling — sticky sessions, quota-aware rotation, predictive avoidance | `auth = "claude_oauth"` with two or more accounts; `[server.pool]` is optional tuning | [How-to](https://shunt.dev/guides/anthropic-multi-account/) |
| Codex multi-account pooling — `x-codex-*` window tracking, slow-start ramp, re-probing | `auth = "chatgpt_oauth"` with two or more accounts; `[server.pool]` is optional tuning | [How-to](https://shunt.dev/guides/codex-multi-account/) |
| Inbound Codex endpoint — point the **Codex CLI** at shunt and pool it, with opt-in per-model routing | `[server.codex_endpoint]` | [How-to](https://shunt.dev/guides/inbound-codex-endpoint/) |
| Claude apps gateway login — OAuth device flow, managed settings, per-user policy | `[server.gateway]` with `public_url`, a 32-byte-or-longer JWT secret, and static users or `[server.gateway.oidc]` | [How-to](https://shunt.dev/guides/gateway-login/) |
| Gateway telemetry ingest — verbatim OTLP relay for managed clients | a configured `[server.gateway]`, plus `[server.gateway.telemetry]` with a non-empty `forward_to` | [Reference](https://shunt.dev/reference/configuration/#servergatewaytelemetry-optional) |
| Admin web surface — accounts and usage dashboard, browser provisioning | `[server.admin]`, `shunt dashboard setup` | [How-to](https://shunt.dev/guides/admin-remote-provisioning/) |
| Spend-limit Admin API — organization- and user-scoped caps (stage 1 stores, does not enforce) | `[server.admin]` + `[server.spend]` | [Reference](https://shunt.dev/reference/configuration/#serverspend-optional) |
| Client usage endpoint — sanitized, aggregated pool headroom at `GET /usage` | `[server.auth]` + `[server.usage]` | [Reference](https://shunt.dev/reference/configuration/#serverusage-optional) |
| Claude Code CLI native usage bars — serves `GET /api/oauth/usage` | `[server.oauth_usage]`, plus `[server.auth]` or `[server.gateway]` on a non-loopback bind | [Reference](https://shunt.dev/reference/configuration/#serveroauth_usage-optional) |
| Upstream status polling — Statuspage indicators in the dashboard and as a metric | `[server.status]` with at least one `[[server.status.sources]]` entry | [Reference](https://shunt.dev/reference/configuration/#serverstatus-optional) |
| Bounded upstream retry — **on by default**, conservative, never mid-stream | `[providers.<name>.retry]` | [Reference](https://shunt.dev/reference/configuration/#providersnameretry) |
| Shared-deployment limits — **on by default** (1024 concurrent, 32 MiB bodies, 120 s TTFB, device-flow rate limits); CIDR, header, and URL limits are opt-in | `[server] max_concurrent_requests`, `[server.access_control]`, `[server.limits]`, `[server.timeouts]`, `[server.rate_limits]` | [How-to](https://shunt.dev/guides/shared-gateway/) |
| Secret references — `${VAR}` or `${file:/abs/path}` in any string value, re-resolved on hot reload (not `[sentry]`/`[otel]`, built once at startup — rotating those needs a restart) | any config string (**always on**) | [Reference](https://shunt.dev/reference/configuration/) |
| OpenTelemetry metrics and traces | `[otel]` with a non-empty `endpoint` | [How-to](https://shunt.dev/guides/opentelemetry/) |

## Documentation

Everything for users lives at **[shunt.dev](https://shunt.dev)**:

- [Quickstart](https://shunt.dev/getting-started/quickstart/) · [Why shunt?](https://shunt.dev/getting-started/why-shunt/) · [Providers](https://shunt.dev/guides/providers/) · [Configuration](https://shunt.dev/guides/configuration/) · [Troubleshooting](https://shunt.dev/reference/troubleshooting/)
- **For agents:** every page has a Markdown twin (append `.md` to any URL, or use the page's *Copy Markdown* / *Open in AI* buttons), and the site publishes [`/llms.txt`](https://shunt.dev/llms.txt), [`/llms-small.txt`](https://shunt.dev/llms-small.txt), and [`/llms-full.txt`](https://shunt.dev/llms-full.txt) per the [llms.txt spec](https://llmstxt.org/).

Design notes and milestone specs for contributors live in [`docs/`](docs/) — start with [`docs/implementation-plan.md`](docs/implementation-plan.md).

## Why

Claude Code sends every turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the models you map, diverts their inference to another provider (OpenAI, Codex/ChatGPT, …). Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the session keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast with the alternative approach (handing a `subagent_type` off to another runtime like Codex CLI), which cuts higher in the stack and drops persona and preloaded skills.

### Per-model, not per-agent — and not a global swap

Selectivity is driven by the **`model` id on each request**, which Claude Code already lets you choose per context: the `/model` picker for the main session, a subagent definition's `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents, or `ANTHROPIC_CUSTOM_MODEL_OPTION` to add a custom entry to the picker. So "divert only this agent / this session" is decided in Claude Code, and shunt just honors the model id it receives — no fragile per-agent system-prompt fingerprinting. Unlike global model-swap proxies, the main session can stay on Claude while only the models you name divert.

## Claude Code integration (official surface)

Claude Code exposes a **first-class gateway contract** behind `ANTHROPIC_BASE_URL` — `shunt` implements this rather than the fragile "hash the subagent's system prompt" heuristic that earlier Claude Code proxies rely on.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — the API contract: endpoints, headers and body fields to forward vs consume, feature pass-through, and attribution. A running gateway serves the machine-readable spec at `GET /protocol`. Claude Code prepends a client-version and conversation fingerprint to the system prompt; shunt forwards that attribution block unchanged, since suppressing it is the developer's call via `CLAUDE_CODE_ATTRIBUTION_HEADER=0`.
- [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code queries `GET /v1/models?limit=1000` at startup (opt-in via `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and adds returned models to the `/model` picker. shunt answers with curated `[[models]]` entries plus, while `auto_include_builtin_models` stays `true`, the caller's own live catalog — fetched only when `server.default_provider` is Anthropic-kind, and falling back to a built-in snapshot when it isn't, when no credential is available, or when the fetch fails. **Constraint:** entries whose `id` doesn't begin with `claude`/`anthropic` are ignored — non-Claude models must be aliased or added manually. See [Model discovery](https://shunt.dev/guides/model-discovery/).
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` adds a gateway-routed entry to the `/model` picker without replacing built-in aliases; the ID skips validation, so any string the gateway accepts works. **This is the primary way to select a non-Claude model** (e.g. `gpt-5.6-sol`), given the discovery constraint above.
- **Tool search** (`ENABLE_TOOL_SEARCH`) — Claude Code defers MCP/LSP tool schemas and reveals them on demand, reclaiming context. Because shunt isn't a first-party Anthropic host, Claude Code keeps this **off** unless you opt in. Whether deferral then survives depends on the upstream, not on a setting alone: `claude*` and `anthropic/*` ids keep the protocol byte-for-byte, other ids have their `defer_loading` markers stripped because those hosts reject them, and the Responses path has its own three-state `tool_search` setting. See [Tool search](https://shunt.dev/guides/codex/#tool-search).

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

### Code review

Pull requests to `shunt` are reviewed by two AI code reviewers, both free for open source:

- [Greptile](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source) — free for non-commercial MIT/Apache projects under its OSS program.
- [cubic](https://cubic.dev/) — free for public repositories.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
