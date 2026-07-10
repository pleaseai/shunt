---
title: Quickstart
description: Configure shunt, run the gateway, and point Claude Code at it in five minutes.
---

This walkthrough takes you from an installed `shunt` binary to a Claude Code session where a `gpt-*` model runs inside Claude Code's own harness. Install shunt first — see [Installation](/getting-started/installation/).

## 1. Configure

shunt ships with all providers preconfigured, so a minimal config only declares routing. Create `shunt.toml` (in the working directory, or `~/.config/shunt/shunt.toml`):

```toml
# Exact model id -> provider
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"     # reuses your ChatGPT login via `codex login`

# Or send every gpt-* id to the OpenAI API
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"    # uses OPENAI_API_KEY
```

Validate it:

```bash
shunt check
# -> config ok
```

## 2. Provide the provider credential

Pick the provider you routed to:

```bash
codex login                     # codex provider: ChatGPT subscription login
# or
export OPENAI_API_KEY=sk-...    # openai provider: API key
```

## 3. Run the gateway

```bash
shunt run
# -> shunt listening on 127.0.0.1:3001
```

## 4. Point Claude Code at it

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # so /effort maps to reasoning.effort
claude
```

Inside Claude Code, run `/model` and pick `gpt-5.6-sol`. Unmapped models (all your `claude-*` ids) keep working exactly as before — shunt forwards them to Anthropic with your own credential.

## 5. Verify

Test the gateway directly before (or instead of) opening Claude Code:

```bash
# Mapped model -> diverted to the provider (uses shunt's provider credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

A JSON response starting with `{"id":"msg_` means it worked. Inside Claude Code, `/status` should show the **Anthropic base URL** as `http://127.0.0.1:3001`.

## Where to next

- [Configuration](/guides/configuration/) — config files, env overrides, routing precedence.
- [Providers](/guides/providers/) — add Kimi, DeepSeek, GLM, OpenRouter, and other backends.
- [Connect Claude Code](/guides/connect-claude-code/) — credentials in depth, per-agent routing.
- [Troubleshooting](/reference/troubleshooting/) — common errors and fixes.
