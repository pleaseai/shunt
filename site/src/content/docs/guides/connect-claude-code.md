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

### Per-agent diversion

Per-context selection works via Claude Code's own knobs — a subagent's `model:` frontmatter, or `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents — so you can divert only one agent while the main session stays on Claude:

```yaml
# .claude/agents/researcher.md
---
name: researcher
model: gpt-5.6-sol   # this agent's inference is diverted; the main session stays on Claude
---
```

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
