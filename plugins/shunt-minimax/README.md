# shunt-minimax

A Claude Code subagent that runs on **MiniMax-M3** (1M-token context), routed
through the [shunt](https://github.com/pleaseai/shunt) gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)          | Model id (`model:`) |
| ---------------------------- | ------------------- |
| `shunt-minimax:minimax-m3`   | `MiniMax-M3[1m]`    |

MiniMax is served over its **Anthropic-compatible** endpoint, so shunt forwards
Claude Code's Messages request as-is and injects the MiniMax API key. The `[1m]`
suffix is Claude Code's 1M-token context marker — it is the exact `ANTHROPIC_MODEL`
value MiniMax's own [Claude Code integration](https://platform.minimax.io/docs/token-plan/claude-code)
documents, so route that literal slug.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model id above to a MiniMax (`minimax`) provider.
`minimax` is **not** a built-in provider, so you add both the provider table and a
route:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide the MiniMax API key**:
   ```bash
   export MINIMAX_API_KEY=…
   ```
3. **Add the provider and route** in your `shunt.toml`:
   ```toml
   [providers.minimax]
   kind = "anthropic"
   base_url = "https://api.minimax.io/anthropic"
   auth = "api_key"
   api_key_env = "MINIMAX_API_KEY"

   [[routes]]
   model = "MiniMax-M3[1m]"
   provider = "minimax"
   ```

   For the full provider reference, see
   [`docs/running.md` §3](https://github.com/pleaseai/shunt/blob/main/docs/running.md)
   and [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).
   The current MiniMax model slug is documented in MiniMax's
   [Claude Code integration guide](https://platform.minimax.io/docs/token-plan/claude-code).

Without a running shunt gateway mapping this id, Claude Code will send
`MiniMax-M3[1m]` straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-minimax@shunt
```

## Usage

```
@shunt-minimax:minimax-m3  refactor this module and run the tests
```

Or set every subagent to MiniMax for a session with
`CLAUDE_CODE_SUBAGENT_MODEL="MiniMax-M3[1m]"`.

Both require a running shunt gateway with the slug routed to the `minimax`
provider — see [Prerequisites](#prerequisites). Without it the request fails
against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
