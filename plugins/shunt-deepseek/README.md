# shunt-deepseek

Claude Code subagents that run on **DeepSeek** models — **deepseek-v4-pro** and
**deepseek-v4-flash** — routed through the
[shunt](https://github.com/pleaseai/shunt) gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)               | Model id (`model:`)  | Notes                    |
| --------------------------------- | -------------------- | ------------------------ |
| `shunt-deepseek:deepseek-v4-pro`  | `deepseek-v4-pro`    | Frontier tier            |
| `shunt-deepseek:deepseek-v4-flash`| `deepseek-v4-flash`  | Fast, lighter tier       |

DeepSeek is served over its **Anthropic-compatible** endpoint, so shunt forwards
Claude Code's Messages request as-is and injects the DeepSeek API key.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model ids above to a DeepSeek (`deepseek`)
provider. `deepseek` is **not** a built-in provider, so you add both the provider
table and the routes:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide the DeepSeek API key**:
   ```bash
   export DEEPSEEK_API_KEY=…
   ```
3. **Add the provider and routes** in your `shunt.toml`:
   ```toml
   [providers.deepseek]
   kind = "anthropic"
   base_url = "https://api.deepseek.com/anthropic"
   auth = "api_key"
   api_key_env = "DEEPSEEK_API_KEY"

   [[routes]]
   model = "deepseek-v4-pro"
   provider = "deepseek"

   [[routes]]
   model = "deepseek-v4-flash"
   provider = "deepseek"
   ```

   For the full provider reference, see
   [`docs/running.md` §3](https://github.com/pleaseai/shunt/blob/main/docs/running.md)
   and [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

Without a running shunt gateway mapping these ids, Claude Code will send the
`deepseek-*` model id straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-deepseek@shunt
```

## Usage

```
@shunt-deepseek:deepseek-v4-pro  refactor this module and run the tests
```

Or set every subagent to DeepSeek for a session with
`CLAUDE_CODE_SUBAGENT_MODEL=deepseek-v4-pro`.

Both require a running shunt gateway with the slug routed to the `deepseek`
provider — see [Prerequisites](#prerequisites). Without it the request fails
against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
