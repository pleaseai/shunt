# shunt-zai

Claude Code subagents that run on **Z.ai GLM** models — **glm-5.2** and
**glm-4.7** — routed through the [shunt](https://github.com/pleaseai/shunt)
gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)   | Model id (`model:`) |
| --------------------- | ------------------- |
| `shunt-zai:glm-5.2`   | `glm-5.2`           |
| `shunt-zai:glm-4.7`   | `glm-4.7`           |

GLM is served over Z.ai's **Anthropic-compatible** endpoint, so shunt forwards
Claude Code's Messages request as-is and injects the Z.ai API key.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model ids above to a Z.ai (`zai`) provider. `zai`
is **not** a built-in provider, so you add both the provider table and the routes:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide the Z.ai API key**:
   ```bash
   export ZAI_API_KEY=…
   ```
3. **Add the provider and routes** in your `shunt.toml`:
   ```toml
   [providers.zai]
   kind = "anthropic"
   base_url = "https://api.z.ai/api/anthropic"
   auth = "api_key"
   api_key_env = "ZAI_API_KEY"

   [[routes]]
   model = "glm-5.2"
   provider = "zai"

   [[routes]]
   model = "glm-4.7"
   provider = "zai"
   ```

   For the full provider reference, see
   [`docs/running.md` §3](https://github.com/pleaseai/shunt/blob/main/docs/running.md)
   and [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

Without a running shunt gateway mapping these ids, Claude Code will send the
`glm-*` model id straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-zai@shunt
```

## Usage

```
@shunt-zai:glm-5.2  refactor this module and run the tests
```

Or set every subagent to GLM for a session with
`CLAUDE_CODE_SUBAGENT_MODEL=glm-5.2`.

Both require a running shunt gateway with the slug routed to the `zai` provider —
see [Prerequisites](#prerequisites). Without it the request fails against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
