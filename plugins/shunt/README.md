# shunt

A **Claude Mod** — a Claude Code plugin whose behaviour is a
[function hooks](https://github.com/anthropics/claude-code/issues/91870) module —
that answers `/shunt:usage` with the [shunt](https://github.com/pleaseai/shunt)
gateway's shared account pool usage.

```
> /shunt:usage

shunt: pool — degraded   http://127.0.0.1:3001

  5h    ▓▓▓▓▓▓░░░░  62% left   resets 04:11
  7d    ▓▓▓▓▓▓▓▓░░  81% left   resets Sun 01:11
  fable ▓▓░░░░░░░░  19% left   resets Sun 01:11

  claude  ok         5h  71%  7d  84%  fable  19%
  codex   exhausted  5h   0%  7d  40%  fable    —

  headroom left, averaged over the pool's accounts; a shared figure, not a promise about your next request
```

The mod reads the gateway's [`GET /usage`](https://shunt.dev/reference/endpoints/)
endpoint and prints it. It answers the command itself — the hook returns without
calling `next`, so nothing is sent to the model and the answer costs no tokens.

## Reading the numbers

`remaining` is the fraction of the pool's combined capacity still **unused**, so
`62%` means 62% of the headroom is left, not that 62% is spent. It is
`mean(1 - utilization)` over the non-disabled accounts that report the window:
nine exhausted accounts plus one fresh one read `10%`, not `100%`.

It is a **pool-wide aggregate, not a prediction** — routing also weighs
availability, model, session affinity and priority, so a healthy figure is not a
promise that your next request is admitted. For the routing-aware worst case, use
`GET /api/oauth/usage` instead.

| Window  | What it covers |
| ------- | -------------- |
| `5h`    | The rolling 5-hour session window |
| `7d`    | The shared weekly window |
| `fable` | The Fable-scoped weekly window (`7d_oi`) |

A window reads `—` when no non-disabled account reports it. ChatGPT/Codex
accounts populate `5h` and `7d` from `x-codex-*` response headers and have no
Fable-scoped signal of their own.

`pool` is the aggregate across every pooled provider; the rows beneath it are the
same aggregate per pooled provider, so a session routed to one provider can read
that provider's headroom instead of the blended figure. The endpoint never
carries account names, counts, priorities or per-account numbers — that detail
stays behind the admin-only `GET /admin/api/pool`.

## Prerequisites

1. **Point Claude Code at your gateway**, as you already do to route through it:

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
   export ANTHROPIC_AUTH_TOKEN=<your client token>
   ```

2. **Enable the endpoint** on the gateway. `GET /usage` is opt-in and requires
   `[server.auth]`, so both tables must be present in `shunt.toml`:

   ```toml
   [server.auth]
   tokens = ["<your client token>"]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

3. **Run Claude Code with function hooks enabled** — the feature is early access:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

Without step 3 the command still exists, but falls back to `commands/usage.md`,
which asks the model to read the endpoint with a tool call instead.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt@shunt
```

## Configuration

The mod reads five environment variables and writes none. By default it sends the
session's own credential to the gateway the session is already sending every
message to, so it reaches no host the session was not already using;
`SHUNT_BASE_URL` is the one deliberate exception, and points it at a gateway you
name instead. With no base URL set at all it asks nothing rather than sending the
credential to Anthropic's API.

| Variable | Purpose |
| -------- | ------- |
| `SHUNT_BASE_URL` | The gateway base URL; overrides `ANTHROPIC_BASE_URL` |
| `ANTHROPIC_BASE_URL` | The gateway this session already routes through |
| `SHUNT_TOKEN` | The client token; overrides both below, sent as `Authorization: Bearer` |
| `ANTHROPIC_AUTH_TOKEN` | Sent as `Authorization: Bearer`, as Claude Code sends it |
| `ANTHROPIC_API_KEY` | Sent as `x-api-key`, as Claude Code sends it |

`SHUNT_BASE_URL` is what lets you read one gateway's pool while routing traffic
through another.

## Why `/shunt:usage` and not `/usage`

`/usage` is Claude Code's own built-in, and the engine refuses to let a plugin
take a built-in's name:

```
$.command.register: "/usage" refused: it is the built-in /usage
```

`$.command.register` takes a bare, global name. A plugin's **markdown** command
is namespaced by the plugin instead, so `commands/usage.md` in a plugin named
`shunt` lists as `shunt:usage` and collides with nothing. The hooks module then
intercepts `command.run` for that name and answers it directly.

## Layout

| Path | What it is |
| ---- | ---------- |
| `hooks/register.ts` | The one hook: `command.run` on `shunt:usage` |
| `hooks/endpoint.ts` | Resolves the gateway URL and credential header |
| `hooks/report.ts` | Reads a `GET /usage` body, defensively |
| `hooks/views.ts` | Renders the bars, percentages and reset times |
| `hooks/names.ts` | The command name and the fixed texts |
| `commands/usage.md` | The command, and the no-function-hooks fallback |
| `tests/` | Vitest suite over the pure modules |

## Development

```bash
# The static side-effect analysis: which events it hooks, which $ calls and
# environment variables it makes and reads.
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude plugin validate plugins/shunt

# The pure modules' suite (also a CI gate).
cd plugins/shunt && npm ci && npm test

# Run it from source against a live gateway.
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude --plugin-dir plugins/shunt
```

`hooks/register.ts` is the only file that imports `claude-code`. Typechecking it
needs the engine's own `claude-code.d.ts`, which Claude Code writes into a plugin
author's project via `/plugin-types`; the other modules import nothing from the
engine, which is why the suite covers them without it.

> Function hooks are early access. Hooks modules load only where they are
> enabled, and the API this mod is written against may change between Claude Code
> releases without notice.

## License

MIT OR Apache-2.0, matching the shunt project.
