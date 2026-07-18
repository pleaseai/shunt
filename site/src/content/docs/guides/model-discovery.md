---
title: Model Discovery
description: Populate Claude Code's /model picker automatically with Claude-named aliases.
---

Discovery (`GET /v1/models`) can populate Claude Code's `/model` picker automatically. By default, shunt returns the admin-curated `[[models]]` entries first, followed by its builtin Claude model catalog mirroring the reference Claude apps gateway. Exact-id duplicates are removed in favor of the curated entry. Set the top-level `auto_include_builtin_models = false` to expose only the curated list. Builtin models need no dedicated `[[routes]]` entry — they resolve through your normal routing rules, falling back to `server.default_provider` when no `[[routes]]` or `[[route_prefixes]]` entry matches.

Claude Code ignores any discovered id that doesn't begin with `claude`/`anthropic` ([protocol reference](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)). Therefore, add a **Claude-named alias** when curating a non-Claude model such as `gpt-*`, and use a `[[routes]]` entry to rewrite it to the real upstream slug:

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

## Claude Desktop recognizes only tier-named ids

Claude Code accepts any discovered id beginning with `claude`/`anthropic`, but **Claude Desktop is stricter**: it surfaces only tier-named ids — `claude-sonnet-*`, `claude-opus-*`, `claude-haiku-*`, `claude-fable-*`. A `claude-<slug>-via-<provider>` alias like the one above therefore shows up in Claude Code but is **silently dropped by Claude Desktop**, since `gpt` is not a tier name.

The builtin catalog is all tier-named, so it stays visible in Desktop; only your curated `claude-<slug>-via-<provider>` aliases are lost. To expose a non-Anthropic backend to Claude Desktop, reuse a tier-named id and map it with a `[[routes]]` `upstream_model`:

```toml
[[routes]]
model = "claude-sonnet-5"        # a tier-named id Claude Desktop recognizes
provider = "codex"
upstream_model = "gpt-5.6-sol"   # real backend slug
```

Selecting it in Desktop resolves to the intended upstream. The route overrides the builtin catalog entry's default routing for that id, so pick a tier name whose backend mapping stays meaningful to your users.

## Discovery needs a gateway credential

A claude.ai OAuth *login* alone won't trigger discovery. Claude Code only issues the `/v1/models` request when `ANTHROPIC_AUTH_TOKEN`, an API key, or an `apiKeyHelper` is set; under a plain Max/Pro subscription login it sends nothing — no request reaches shunt, no cache is written — even with the flag on. See [choosing the credential](/guides/connect-claude-code/#2-choose-the-anthropic-credential); `claude setup-token` is the recommended route.

When shunt has [`[server.auth]`](/guides/shared-gateway/) enabled, discovery also requires a valid client token. It accepts the configured client-token header (for example through `ANTHROPIC_CUSTOM_HEADERS`) and Claude Code's discovery credential forms: `x-api-key` or `Authorization: Bearer`. Missing or invalid inbound credentials return `401 authentication_error`. Without `[server.auth]`, discovery remains open.

## Debugging

Discovery fails **silently** (3-second timeout, any redirect counts as failure) and falls back to the cached/built-in list. Run `claude --debug` and look for `[gatewayDiscovery]` lines to confirm it ran.
