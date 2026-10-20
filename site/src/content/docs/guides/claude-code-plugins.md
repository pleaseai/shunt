---
title: Claude Code Plugins
description: Install shunt's Claude Code plugins — the mod that shows usage above the prompt and pool headroom in /shunt:usage, and the subagent bundles for the models shunt diverts.
---

shunt ships its own Claude Code plugin marketplace. Add it once:

```
/plugin marketplace add pleaseai/shunt
```

Two kinds of plugin live there. The **`shunt` mod** shows usage in a band above the prompt — the gateway pool's on shunt, the session's own otherwise — and the pool's full breakdown in a command. The **provider bundles** add subagents that run on the models shunt diverts to other providers.

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

### The usage band

The mod also keeps usage in a band above the prompt, so you see it without asking. On a shunt gateway it shows the pool, tagged `shunt`:

```
shunt · 5H 5% ↻1h 41m · WK 46% ↻1d 7h · Fable 31% ↻1d 7h ⚠ anthropic degraded
```

Each figure is how much of the window is **used**: `1 - remaining`, the same pool-wide mean the command reports as headroom (see [Reading the numbers](#reading-the-numbers)). It is followed by the time until that window's earliest reset. The band counts used rather than left so that it reads the same way as Claude Code's own `/usage` and as the band does off shunt. A figure turns amber from 70% and red from 90%. Each pooled provider whose status is not `ok` is flagged at the end — amber for `degraded`, red for `exhausted` or `capped` — or the pool itself when no provider is listed. A window no account reports is left out.

Off shunt, the band shows the session's own Claude rate limits instead, untagged. That covers a session with no gateway set, a gateway that answers `GET /usage` with 404 or with something other than a pool report, and a gateway that pools no provider:

```
5H 5% ↻1h 41m · WK 46% ↻1d 7h
```

On shunt the band reads `GET /usage` when the session starts, every minute after that, and after each turn. It ignores the session's own limits there, because they come from whichever pool account answered last. A refused token, an unreachable gateway or another error status is a fault on shunt, so it is flagged rather than replaced: `shunt · ⚠ credential refused`. With neither a gateway nor rate limits — an API key sent straight to Anthropic — the band draws nothing.

Collapse the band for the session with its `[-]` (ctrl+x ctrl+a). To turn it off for good, set the plugin's **Usage band** option (`usageBand`) to off in `/config`: the mod then polls nothing and `/shunt:usage` still answers.

### Reading the numbers

`remaining` is the fraction of the pool's combined capacity still **usable**. `62%` means 62% of the headroom is left, not that 62% is spent. It is `mean(clamp(cap - utilization, 0, 1))` over the non-disabled accounts reporting the window, where `cap` is the account's `max_utilization` hard cap for that window (`100%` when none is set), so nine exhausted accounts plus one fresh uncapped one read `10%`, not `100%`, and an account at 44% under a 50% cap counts only `6%`. An account a cap currently excludes from the requests a window serves counts zero there: a 5h or 7d cap excludes it from every request, so it also counts zero in the other shared window and in the Fable window.

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

2. Enable the endpoint. `GET /usage` is opt-in and needs one of [`[server.auth]`](/guides/shared-gateway/) (client tokens) or `[server.gateway]` (gateway login) alongside the `[server.usage]` table in your [configuration](/reference/configuration/). With client tokens:

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   `[server.auth]` takes its tokens from the environment rather than the TOML — by default `SHUNT_CLIENT_TOKENS`, as `name:token` pairs — and the gateway fails to start when it is unset. Export it where the gateway runs, using the same token you set as `ANTHROPIC_AUTH_TOKEN` above:

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

   A gateway that issues logins through `[server.gateway]` can enable `[server.usage]` with that table instead of `[server.auth]`. Sessions started with `shunt gateway claude` then need no client token: the mod authenticates with their gateway login (see below).

3. Run Claude Code with function hooks enabled — the feature is early access:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

Without step 3 there is no band, and the command still exists but falls back to asking the model to read the endpoint with a tool call instead of answering directly.

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

A session started with [`shunt gateway claude`](/guides/gateway-login/) keeps no credential in these variables: the launcher scrubs `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` and wires `apiKeyHelper` to `shunt gateway token` instead. When none of the variables in the table is set, the mod reads the merged Claude Code settings and, if `apiKeyHelper` is shunt's own `shunt gateway token` (by bare name or by path), runs it directly, without a shell, and sends the gateway login token it prints as `Authorization: Bearer`. Because no shell runs, a leading `~/` in an unquoted helper path is expanded to your home directory by the mod itself. It reuses that token for five minutes, and asks the helper once more when the gateway refuses a reused one. The login is sent only when `SHUNT_BASE_URL` is unset or spells the same base URL as `ANTHROPIC_BASE_URL` (compared after trimming whitespace and trailing slashes, so `localhost` and `127.0.0.1` count as different); to read another gateway's pool, export `SHUNT_TOKEN` for it. If the helper fails (non-zero exit, no output, or it cannot be started), the band flags `shunt · ⚠ gateway login unavailable` and `/shunt:usage` tells you to run `shunt gateway login`, rather than quietly showing the session's own limits. The launcher does not scrub `SHUNT_TOKEN`: one inherited from your shell still takes precedence over the helper, so unset it to use the gateway login. Any other `apiKeyHelper` is never run; export `SHUNT_TOKEN` in such a session instead. The gateway accepts a gateway login on `GET /usage` alongside `[server.auth]` client tokens.

### Why `/shunt:usage` and not `/usage`

`/usage` is Claude Code's own built-in, and the engine refuses to let a plugin take a built-in's name. A plugin's markdown command is namespaced by the plugin instead, so the command ships as `shunt:usage` and collides with nothing.

## Provider subagent plugins

These add subagents pinned to a model id that shunt routes to another provider. The session keeps running inside Claude Code's harness — same tools, same skills — and only token generation is diverted.

| Plugin | Models | Setup |
| ------ | ------ | ----- |
| `shunt-codex` | GPT-6 Sol · Luna, GPT-5.6 Sol · Terra · Luna | [ChatGPT / Codex](/guides/codex/) |
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
