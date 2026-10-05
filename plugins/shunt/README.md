# shunt

A **Claude Mod** — a Claude Code plugin whose behaviour is a
[function hooks](https://github.com/anthropics/claude-code/issues/91870) module —
that shows usage in a band above the prompt — the
[shunt](https://github.com/pleaseai/shunt) gateway's shared account pool on
shunt, the session's own Claude rate limits otherwise — and answers
`/shunt:usage` with the pool's full breakdown.

```
shunt · 5H 5% ↻1h 41m · WK 46% ↻1d 7h · Fable 31% ↻1d 7h ⚠ anthropic degraded
```

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

The mod reads the gateway's [`GET /usage`](https://shunt.sh/reference/endpoints/)
endpoint and prints it. It answers the command itself — the hook returns without
calling `next`, so nothing is sent to the model and the answer costs no tokens.

## The usage band

On a shunt gateway the band shows the pool, tagged `shunt ·`. Each figure is
how much of the window is **used** — `1 - remaining`, the same pool-wide mean
the command reports as headroom — followed by the time to that window's
earliest reset. Used rather than left, so it reads the same way as Claude
Code's own `/usage` and as the band reads off shunt. A figure turns amber from
70% and red from 90%. Each pooled provider whose status is not `ok` is flagged
at the end (`⚠ anthropic degraded`: amber for `degraded`, red for `exhausted`
or `capped`), or the pool itself when no provider is listed. A window no
account reports is left out.

Off shunt — no gateway set, a gateway that answers `GET /usage` with 404, a
successful response that is not a pool report, or one that pools no provider — the
band shows the session's own rate limits instead, untagged, as
`$.session.usage()` and `session.measure` report them:

```
5H 5% ↻1h 41m · WK 46% ↻1d 7h
```

On shunt it reads `GET /usage` when the session starts, every minute after,
and after each turn — the moment the pool has just been spent from — and
ignores the session's own limits, which come from whichever pool account
answered last. A refused token (401 or 403), an unreachable gateway, another
error status, or a `shunt gateway token` helper that fails is a fault on shunt,
so it is flagged rather than replaced: `shunt · ⚠ credential refused`, or
`shunt · ⚠ gateway login unavailable` for the helper. With neither a gateway nor rate limits (an
API key sent straight to Anthropic) the band draws nothing. Whatever another
plugin draws in the band stays on its left.

Collapse the band for the session with its `[-]` (ctrl+x ctrl+a), or turn it
off with the `usageBand` option below.

## Reading the numbers

`remaining` is the fraction of the pool's combined capacity still **usable**, so
`62%` means 62% of the headroom is left, not that 62% is spent. It is
`mean(clamp(cap - utilization, 0, 1))` over the non-disabled accounts that report
the window, where `cap` is the account's `max_utilization` hard cap for that
window (`100%` when none is set): nine exhausted accounts plus one fresh uncapped
one read `10%`, not `100%`, and an account at 44% under a 50% cap counts only `6%`.
An account a cap already excludes from the window's requests counts zero; a 5h or
7d cap excludes it from every request, Fable ones included.

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
   either `[server.auth]` (client tokens) or `[server.gateway]` (gateway login),
   next to the `[server.usage]` table in `shunt.toml`:

   ```toml
   [server.auth]

   # Presence alone opts in; the table takes no keys.
   [server.usage]
   ```

   With `[server.gateway]` instead of `[server.auth]`, see
   [`shunt gateway claude` sessions](#shunt-gateway-claude-sessions) below.
   `[server.auth]` takes its tokens from the environment rather than the TOML —
   by default `SHUNT_CLIENT_TOKENS`, as `name:token` pairs — and the gateway
   fails to start when it is unset. Export it where the gateway runs, using the
   same token you set as `ANTHROPIC_AUTH_TOKEN` above:

   ```bash
   export SHUNT_CLIENT_TOKENS="claude-code:<your client token>"
   ```

3. **Run Claude Code with function hooks enabled** — the feature is early access:

   ```bash
   CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
   ```

Without step 3 there is no band, and the command still exists but falls back to
`commands/usage.md`, which asks the model to read the endpoint with a tool call
instead.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt@shunt
```

## Configuration

One option, a row in `/config` (stored in settings.json under `pluginConfigs`):

| Option | Default | Purpose |
| ------ | ------- | ------- |
| `usageBand` | `true` | Show the band above the prompt. Off, the mod registers no band and polls nothing; `/shunt:usage` still answers |

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

### `shunt gateway claude` sessions

`shunt gateway claude` scrubs `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` from
the environment it hands Claude Code, and wires `apiKeyHelper` to `shunt gateway
token` instead; Claude Code consumes the helper's output itself and never
re-exports it. When none of the variables above holds a credential, the mod
therefore reads the merged settings (`--settings` included) and, if
`apiKeyHelper` is shunt's own — `shunt gateway token`, by bare name or by path,
as the launcher writes it — runs it directly, without a shell, and sends the
gateway login token it prints as `Authorization: Bearer`. Because there is no
shell, a leading `~/` in an unquoted helper path is expanded to the home
directory (`HOME`, else `USERPROFILE`) by the mod itself; a quoted `'~/…'` stays
literal, as in a shell. If that helper fails — a non-zero exit, no output, or it
cannot be started — the band flags `shunt · ⚠ gateway login unavailable` and
`/shunt:usage` says to run `shunt gateway login`, rather than silently showing
the session's own limits.

The token is reused for five minutes, so a poll every minute does not run the
helper every minute. A reused token the gateway refuses is dropped and the
helper asked once more, since a refresh may have rotated it. Any other
`apiKeyHelper` — a password manager, a script that prompts — is never run: the
mod would otherwise run it every few minutes for a usage figure. In such a
session export `SHUNT_TOKEN` with a `[server.auth]` client token instead, or the
band falls back to the session's own rate limits.

The gateway has to accept a gateway login on `GET /usage`, which needs a shunt
build that authenticates `[server.gateway]` logins there; `[server.usage]` with
only `[server.gateway]` configured is enough.

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
| `hooks/register.tsx` | The hooks: `command.run` on `shunt:usage`, and the band's `session.start`, `session.measure`, `turn.complete` and `ui.render` on `AbovePrompt` |
| `hooks/endpoint.ts` | Resolves the gateway URL and credential header |
| `hooks/report.ts` | Reads a `GET /usage` body, defensively |
| `hooks/reading.ts` | Turns one round trip into a report or a problem, full and brief |
| `hooks/views.ts` | Renders the command's bars, percentages and reset times |
| `hooks/band.ts` | The band's cells, alerts, levels and countdowns, from the pool or the session's own limits |
| `hooks/names.ts` | The command name and the fixed texts |
| `types/index.d.ts` | The `$.state` contract: the pool snapshot, the session's limits and the clock the band draws from |
| `commands/usage.md` | The command, and the no-function-hooks fallback |
| `tests/*.spec.ts` | Vitest suite over the pure modules |
| `tests/mod/*.test.tsx` | Engine suite over the hooks, run by `claude plugin test` |

## Development

```bash
# The static side-effect analysis: which events it hooks, which $ calls and
# environment variables it makes and reads.
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude plugin validate plugins/shunt

# The pure modules' suite and typecheck (both CI gates).
cd plugins/shunt && npm ci && npm run typecheck && npm test

# The hooks against the engine itself: the band and the command, with the
# gateway, clock and environment mocked. Needs a Claude Code binary, so CI
# does not run it.
claude plugin test plugins/shunt

# Run it from source against a live gateway.
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude --plugin-dir plugins/shunt
```

`hooks/register.tsx` is the only hooks file that imports `claude-code`. Typechecking it
needs the engine's own `claude-code.d.ts`, which Claude Code writes into a plugin
author's project via `/plugin-types`; the other modules import nothing from the
engine, which is why the suite covers them without it. For the same reason
`tsconfig.json` excludes `hooks/register.tsx` from `npm run typecheck`, along
with `tests/mod`, the engine suite — that exclusion is deliberate, not an oversight, and typechecking that one file means
running `/plugin-types` first.

> Function hooks are early access. Hooks modules load only where they are
> enabled, and the API this mod is written against may change between Claude Code
> releases without notice.

## License

MIT OR Apache-2.0, matching the shunt project.
