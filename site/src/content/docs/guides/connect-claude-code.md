---
title: Connect Claude Code
description: Point Claude Code at shunt, choose the right Anthropic credential, and select mapped models.
---

Based on the official [Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect) guide — shunt *is* the gateway you connect to.

## 1. Point Claude Code at shunt

Set the base URL to your running gateway (default bind `127.0.0.1:3001`), in your shell or persisted in a [settings file](https://code.claude.com/docs/en/settings) `env` block:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
```

```json
// ~/.claude/settings.json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3001"
  }
}
```

Keep your existing Anthropic credential — shunt **forwards it unchanged** to `api.anthropic.com` for every model you didn't map, so unmapped models keep working exactly as before. Provider credentials for mapped models are injected by shunt itself; Claude Code never sends them.

## 2. Choose the Anthropic credential

The credential Claude Code sends to shunt plays two roles: it authenticates **Claude passthrough models**, and it **gates [model discovery](/guides/model-discovery/)** — Claude Code only issues the `GET /v1/models` request when `ANTHROPIC_AUTH_TOKEN`, an API key, or an `apiKeyHelper` is set. Mapped models (`gpt-*` etc.) are unaffected either way.

| Credential | Token refresh | Discovery | Claude passthrough | Billing |
| :-- | :-- | :-- | :-- | :-- |
| claude.ai OAuth **login** only | automatic | ❌ never fires | ✅ | subscription |
| `ANTHROPIC_AUTH_TOKEN` from `claude setup-token` — **recommended** | none needed (one-year token) | ✅ | ✅ | subscription |
| `apiKeyHelper` = `shunt token` | the helper refreshes it | ✅ | ✅ | subscription |
| `ANTHROPIC_AUTH_TOKEN=<real API key>` | none needed | ✅ | ✅ | **API (not subscription)** |

A dummy value like `sk-dummy` satisfies the discovery gate but breaks passthrough — it is forwarded to Anthropic and returns 401.

**Prefer `claude setup-token`.** It mints a **one-year** OAuth token ([authentication docs](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)), so nothing needs refreshing, and one value covers both roles:

```bash
claude setup-token                        # browser sign-in → prints sk-ant-oat…
export ANTHROPIC_AUTH_TOKEN=sk-ant-oat…   # or persist it in a settings `env` block
```

:::caution[The refresh trap]
Once a gateway credential is active, Claude Code **stops refreshing its own login**, so the short-lived access token inside `~/.claude/.credentials.json` expires within hours and a helper that just *reads* that file breaks. Don't refresh it by hand either — `platform.claude.com/v1/oauth/token` is aggressively rate-limited. To reuse the live subscription login, use the built-in [`shunt token`](/reference/cli/#shunt-token) helper, which refreshes it safely.
:::

### The `shunt token` credential helper

`shunt token` prints a Claude subscription OAuth token to stdout, so it wires straight into Claude Code's `apiKeyHelper`:

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

- **Static mode** — if `SHUNT_GATEWAY_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` is set, it echoes that value unchanged. Point it at a `claude setup-token` value and nothing is ever refreshed.
- **Auto-refresh mode** — otherwise it reads `~/.claude/.credentials.json` (override with `CLAUDE_CREDENTIALS`), returns the access token, and refreshes it only within 5 minutes of expiry, writing back atomically at `0600`.

The static + `setup-token` route stays the simplest and safest default.

:::note[Why this authenticates Claude passthrough]
Claude Code sends an `apiKeyHelper` value in **both** `x-api-key` and `Authorization: Bearer`. A subscription OAuth token (`sk-ant-oat…`) is valid only as a bearer, so the copy in `x-api-key` would make `api.anthropic.com` reject the request. On the passthrough path shunt strips that duplicated `x-api-key` when the bearer is an OAuth token, leaving it to stand alone. Without this, `apiKeyHelper` + an OAuth token would cover only discovery and mapped models — passthrough would 401.
:::

## 3. Provide the mapped provider's credential

These go to **shunt's environment**, not Claude Code's:

```bash
export OPENAI_API_KEY=sk-...   # openai provider
codex login                    # codex/ChatGPT provider (auto-refreshed thereafter)
```

## 4. Select a mapped model

Claude Code's model discovery only honors ids beginning with `claude`/`anthropic`, so for OpenAI/Codex ids (`gpt-*`) use `ANTHROPIC_CUSTOM_MODEL_OPTION` — it adds a picker entry whose id skips validation:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

Then pick it from `/model` in Claude Code. That id is what shunt routes on, so it must match a `[[routes]]`/`[[route_prefixes]]` rule in your config.

The two picker-exposure methods split cleanly on the `claude-`/`anthropic-` prefix — they don't overlap. Discovery honors *only* `claude-`/`anthropic-` ids; `ANTHROPIC_CUSTOM_MODEL_OPTION` and the `CLAUDE_CODE_MAX_CONTEXT_TOKENS` window override apply *only* to ids that do **not** start with that prefix:

| What | `claude-`/`anthropic-` id (discovery alias) | non-`claude-` id (e.g. `gpt-5.6-sol`) |
| :-- | :-- | :-- |
| [`/v1/models` discovery](/guides/model-discovery/) → `/model` picker | ✅ auto-listed ("From gateway"), many models | ❌ dropped by Claude Code |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ not honored | ✅ adds to picker (**one id only**) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` window | ❌ ignored → 200k default | ✅ applies → set the real window |

