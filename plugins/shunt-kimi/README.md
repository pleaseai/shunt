# shunt-kimi

Claude Code subagents that run on Moonshot AI's **kimi-k3** and
**kimi-k2.7-code**, routed through the [shunt](https://github.com/pleaseai/shunt)
gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)          | Model id (`model:`) |
| ---------------------------- | ------------------- |
| `shunt-kimi:kimi-k3`         | `kimi-k3[1m]`       |
| `shunt-kimi:kimi-k2.7-code`  | `kimi-k2.7-code`    |

Kimi is served over Moonshot's **Anthropic-compatible** endpoint, so shunt
forwards Claude Code's Messages request as-is and injects the Moonshot API key.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model id above to a Moonshot (`kimi`) provider.
`kimi` is **not** a built-in provider, so you add both the provider table and a
route:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide the Moonshot API key**:
   ```bash
   export MOONSHOT_API_KEY=…
   ```
3. **Add the provider and route** in your `shunt.toml`:
   ```toml
   [providers.kimi]
   kind = "anthropic"
   base_url = "https://api.moonshot.ai/anthropic"
   auth = "api_key"
   api_key_env = "MOONSHOT_API_KEY"

   [[routes]]
   model = "kimi-k3[1m]"
   provider = "kimi"

   [[routes]]
   model = "kimi-k2.7-code"
   provider = "kimi"
   ```

   For the full provider reference, see
   [`docs/running.md` §3](https://github.com/pleaseai/shunt/blob/main/docs/running.md)
   and [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

Without a running shunt gateway mapping this id, Claude Code will send
`kimi-k2.7-code` straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```

## Usage

```
@shunt-kimi:kimi-k3  refactor this module and run the tests
```

Or set every subagent to Kimi for a session with
`CLAUDE_CODE_SUBAGENT_MODEL="kimi-k3[1m]"`.

Both require a running shunt gateway with the slug routed to the `kimi` provider —
see [Prerequisites](#prerequisites). Without it the request fails against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
