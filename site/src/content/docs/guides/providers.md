---
title: Providers
description: The built-in providers and how to add any Anthropic-compatible backend with a TOML table.
---

Providers are a **name → config map**: a new upstream is just another `[providers.<name>]` table — no code change. Three adapter kinds cover everything:

- **`kind = "anthropic"`** — the upstream speaks the Anthropic Messages API. shunt passes the request through, optionally injecting a different API key.
- **`kind = "responses"`** — the upstream speaks the OpenAI Responses API. shunt translates Anthropic Messages ⇄ Responses, including streaming.
- **`kind = "cursor"`** — the native Cursor adapter. shunt bridges Cursor's ConnectRPC/protobuf AgentService to the Anthropic Messages API — streaming, reasoning in streaming responses, native tool calls, and inline images included. Used by the built-in `cursor` provider.

## Built-in providers

| Name | Kind | Auth | Backend |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — forwards the caller's own credential |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — reuses `~/.codex/auth.json` |
| `xai` | `responses` | `api_key` (`XAI_API_KEY`) | `api.x.ai/v1` — the developer API, billed per token |
| `grok` | `responses` | `xai_oauth` | `cli-chat-proxy.grok.com/v1` — the Grok CLI proxy; reuses `~/.shunt/xai-auth.json` |
| `cursor` | `cursor` | `cursor_oauth` | `api2.cursor.sh` — reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`) |

### The codex provider (ChatGPT subscription)

Log in once with the Codex CLI; shunt reads and auto-refreshes `~/.codex/auth.json`:

```bash
codex login
```

If the file is missing or expired, shunt returns an `authentication_error` telling you to run `codex login`.

For the full setup — auth-file handling, model selection, effort, and context sizing — see the dedicated [ChatGPT / Codex guide](/guides/codex/).

### The grok provider (SuperGrok / X Premium+ subscription)

Log in once with the built-in device-code flow; shunt writes and auto-refreshes `~/.shunt/xai-auth.json`:

```bash
shunt login xai
```

xAI may gate OAuth access by subscription tier — if `grok` returns 403, use the `xai` API-key provider instead (`export XAI_API_KEY=…`).

:::caution[Model slugs]
The ChatGPT-account Codex backend **rejects** `gpt-*-codex` slugs — it only accepts the account's live-entitled slugs. The authoritative catalog is openai/codex's [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json). Current slugs are `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` (frontier) and `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`; older accounts may only be entitled to the earlier ones. Use `upstream_model` in a route to map any alias onto an entitled slug.
:::

### The cursor provider (Cursor subscription)

The built-in `cursor` provider reaches your **Cursor** subscription through Cursor's own ConnectRPC/protobuf AgentService — the `kind = "cursor"` native adapter translates it to and from Anthropic Messages, streaming included. Login and token refresh use `api2.cursor.sh`; agent turns run over HTTP/2 against Cursor's current agent host (`agentn.global.api5.cursor.sh`). Log in once:

```bash
shunt login cursor
```

This runs the Cursor OAuth flow and writes `~/.shunt/cursor-auth.json`, which shunt reads and auto-refreshes. If the file is missing or expired, shunt returns an `authentication_error` telling you to run `shunt login cursor`.

Route a `cursor:*` model id to it — the provider is seeded by default, so no `[providers.cursor]` table is required:

```toml
[[routes]]
model = "cursor:default"
provider = "cursor"
```

:::note[What the adapter carries]
The adapter streams assistant **text and reasoning**, bridges your client's **tools** as native Cursor MCP tool calls (a tool the model invokes surfaces as an Anthropic `tool_use` block with `stop_reason: "tool_use"`; you run it and send the `tool_result` back, and shunt re-runs the turn with that result in history), and forwards **inline images** (base64 sources; URL images are skipped). Cursor's own agentic file/shell tools are not exposed — only the tools your request advertises.
:::

**Model ids and agent modes.** The prefix selects Cursor's agent mode (Agent / Plan / Ask) and the suffix is the Cursor model id. Use the **wire** id, not the display name from `cursor-agent models`: Auto is `default` (routing `cursor:auto` fails with `Unknown model ID: auto`). Named models (e.g. `cursor:gpt-5.2`) require a paid plan that entitles them; free plans are limited to `cursor:default`.

| Form | Agent mode | Example |
| :-- | :-- | :-- |
| `cursor:<id>` / `cursor-agent:<id>` | Agent | `cursor:default` |
| `cursor-plan:<id>` | Plan | `cursor-plan:default` |
| `cursor-ask:<id>` | Ask | `cursor-ask:default` |

Legacy bare names are also accepted: `cursor`, `cursor-agent`, `cursor-composer`, `cursor-composer-fast` (Agent); `cursor-plan`, `composer-2.5` (Plan); `cursor-ask`, `composer-2.5-fast` (Ask). Any other model id is rejected with an `invalid_request_error`.

:::note[Overrides]
`SHUNT_CURSOR_BASE_URL` overrides the login/refresh endpoint, `SHUNT_CURSOR_AGENT_BASE_URL` the agent host (must stay an HTTPS `cursor.sh` host), `SHUNT_CURSOR_AUTH_FILE` the credential path, and `SHUNT_CURSOR_CLIENT_VERSION` the `x-cursor-client-version` header (bump it without a rebuild if Cursor starts rejecting a stale client version). A `cursor_oauth` provider is pinned to a Cursor host over HTTPS — pointing `base_url` off-origin is refused so the bearer token cannot leak.
:::

:::caution[Your own call]
Reusing a Cursor subscription from an unofficial client is your own call — it may run afoul of Cursor's terms or account enforcement. Use at your own risk.
:::

### The xai / grok providers (Grok)

Two built-in providers reach xAI's **Grok** models, split by credential: **`grok`** spends your
**SuperGrok / X Premium+** subscription over OAuth (`shunt login xai`, no per-token billing), while
**`xai`** uses an `XAI_API_KEY` against the metered developer API. A subscription bearer and an API
key are **not** interchangeable — each works only against its own provider.

For the full setup — login, both provider blocks, model slugs, the opt-in effort dial, and the
entitlement gotchas — see the dedicated [xAI / Grok guide](/guides/xai/).

## Adding an Anthropic-compatible backend

Most third-party "use Claude Code with X" gateways are Anthropic-Messages-compatible: `kind = "anthropic"` with `auth = "api_key"`, differing only in `base_url` and the key env var. Ready-to-use bases:

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| Mimo (Xiaomi) | `https://api.xiaomimimo.com/anthropic` | `mimo-v2.5-pro` — see [Mimo docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` (accepts `x_api_key`) |