So a `claude-…-via-codex` discovery alias is convenient (auto-listed, one-tap) but its context window is **stuck at the 200k default** — the override can't reach a `claude-`-prefixed id ([Effort & Context](/guides/effort-and-context/)). Pick the **discovery alias** for picker convenience across several models (accept the 200k denominator), or a **non-`claude-` id via `ANTHROPIC_CUSTOM_MODEL_OPTION`** for an accurate window, one model at a time.

:::tip[Or remap the tier aliases]
A third option repoints Claude Code's built-in `haiku`/`sonnet`/`opus` aliases at Codex slugs (e.g. `haiku → gpt-5.6-luna`, `sonnet → gpt-5.6-sol`), so the whole session's tier system resolves to your ChatGPT subscription without `ANTHROPIC_CUSTOM_MODEL_OPTION`. See [ChatGPT / Codex → Remap the tier aliases](/guides/codex/#remap-the-tier-aliases-to-codex).
:::

### Per-agent diversion

Per-context selection works via Claude Code's own knobs — divert one agent to a mapped model while the main session stays on Claude:

```yaml
# .claude/agents/researcher.md
---
name: researcher
model: gpt-5.6-sol   # this agent's inference is diverted; the main session stays on Claude
---
```

A named subagent's `model:` frontmatter is the **only** way to put a subagent on a `gpt-*` id: that field takes any string, whereas the Agent/Task tool's `model` parameter is restricted to the built-in aliases (`opus`/`sonnet`/`haiku`/`fable`) and can't take a gateway id. Spawn the agent by its type **without** a `model` override — the tool parameter outranks frontmatter (`CLAUDE_CODE_SUBAGENT_MODEL` > tool `model` > frontmatter > `inherit`), so passing one would shadow the mapped model. `CLAUDE_CODE_SUBAGENT_MODEL` forces every subagent onto one model. The window follows the model id automatically, so one global `CLAUDE_CODE_MAX_CONTEXT_TOKENS` sizes the mapped subagent while the Claude main keeps its own.

## 5. Verify

```bash
# Unmapped model -> forwarded to Anthropic (uses your Anthropic credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[{"role":"user","content":"."}]}'

# Mapped model -> diverted to the provider (uses shunt's provider credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

Then start `claude`, run `/status`, and check the **Anthropic base URL** line shows your gateway. See also [Effort & Context](/guides/effort-and-context/) for reasoning-effort and context-window tuning.
