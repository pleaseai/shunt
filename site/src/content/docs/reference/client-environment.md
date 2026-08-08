---
title: Client Environment Variables
description: The Claude Code environment variables that change when the client routes through shunt.
---

This is the delta from a stock Claude Code setup — only the variables you set or change to route
Claude Code through shunt. For every other variable, see Anthropic's
[Environment variables](https://code.claude.com/docs/en/env-vars) reference.

Behavior marked *verified against Claude Code v2.1.224* was read from that build. Client behavior
changes between releases; re-check it against the build you run.

## Which gateway restrictions apply to you

Three unrelated things in Claude Code get called "gateway limitations", and they are triggered
separately. A normal shunt setup trips the second and third, not the first.

| Gate | What triggers it | Scope |
| :-- | :-- | :-- |
| **Signed-in Claude apps gateway session** | `forceLoginMethod: "gateway"` in managed settings, that is, shunt's [gateway login](/guides/gateway-login/). **`ANTHROPIC_BASE_URL` alone does not trigger it.** | The largest set — see [Gateway Login → Feature trade-offs](/guides/gateway-login/#feature-trade-offs). |
| **Non-first-party base URL** | `ANTHROPIC_BASE_URL` pointing anywhere other than `api.anthropic.com`. This is the normal shunt setup. | Tool search, fine-grained tool streaming, model discovery, Remote Control. |
| **Credential type** | Having `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, or an `apiKeyHelper` set at all, whatever the base URL is. | Prompt-cache TTL defaults, Remote Control, voice dictation, artifact publishing. |

Claude Code "treats an `ANTHROPIC_BASE_URL` gateway as an Anthropic-format endpoint and sends it
the beta headers and request body fields it sends to `api.anthropic.com`, except a small set of
diagnostics and defaults reserved for direct connections" — and the same
[gateway protocol reference](https://code.claude.com/docs/en/llm-gateway-protocol#feature-pass-through)
adds that "that set varies by release, so don't depend on its contents." Treat the tables below as
the variables that matter in practice, not as a complete inventory of the delta.

## Required

| Variable | Why |
| :-- | :-- |
| `ANTHROPIC_BASE_URL=http://127.0.0.1:3001` | Points Claude Code at your running gateway. See [Connect Claude Code](/guides/connect-claude-code/). |
| `ENABLE_TOOL_SEARCH=true` | Tool search is off by default on a non-first-party host. The official reference notes that when the base URL is "set to a non-first-party host, MCP tool search is disabled by default. Set `ENABLE_TOOL_SEARCH=true` if your proxy forwards `tool_reference` blocks." shunt forwards them — see [Tool search](/guides/codex/#tool-search). Two caveats from the same entry: `true` makes requests "fail on proxies that don't support `tool_reference`", and the variable is "ignored when `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` is set, which forces all tools to load upfront" — so don't set both. |

## Optional

| Variable | Why |
| :-- | :-- |
| `ENABLE_PROMPT_CACHING_1H=1` | Documented as "request a 1-hour prompt cache TTL instead of the default 5 minutes", and as "intended for API key, Amazon Bedrock, Google Cloud's Agent Platform, Microsoft Foundry, and Claude Platform on AWS users". **Most shunt users don't need it**: the same entry states that "subscription users within included usage receive 1-hour TTL automatically", and that only "subscription users drawing on usage credits can set it to keep the 1-hour TTL". "1-hour cache writes are billed at a higher rate". `FORCE_PROMPT_CACHING_5M=1` is the inverse override. On a signed-in gateway session the 1-hour TTL is unavailable for a different reason — see [Gateway Login → Feature trade-offs](/guides/gateway-login/#feature-trade-offs). |
| `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` | Usually unnecessary behind a plain `ANTHROPIC_BASE_URL`. Verified on the wire against Claude Code v2.1.224: an unrecognized model id such as `gpt-5.6-sol` already receives `output_config.effort` without the flag, and setting it produces a byte-identical request. The value comes from `CLAUDE_CODE_EFFORT_LEVEL`, which takes precedence over `/effort` and the `effortLevel` setting. The flag matters on non-first-party providers (Bedrock, Vertex, Foundry, gateway login). Caveat: ids on the client's legacy effort deny list — `claude-sonnet-4-5`, `claude-haiku-4-5` — never send effort, and no environment variable overrides that, so avoid remapping onto them. See [Effort & Context](/guides/effort-and-context/#reasoning-effort). |
| `API_FORCE_IDLE_TIMEOUT=0` | Turns off the 5-minute body idle timeout that "aborts a streaming model response when no bytes arrive". The official reference states that when unset, "the timeout is active on providers other than the direct Anthropic API and Claude Platform on AWS" — which includes a gateway, so it is **on** by default behind shunt. Despite the name it overrides in both directions: set `0` if an upstream can pause longer than 5 minutes between chunks, or `1` to keep the timeout on for every provider. The stream watchdogs "run independently of it and abort a long silent pause even when you set `0`". Requires Claude Code v2.1.169 or later. |
| `CLAUDE_CODE_ATTRIBUTION_HEADER=0` | Suppresses the client attribution block — despite the name it is a system-prompt text block, not an HTTP header. Verified against v2.1.224: the variable can only suppress it; a truthy value is a no-op. |
| `CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1` | Streams tool-call inputs as they are generated. The official reference documents it as "off by default on Microsoft Foundry and [gateway](https://code.claude.com/docs/en/llm-gateway) connections", and names `1` as the way to "force on when routing through a proxy via `ANTHROPIC_BASE_URL`". Without it, a large tool input "arrives only after Claude finishes generating it, which can look like it's hanging". shunt does not buffer streaming responses, so the deltas reach the client. |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | Adds a non-`claude-` id such as `gpt-5.6-sol` to the `/model` picker. See [Connect Claude Code](/guides/connect-claude-code/#4-select-a-mapped-model). |
| `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL` | Repoints a built-in tier alias at a shunt-routed id; `_NAME` and `_DESCRIPTION` control how it renders in the picker. `opusplan` has no variable of its own — it follows the Opus id in Plan Mode and the Sonnet id outside it. See [Model Aliases](/guides/model-aliases/#alias-resolution). |
| `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5` | Called out separately because Fable is filtered out of the picker behind a plain `ANTHROPIC_BASE_URL`, so without this the `fable` alias falls back to Opus. See [Fable disappears from the picker](/guides/model-aliases/#fable-disappears-from-the-picker). |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | Required, alongside a discovery-gating credential, for `GET /v1/models` to populate the picker. See [Model Discovery](/guides/model-discovery/). |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Sets the real context window for non-`claude-` ids; ignored for `claude-`-prefixed ones. See [Effort & Context](/guides/effort-and-context/). |

## No longer needed

`CLAUDE_CODE_ENABLE_AUTO_MODE=1` used to be required to make
[auto mode](https://code.claude.com/docs/en/permission-modes#eliminate-prompts-with-auto-mode)
available behind a gateway. It isn't any more. The official reference now lists the variable as
"accepted for compatibility with older releases and has no effect", because "auto mode is available
by default on every provider, including … signed-in Claude apps gateway sessions"; it was required
only "in v2.1.158 through v2.1.206".

The model restriction is separate and still applies. On the Anthropic API and Claude Platform on
AWS, auto mode needs "Claude Opus 4.6 or later, Sonnet 4.6 or later, or Fable 5". On Amazon Bedrock,
Google Cloud's Agent Platform, Microsoft Foundry, and signed-in Claude apps gateway sessions, "only
Claude Sonnet 5, Opus 4.7 or later, and Fable 5" are supported. If you remap a tier alias onto an
older id, auto mode stops appearing — that is the requirement, not an outage.

## What a base URL does not redirect

Two client checks call `api.anthropic.com` directly instead of following `ANTHROPIC_BASE_URL`, so
they never reach shunt and never appear in its logs: the
[fast mode](https://code.claude.com/docs/en/fast-mode) availability check and the
[WebFetch domain safety check](https://code.claude.com/docs/en/data-usage#webfetch-domain-safety-check).
On a network that blocks direct egress to `api.anthropic.com`, "fast mode can report a connectivity
error while inference through the gateway keeps working"
([gateway protocol reference](https://code.claude.com/docs/en/llm-gateway-protocol)).

Setting the base URL on its own also doesn't change who pays. Per the
[LLM gateway overview](https://code.claude.com/docs/en/llm-gateway#subscriptions-and-gateways),
`ANTHROPIC_BASE_URL` without a gateway credential "doesn't replace the subscription. Requests still
route through the gateway, but a saved claude.ai login remains the active credential", so that
login's usage limits and billing still apply.

## Features a gateway credential turns off

These follow from having a credential variable set, not from shunt:

- **Remote Control and voice dictation** are unavailable "while `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, or an `apiKeyHelper` is active". As of Claude Code v2.1.196, Remote
  Control is "also disabled while `ANTHROPIC_BASE_URL` points at a non-Anthropic host, so signing in
  with claude.ai isn't enough on its own"
  ([connect guide](https://code.claude.com/docs/en/llm-gateway-connect)). `claude doctor` names the
  variable to unset.
- **Publishing artifacts** requires a claude.ai-backed session: "sessions using an API key, gateway
  token, or cloud-provider credential cannot publish"
  ([artifacts](https://code.claude.com/docs/en/artifacts)).
- **Claude Desktop with a gateway configuration active** "runs sessions on your local machine only:
  the environment picker doesn't offer SSH sessions or Anthropic-hosted cloud environments, and
  Remote Control is unavailable" ([connect guide](https://code.claude.com/docs/en/llm-gateway-connect)).
  See [Connect Claude Desktop](/guides/connect-claude-desktop/).

## Do not set

`CLAUDE_CODE_USE_GATEWAY` switches the client into signed-in Claude apps gateway session mode, which
disables a further set of features client-side regardless of what the upstream supports — see
[Gateway Login → Feature trade-offs](/guides/gateway-login/#feature-trade-offs). A plain
`ANTHROPIC_BASE_URL` is all a normal shunt setup needs.
