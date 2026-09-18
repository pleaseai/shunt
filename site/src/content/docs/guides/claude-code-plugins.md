---
title: Claude Code Plugins
description: Install shunt's Claude Code plugins — the /shunt:usage mod that shows pool headroom, and the subagent bundles for the models shunt diverts.
---

shunt ships its own Claude Code plugin marketplace. Add it once:

```
/plugin marketplace add pleaseai/shunt
```

Two kinds of plugin live there. The **`shunt` mod** adds a command that reports the gateway's own pool usage. The **provider bundles** add subagents that run on the models shunt diverts to other providers.

## The `shunt` mod: `/shunt:usage`

```
/plugin install shunt@shunt
```

`/shunt:usage` prints the shared account pool's remaining headroom, read from the gateway's [`GET /usage`](/reference/endpoints/) endpoint:

```
shunt: pool — degraded   http://127.0.0.1:3001

  5h    ▓▓▓▓▓▓░░░░  62% left   resets 04:11
  7d    ▓▓▓▓▓▓▓▓░░  81% left   resets Sun 01:11
  fable ▓▓░░░░░░░░  19% left   resets Sun 01:11

  claude  ok         5h  71%  7d  84%  fable  19%
  codex   exhausted  5h   0%  7d  40%  fable    —

  headroom left, averaged over the pool's accounts; a shared figure, not a promise about your next request
```

The mod answers the command itself, so nothing is sent to the model and the answer costs no tokens.

### Reading the numbers

`remaining` is the fraction of the pool's combined capacity still **unused**. `62%` means 62% of the headroom is left, not that 62% is spent. It is `mean(1 - utilization)` over the non-disabled accounts reporting the window, so nine exhausted accounts plus one fresh one read `10%`, not `100%`.

It is a **pool-wide aggregate, not a prediction**. Routing also weighs availability, model, session affinity and priority, so a healthy figure is not a promise that your next request is admitted.

| Window  | What it covers |
| ------- | -------------- |
| `5h`    | The rolling 5-hour session window |
| `7d`    | The shared weekly window |
| `fable` | The Fable-scoped weekly window (`7d_oi`) |

A window reads `—` when no non-disabled account reports it. ChatGPT/Codex accounts populate `5h` and `7d` from `x-codex-*` response headers and have no Fable-scoped signal of their own.

The first block is the aggregate across every pooled provider; the rows beneath it are the same aggregate per pooled provider, so a session routed to one provider can read that provider's headroom instead of the blended figure. The endpoint never carries account names, counts, priorities or per-account numbers — that detail stays behind the admin-only `GET /admin/api/pool`.

### Prerequisites

1. Point Claude Code at your gateway, as in [Connect Claude Code](/guides/connect-claude-code/):

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
   export ANTHROPIC_AUTH_TOKEN=<your client token>
   ```

2. Enable the endpoint. `GET /usage` is opt-in and requires [`[server.auth]`](/guides/shared-gateway/), so both tables must be present in your [configuration](/reference/configuration/):

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   `[server.auth]` takes its tokens from the environment rather than the TOML — by default `SHUNT_CLIENT_TOKENS`, as `name:token` pairs — and the gateway fails to start when it is unset. Export it where the gateway runs, using the same token you set as `ANTHROPIC_AUTH_TOKEN` above:

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

3. Run Claude Code with function hooks enabled — the feature is early access:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

Without step 3 the command still exists, but falls back to asking the model to read the endpoint with a tool call instead of answering directly.

### What it reads

The mod reads five environment variables and writes none. By default it sends the session's own credential to the gateway the session is already sending every message to, so it reaches no host the session was not already using; `SHUNT_BASE_URL` is the one deliberate exception, and points it at a gateway you name instead. With no base URL set at all it says so rather than calling Anthropic's own API.

| Variable | Purpose |
| -------- | ------- |
| `SHUNT_BASE_URL` | The gateway base URL; overrides `ANTHROPIC_BASE_URL` |
| `ANTHROPIC_BASE_URL` | The gateway this session already routes through |
| `SHUNT_TOKEN` | The client token; overrides both below, sent as `Authorization: Bearer` |
| `ANTHROPIC_AUTH_TOKEN` | Sent as `Authorization: Bearer`, as Claude Code sends it |
| `ANTHROPIC_API_KEY` | Sent as `x-api-key`, as Claude Code sends it |

`SHUNT_BASE_URL` is what lets you read one gateway's pool while routing traffic through another.

### Why `/shunt:usage` and not `/usage`

`/usage` is Claude Code's own built-in, and the engine refuses to let a plugin take a built-in's name. A plugin's markdown command is namespaced by the plugin instead, so the command ships as `shunt:usage` and collides with nothing.

## Provider subagent plugins

These add subagents pinned to a model id that shunt routes to another provider. The session keeps running inside Claude Code's harness — same tools, same skills — and only token generation is diverted.

| Plugin | Models | Setup |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/guides/codex/) |
| `shunt-xai` | Grok 4.6 · 4.5 · Build | [xAI / Grok](/guides/xai/) |
| `shunt-kimi` | Kimi K2.7 Code · K3 | [Kimi](/providers/kimi/) |
| `shunt-deepseek` | DeepSeek V4 Pro · Flash | [DeepSeek](/providers/deepseek/) |
| `shunt-zai` | GLM 5.2 · 4.7 | [Z.ai](/providers/zai/) |
| `shunt-minimax` | MiniMax-M3 | [MiniMax](/providers/minimax/) |
| `shunt-mimo` | MiMo V2.5 Pro | [MiMo](/providers/mimo/) |

Install one the same way:

```
/plugin install shunt-codex@shunt
```

Each needs the matching model ids routed to that provider in your gateway configuration; without that, Claude Code sends the model id straight to Anthropic and the request fails.

## Caveats

Function hooks are early access. A hooks module loads only where they are enabled, and the API the `shunt` mod is written against may change between Claude Code releases without notice. The provider bundles use no function hooks and are unaffected.
