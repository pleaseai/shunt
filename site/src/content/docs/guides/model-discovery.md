---
title: Model Discovery
description: Populate Claude Code's /model picker automatically with Claude-named aliases.
---

Discovery (`GET /v1/models`) can populate Claude Code's `/model` picker automatically — **but Claude Code ignores any id that doesn't begin with `claude`/`anthropic`** ([protocol reference](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)). So a `gpt-*` id is dropped client-side no matter what; discovery is only useful when you expose a **Claude-named alias** that a `[[routes]]` entry rewrites to the real upstream slug:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # must begin with claude/anthropic
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # the alias Claude Code sends
provider = "codex"
upstream_model = "gpt-5.6-sol"          # real slug forwarded to the ChatGPT backend
```

Then enable discovery (Claude Code v2.1.129+) and restart shunt + Claude Code:

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

The alias appears in `/model` labeled *From gateway*; selecting it sends `claude-gpt-5.6-sol-via-codex`, which shunt routes to `codex` and rewrites to `gpt-5.6-sol`.

For `gpt-*` ids without an alias, use `ANTHROPIC_CUSTOM_MODEL_OPTION` instead — see [Connect Claude Code](/guides/connect-claude-code/#4-select-a-mapped-model).

## Discovery needs a gateway credential

A claude.ai OAuth *login* alone won't trigger discovery. Claude Code only issues the `/v1/models` request when `ANTHROPIC_AUTH_TOKEN`, an API key, or an `apiKeyHelper` is set; under a plain Max/Pro subscription login it sends nothing — no request reaches shunt, no cache is written — even with the flag on. See [choosing the credential](/guides/connect-claude-code/#2-choose-the-anthropic-credential); `claude setup-token` is the recommended route.

## Debugging

Discovery fails **silently** (3-second timeout, any redirect counts as failure) and falls back to the cached/built-in list. Run `claude --debug` and look for `[gatewayDiscovery]` lines to confirm it ran.
