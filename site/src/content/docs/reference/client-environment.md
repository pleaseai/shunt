---
title: Client Environment Variables
description: The Claude Code environment variables that change when the client routes through shunt.
---

This is the delta from a stock Claude Code setup — only the variables you set or change to route
Claude Code through shunt. For every other variable, see Anthropic's
[Environment variables](https://code.claude.com/docs/en/env-vars) reference.

Behavior marked *verified against Claude Code v2.1.224* was read from that build. Client behavior
changes between releases; re-check it against the build you run.

## Required

| Variable | Why |
| :-- | :-- |
| `ANTHROPIC_BASE_URL=http://127.0.0.1:3001` | Points Claude Code at your running gateway. See [Connect Claude Code](/guides/connect-claude-code/). |
| `ENABLE_TOOL_SEARCH=true` | Tool search is off by default on a non-first-party host. The official reference notes that when the base URL is "set to a non-first-party host, MCP tool search is disabled by default. Set `ENABLE_TOOL_SEARCH=true` if your proxy forwards `tool_reference` blocks." shunt forwards them — see [Tool search](/guides/codex/#tool-search). |

## Optional

| Variable | Why |
| :-- | :-- |
| `ENABLE_PROMPT_CACHING_1H=1` | Documented as "request a 1-hour prompt cache TTL instead of the default 5 minutes". The same entry notes that "1-hour cache writes are billed at a higher rate". `FORCE_PROMPT_CACHING_5M=1` is the inverse override. |
| `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` | Sends the `effort` parameter for every model. Verified against v2.1.224: Claude Code otherwise sends it only for models on a built-in support list, so a model reached through shunt under a remapped id drops effort control silently. See [Effort & Context](/guides/effort-and-context/#reasoning-effort). |
| `API_FORCE_IDLE_TIMEOUT=0` | Turns off the 5-minute body idle timeout that "aborts a streaming model response when no bytes arrive". The official reference states that when unset, "the timeout is active on providers other than the direct Anthropic API and Claude Platform on AWS" — which includes a gateway, so it is **on** by default behind shunt. Set `0` if an upstream can pause longer than 5 minutes between chunks; `1` keeps it on for every provider. The stream watchdogs "run independently of it and abort a long silent pause even when you set `0`". Requires Claude Code v2.1.169 or later. |
| `CLAUDE_CODE_ATTRIBUTION_HEADER=0` | Suppresses the client attribution header. Verified against v2.1.224: the variable can only suppress the header — a truthy value is a no-op. |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | Adds a non-`claude-` id such as `gpt-5.6-sol` to the `/model` picker. See [Connect Claude Code](/guides/connect-claude-code/#4-select-a-mapped-model). |
| `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` | Repoints a built-in tier alias at a shunt-routed id; `_NAME` and `_DESCRIPTION` control how it renders in the picker. See [Model Aliases](/guides/model-aliases/#alias-resolution). |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | Required, alongside a discovery-gating credential, for `GET /v1/models` to populate the picker. See [Model Discovery](/guides/model-discovery/). |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Sets the real context window for non-`claude-` ids; ignored for `claude-`-prefixed ones. See [Effort & Context](/guides/effort-and-context/). |

## Do not set

`CLAUDE_CODE_USE_GATEWAY` switches the client into enterprise-gateway session mode, which disables
some features client-side regardless of what the upstream supports — see
[Gateway Login](/guides/gateway-login/) for the trade-offs. A plain `ANTHROPIC_BASE_URL` is all a
normal shunt setup needs.
