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
| `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` | Usually unnecessary behind a plain `ANTHROPIC_BASE_URL`. Verified on the wire against Claude Code v2.1.224: an unrecognized model id such as `gpt-5.6-sol` already receives `output_config.effort` without the flag, and setting it produces a byte-identical request. The value comes from `CLAUDE_CODE_EFFORT_LEVEL`, which takes precedence over `/effort` and the `effortLevel` setting. The flag matters on non-first-party providers (Bedrock, Vertex, Foundry, gateway login). Caveat: ids on the client's legacy effort deny list — `claude-sonnet-4-5`, `claude-haiku-4-5` — never send effort, and no environment variable overrides that, so avoid remapping onto them. See [Effort & Context](/guides/effort-and-context/#reasoning-effort). |
| `API_FORCE_IDLE_TIMEOUT=0` | Turns off the 5-minute body idle timeout that "aborts a streaming model response when no bytes arrive". The official reference states that when unset, "the timeout is active on providers other than the direct Anthropic API and Claude Platform on AWS" — which includes a gateway, so it is **on** by default behind shunt. Despite the name it overrides in both directions: set `0` if an upstream can pause longer than 5 minutes between chunks, or `1` to keep the timeout on for every provider. The stream watchdogs "run independently of it and abort a long silent pause even when you set `0`". Requires Claude Code v2.1.169 or later. |
| `CLAUDE_CODE_ATTRIBUTION_HEADER=0` | Suppresses the client attribution block — despite the name it is a system-prompt text block, not an HTTP header. Verified against v2.1.224: the variable can only suppress it; a truthy value is a no-op. |
| `CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1` | Streams tool-call inputs as they are generated. The official reference documents it as "off by default on Microsoft Foundry and [gateway](https://code.claude.com/docs/en/llm-gateway) connections", and names `1` as the way to "force on when routing through a proxy via `ANTHROPIC_BASE_URL`". Without it, a large tool input "arrives only after Claude finishes generating it, which can look like it's hanging". shunt does not buffer streaming responses, so the deltas reach the client. |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | Adds a non-`claude-` id such as `gpt-5.6-sol` to the `/model` picker. See [Connect Claude Code](/guides/connect-claude-code/#4-select-a-mapped-model). |
| `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL` | Repoints a built-in tier alias at a shunt-routed id; `_NAME` and `_DESCRIPTION` control how it renders in the picker. `opusplan` has no variable of its own — it follows the Opus id in Plan Mode and the Sonnet id outside it. See [Model Aliases](/guides/model-aliases/#alias-resolution). |
| `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5` | Called out separately because Fable is filtered out of the picker behind a plain `ANTHROPIC_BASE_URL`, so without this the `fable` alias falls back to Opus. See [Fable disappears from the picker](/guides/model-aliases/#fable-disappears-from-the-picker). |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | Required, alongside a discovery-gating credential, for `GET /v1/models` to populate the picker. See [Model Discovery](/guides/model-discovery/). |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Sets the real context window for non-`claude-` ids; ignored for `claude-`-prefixed ones. See [Effort & Context](/guides/effort-and-context/). |

## Do not set

`CLAUDE_CODE_USE_GATEWAY` switches the client into enterprise-gateway session mode, which disables
some features client-side regardless of what the upstream supports — see
[Gateway Login](/guides/gateway-login/) for the trade-offs. A plain `ANTHROPIC_BASE_URL` is all a
normal shunt setup needs.
