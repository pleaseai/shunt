---
title: Client Environment Variables
description: Recommended Claude Code environment variables when the client connects through shunt, and which client features a proxy cannot restore.
---

shunt only controls what happens *after* a request leaves Claude Code. Several client features
are gated **client-side**, before shunt ever sees the request, so the environment you give Claude
Code decides which of them survive the trip through a gateway.

This page collects the variables worth setting and the limitations no proxy configuration can lift.
The authoritative list of every variable is Anthropic's
[Environment variables](https://code.claude.com/docs/en/env-vars) reference; this page covers only
the ones whose behavior changes when the base URL is not `api.anthropic.com`.

Claims below are either quoted from the official documentation or attributed to inspection of a
specific Claude Code build. Client behavior changes between releases — re-check anything
version-attributed against the build you actually run.

## Choose the credential first

The credential is the highest-leverage choice, because it decides whether Claude Code treats the
session as *first-party*. Two independent client-side gates key off it: the prompt-cache TTL and
[model discovery](/guides/model-discovery/).

The [Connect Claude Code](/guides/connect-claude-code/#2-choose-the-anthropic-credential) guide
compares these credentials on token refresh, discovery, passthrough, and billing. This page adds
the cache-TTL axis, which pulls in the opposite direction from discovery:

| Credential | Automatic 1-hour cache TTL | Discovery |
| :-- | :-- | :-- |
| claude.ai OAuth `/login` only | ✅ preserved | ❌ never fires |
| `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` | ✅ preserved | ❌ never fires |
| `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, or `apiKeyHelper` | ❌ disabled — set `ENABLE_PROMPT_CACHING_1H=1` | ✅ fires |

Anthropic's documentation states that
["Subscription users within included usage receive 1-hour TTL automatically"](https://code.claude.com/docs/en/env-vars).
In Claude Code v2.1.224, that automatic path is disabled whenever an auth token, an API key, or an
`apiKeyHelper` is configured — **even when the base URL points at a proxy and the session is
otherwise first-party**. The consequence is silent: prompt caching keeps working, it just falls
back to the 5-minute TTL, so a long agentic session re-writes its cached prefix far more often
than it needs to.

There is no single option that wins on both axes. Pick by what the session actually needs:

- **Long agentic sessions, few models** — prefer `/login` or `CLAUDE_CODE_OAUTH_TOKEN` and keep the
  1-hour TTL. Select mapped models with `ANTHROPIC_CUSTOM_MODEL_OPTION` instead of discovery.
- **Many mapped models, picker convenience** — keep `ANTHROPIC_AUTH_TOKEN` for discovery and add
  `ENABLE_PROMPT_CACHING_1H=1` to restore the TTL.

`CLAUDE_CODE_OAUTH_TOKEN` takes the same `sk-ant-oat…` value `claude setup-token` prints, so
switching from `ANTHROPIC_AUTH_TOKEN` to it is a rename, not a re-login. The official reference
describes it as an
["OAuth access token for Claude.ai authentication … Takes precedence over keychain-stored credentials"](https://code.claude.com/docs/en/env-vars).
It is never auto-refreshed, which is fine for a one-year `setup-token` value but means you own the
expiry. If you also set `CLAUDE_CODE_OAUTH_SCOPES`, it must include `user:inference`; in v2.1.224 a
scope list without it loses the 1-hour TTL with no error.

## Recommended variables

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # required — point Claude Code at shunt
export ENABLE_TOOL_SEARCH=true                    # shunt forwards tool_reference blocks
export ENABLE_PROMPT_CACHING_1H=1                 # only with auth-token / API-key style auth
```

| Variable | Why it matters through shunt |
| :-- | :-- |
| `ENABLE_TOOL_SEARCH` | The official reference notes that when `ANTHROPIC_BASE_URL` is "set to a non-first-party host, [MCP tool search](https://code.claude.com/docs/en/mcp#scale-with-mcp-tool-search) is disabled by default. Set `ENABLE_TOOL_SEARCH=true` if your proxy forwards `tool_reference` blocks." shunt does forward them, so enabling it is safe — see [Tool search](/guides/codex/#tool-search). `auto:N` sets a percentage threshold instead of always deferring. |
| `ENABLE_PROMPT_CACHING_1H` | Restores the 1-hour TTL when your credential disabled the automatic path. Documented as: "Set to `1` to request a 1-hour prompt cache TTL instead of the default 5 minutes… **1-hour cache writes are billed at a higher rate**." On a subscription upstream this is the TTL you would have had anyway; on an API-key upstream it is a real price change. `FORCE_PROMPT_CACHING_5M=1` is the inverse override. |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | Adds a non-`claude-` id (e.g. `gpt-5.6-sol`) to the `/model` picker without discovery — see [Connect Claude Code](/guides/connect-claude-code/#4-select-a-mapped-model). |
| `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` | Repoints a built-in tier alias at a shunt-routed id, with `_NAME` and `_DESCRIPTION` controlling how it renders in the picker. See [Model Aliases](/guides/model-aliases/#alias-resolution). |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | Required — alongside a discovery-gating credential — for `GET /v1/models` to populate the picker. See [Model Discovery](/guides/model-discovery/). |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Sets the real context window for non-`claude-` ids; ignored for `claude-`-prefixed ones. See [Effort & Context](/guides/effort-and-context/). |

## What a proxy cannot restore

### Gateway-session mode disables more than it looks

Signing in through [gateway login](/guides/gateway-login/) — or otherwise putting the client in
Claude apps gateway mode — turns off two features client-side, whatever the upstream supports.
Anthropic's [gateway availability table](https://code.claude.com/docs/en/claude-apps-gateway#availability-and-limitations)
lists both as *Not available*:

> The CLI can't see which upstream provider the gateway routes to, so it can't verify web search
> support and disables WebSearch on gateway sessions.

> The CLI omits the extended-cache-ttl beta on gateway sessions, because not every upstream the
> gateway can route to supports the 1-hour TTL, so prompt caching through the gateway uses the
> 5-minute TTL.

Neither is recoverable by configuring shunt: the WebSearch tool is stripped from the request before
it is sent, and the beta (`extended-cache-ttl-2025-04-11` in v2.1.224) is simply absent from the
header. `cache_control` breakpoints themselves are still forwarded, so ordinary 5-minute prompt
caching keeps working.

Gateway login is worth it for managed multi-user access, and it unlocks picker behavior a plain
base URL does not — but for a personal setup, a plain `ANTHROPIC_BASE_URL` keeps WebSearch and the
1-hour TTL. In v2.1.224 these gates read the client's API-provider mode, not the base-URL hostname,
so a bare `ANTHROPIC_BASE_URL` override does **not** put the session in gateway mode.

Note that Claude Code's **hosted** web search is a different mechanism and does work through
shunt's Codex and OpenAI routes with no extra setup — see [Web search](/guides/codex/#web-search).
It is Anthropic's server-side WebSearch tool that gateway sessions drop.

### Host-gated features

Some optimizations check for an exact `api.anthropic.com` hostname, so **any** proxy loses them
regardless of credential or mode:

- **Global prompt-cache scope** (the `prompt-caching-scope-2026-01-05` beta) is gated on an exact
  host match in v2.1.224, so it is dropped behind shunt.
- **Remote Control** — per the official reference, "As of v2.1.196, [Remote Control](https://code.claude.com/docs/en/remote-control#requirements)
  is disabled when this points at a host other than `api.anthropic.com`."
- **Native 1M context windows** are only trusted on the first-party host; use the `[1m]` aliases
  instead — see [Model Aliases](/guides/model-aliases/#1m-context-is-not-applied-automatically).

## Advanced

`ANTHROPIC_BETAS` takes a "comma-separated list of additional `anthropic-beta` header values to
include in API requests" and, unlike the `--betas` CLI flag, "works with all auth methods including
Claude.ai subscription". It is an escape hatch for opting into a beta before Claude Code supports
it natively, and it is useful for testing what an upstream accepts. It is not a general
recommendation: a beta your upstream does not implement can turn a working request into a 400, and
the value is sent on every request until you unset it.

`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` is the opposite lever, for upstreams that reject unknown
beta headers outright. It also forces every MCP tool to load upfront, overriding `ENABLE_TOOL_SEARCH`.
