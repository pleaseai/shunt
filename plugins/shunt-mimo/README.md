# shunt-mimo

A Claude Code subagent that runs on Xiaomi's **mimo-v2.5-pro**, routed through the
[shunt](https://github.com/pleaseai/shunt) gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)          | Model id (`model:`) |
| ---------------------------- | ------------------- |
| `shunt-mimo:mimo-v2.5-pro`   | `mimo-v2.5-pro`     |

Mimo is served over its **Anthropic-compatible** endpoint, so shunt forwards
Claude Code's Messages request as-is and injects the Mimo API key. Xiaomi also
exposes a **1M-context variant** — append the `[1m]` suffix (`mimo-v2.5-pro[1m]`)
in both the agent `model:` and the route to enable extended context.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model id above to a Mimo (`mimo`) provider. `mimo`
is **not** a built-in provider, so you add both the provider table and a route:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide the Mimo API key**:
   ```bash
   export MIMO_API_KEY=…
   ```
3. **Add the provider and route** in your `shunt.toml`:
   ```toml
   [providers.mimo]
   kind = "anthropic"
   base_url = "https://api.xiaomimimo.com/anthropic"
   auth = "api_key"
   api_key_env = "MIMO_API_KEY"

   [[routes]]
   model = "mimo-v2.5-pro"
   provider = "mimo"
   ```

   For the full provider reference, see
   [`docs/running.md` §3](https://github.com/pleaseai/shunt/blob/main/docs/running.md)
   and [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

   > **base_url note.** `https://api.xiaomimimo.com/anthropic` is the
   > pay-as-you-go host. If you're on a **Token Plan**, use
   > `https://token-plan-cn.xiaomimimo.com/anthropic` instead — set `base_url` to
   > whichever your account requires. Hosts are from Xiaomi's current
   > [Claude Code integration docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode).

Without a running shunt gateway mapping this id, Claude Code will send
`mimo-v2.5-pro` straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-mimo@shunt
```

## Usage

```
@shunt-mimo:mimo-v2.5-pro  refactor this module and run the tests
```

Or set every subagent to Mimo for a session with
`CLAUDE_CODE_SUBAGENT_MODEL=mimo-v2.5-pro`.

Both require a running shunt gateway with the slug routed to the `mimo` provider —
see [Prerequisites](#prerequisites). Without it the request fails against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
