---
title: Providers
description: The built-in providers and how to add any Anthropic-compatible backend with a TOML table.
---

Providers are a **name → config map**: a new upstream is just another `[providers.<name>]` table — no code change. Two adapter kinds cover everything:

- **`kind = "anthropic"`** — the upstream speaks the Anthropic Messages API. shunt passes the request through, optionally injecting a different API key.
- **`kind = "responses"`** — the upstream speaks the OpenAI Responses API. shunt translates Anthropic Messages ⇄ Responses, including streaming.

## Built-in providers

| Name | Kind | Auth | Backend |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `passthrough` | `api.anthropic.com` — forwards the caller's own credential |
| `openai` | `responses` | `api_key` (`OPENAI_API_KEY`) | `api.openai.com/v1` |
| `codex` | `responses` | `chatgpt_oauth` | `chatgpt.com/backend-api` — reuses `~/.codex/auth.json` |

### The codex provider (ChatGPT subscription)

Log in once with the Codex CLI; shunt reads and auto-refreshes `~/.codex/auth.json`:

```bash
codex login
```

If the file is missing or expired, shunt returns an `authentication_error` telling you to run `codex login`.

For the full setup — auth-file handling, model selection, effort, and context sizing — see the dedicated [ChatGPT / Codex guide](/guides/codex/).

:::caution[Model slugs]
The ChatGPT-account Codex backend **rejects** `gpt-*-codex` slugs — it only accepts the account's live-entitled slugs. The authoritative catalog is openai/codex's [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json). Current slugs are `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` (frontier) and `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`; older accounts may only be entitled to the earlier ones. Use `upstream_model` in a route to map any alias onto an entitled slug.
:::

## Adding an Anthropic-compatible backend

Most third-party "use Claude Code with X" gateways are Anthropic-Messages-compatible: `kind = "anthropic"` with `auth = "api_key"`, differing only in `base_url` and the key env var. Ready-to-use bases:

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| Mimo (Xiaomi) | `https://api-mimo.mi.com/anthropic` | see [Mimo docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` (accepts `x_api_key`) |

For example, to route Kimi's model through shunt:

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

Then `export KIMI_API_KEY=…`, [point Claude Code at shunt](/guides/connect-claude-code/), and select `kimi-k2.7-code` (via `ANTHROPIC_CUSTOM_MODEL_OPTION` or `ANTHROPIC_MODEL`). Run `shunt check` to validate — it reports an unknown provider in a route, a missing `api_key_env`, or a bad `base_url`.

Every provider key (`kind`, `auth`, `api_key_header`, `count_tokens`, …) is documented in the [Configuration Reference](/reference/configuration/).