For example, to route Kimi's model through shunt:

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

Then `export MOONSHOT_API_KEY=…`, [point Claude Code at shunt](/guides/connect-claude-code/), and select `kimi-k3[1m]` (via `ANTHROPIC_CUSTOM_MODEL_OPTION` or `ANTHROPIC_MODEL`). Run `shunt check` to validate — it reports an unknown provider in a route, a missing `api_key_env`, or a bad `base_url`.

Every provider key (`kind`, `auth`, `api_key_header`, `count_tokens`, …) is documented in the [Configuration Reference](/reference/configuration/).

## Subagent plugins

The [`pleaseai/shunt` marketplace](https://github.com/pleaseai/shunt/tree/main/plugins) ships ready-made Claude Code subagents pinned to each provider's models — one agent per model. Install a plugin, then `@`-mention a model or set `CLAUDE_CODE_SUBAGENT_MODEL`. Each agent's `model:` frontmatter diverts only that subagent; the main session stays on Claude.

| Plugin | Models (one agent each) | Provider |
| :-- | :-- | :-- |
| `shunt-codex` | `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` | `codex` (ChatGPT subscription) |
| `shunt-xai` | `grok-build-0.1`, `grok-4.5`, `grok-4.3` | `xai` (API key) or `grok` (subscription) |
| `shunt-kimi` | `kimi-k3[1m]`, `kimi-k2.7-code` | `kimi` |
| `shunt-deepseek` | `deepseek-v4-pro`, `deepseek-v4-flash` | `deepseek` |
| `shunt-zai` | `glm-5.2`, `glm-4.7` | `zai` |
| `shunt-minimax` | `MiniMax-M3[1m]` | `minimax` |
| `shunt-mimo` | `mimo-v2.5-pro` | `mimo` |

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-xai@shunt
```

Each plugin still needs its provider routed in `shunt.toml` (see the sections above) and the matching credential exported — the plugin's own README lists the exact route and env var. The grok models can be served by either xAI provider: `xai` (API key, billed per token) or `grok` (SuperGrok / X Premium+ subscription via `shunt login xai`; tier-gated — fall back to `xai` on 403).
